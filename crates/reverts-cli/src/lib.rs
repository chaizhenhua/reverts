mod commands;
mod errors;
mod help;

pub use commands::generate_project::GenerateProjectV2Args;
pub use errors::{
    CliError, CliRunError, ExtractAssetsError, MatchPackagesError, RuntimeInventoryError,
};
pub use help::{HelpTopic, help_text, version_text};

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use reverts_analyze::enrich_program;
use reverts_graph::FunctionExtractor;
use reverts_input::sqlite::{load_project_bundle_from_sqlite, load_project_rows_from_connection};
use reverts_input::{
    AssetKind, InputBundle, InputRows, ModuleDependencyTarget, ModuleInput,
    PACKAGE_ATTRIBUTION_EXTERNAL_IMPORT_POLICY_VERSION, PackageAttributionInput,
    PackageAttributionStatus, PackageEmissionMode, SourceFileInput,
};
use reverts_ir::hash::fnv1a_hex as stable_hash;
use reverts_ir::{BindingName, ModuleId, ModuleKind, is_valid_package_name, split_bare_specifier};
use reverts_js::{
    ParseGoal, TopLevelStatementKind, collect_top_level_statement_facts,
    normalize_source_for_pipeline,
};
use reverts_model::ProgramModel;
use reverts_observe::{AuditFinding, AuditReport, FindingCode};
use reverts_package::{external_import_proof_label, is_node_builtin};
use reverts_package_matcher::{
    BestVersionMatch, ModuleMatchStrategy, PackageMatch, PackageModuleSourceQuality, PackageSource,
    VersionMatchScore, VersionedPackageMatchReport, clean_package_semantic_path_hint,
    has_accepted_external_attribution, is_exact_package_version_hint, is_json_source_path,
    match_packages_with_pipeline, normalize_hint_text, ownership_by_module,
    package_import_names_from_sources, package_module_source_quality, package_source_entry_path,
    package_source_exported_members, package_source_normalized_hash,
    package_source_public_export_proofs, package_source_semantic_surface_hint_score,
    strip_source_extension,
};
use reverts_pipeline::{
    AssetReference, EmittedFile, RuntimeSetterMigrationBindingKey,
    RuntimeSetterMigrationBindingStatus, RuntimeSetterMigrationBlockerReason,
    RuntimeSetterMigrationBlockerReport, collect_required_asset_references_from_rows,
    generate_project_from_prepared, prepare_and_enrich, prepare_input_rows_for_pipeline,
    runtime_setter_migration_blocker_report_from_prepared,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params, params_from_iter};
use semver::{BuildMetadata, Comparator, Op, Version, VersionReq};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchPackagesArgs {
    pub input: PathBuf,
    pub project_id: u32,
    pub apply: bool,
    pub package_names: Vec<String>,
    pub package_source_roots: Vec<PathBuf>,
    pub materialize_package_sources: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchPackagesReportArgs {
    pub input: PathBuf,
    pub all_projects: bool,
    pub limit: Option<u32>,
    pub newest: bool,
    pub package_names: Vec<String>,
    pub package_source_roots: Vec<PathBuf>,
    pub materialize_package_sources: bool,
}

impl MatchPackagesReportArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let mut input = None;
        let mut all_projects = false;
        let mut limit = None;
        let mut newest = false;
        let mut package_names = Vec::new();
        let mut package_source_roots = Vec::new();
        let mut materialize_package_sources = false;
        let mut args = args.into_iter().collect::<Vec<_>>();
        if args
            .first()
            .is_some_and(|argument| argument == help::MATCH_PACKAGES_REPORT_COMMAND)
        {
            args.remove(0);
        }
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--input" => input = Some(next_path(&mut args, "--input")?),
                "--all-projects" => all_projects = true,
                "--limit" => limit = Some(parse_limit(next_value(&mut args, "--limit")?)?),
                "--newest" => newest = true,
                "--package-name" => {
                    let package_name = next_value(&mut args, "--package-name")?;
                    if package_name.trim().is_empty() {
                        return Err(CliError::InvalidPackageName(package_name));
                    }
                    package_names.push(package_name);
                }
                "--package-source-root" => {
                    package_source_roots.push(next_path(&mut args, "--package-source-root")?);
                }
                "--materialize-package-sources" => materialize_package_sources = true,
                other => return Err(CliError::UnknownArgument(other.to_string())),
            }
        }

        if !all_projects {
            return Err(CliError::MissingArgument("--all-projects"));
        }

        Ok(Self {
            input: input.ok_or(CliError::MissingArgument("--input"))?,
            all_projects,
            limit,
            newest,
            package_names,
            package_source_roots,
            materialize_package_sources,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageCacheArgs {
    pub input: PathBuf,
    pub apply: bool,
}

impl PackageCacheArgs {
    pub fn parse(
        args: impl IntoIterator<Item = String>,
        command: &'static str,
    ) -> Result<Self, CliError> {
        let mut input = None;
        let mut apply = false;
        let mut args = args.into_iter().collect::<Vec<_>>();
        if args.first().is_some_and(|argument| argument == command) {
            args.remove(0);
        }
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--input" => input = Some(next_path(&mut args, "--input")?),
                "--apply" => apply = true,
                other => return Err(CliError::UnknownArgument(other.to_string())),
            }
        }

        Ok(Self {
            input: input.ok_or(CliError::MissingArgument("--input"))?,
            apply,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageExternalizationHintsArgs {
    pub input: PathBuf,
    pub package_names: Vec<String>,
    pub limit: Option<u32>,
    pub apply: bool,
}

impl PackageExternalizationHintsArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let mut input = None;
        let mut package_names = Vec::new();
        let mut limit = None;
        let mut apply = false;
        let mut args = args.into_iter().collect::<Vec<_>>();
        if args
            .first()
            .is_some_and(|argument| argument == help::PACKAGE_EXTERNALIZATION_HINTS_COMMAND)
        {
            args.remove(0);
        }
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--input" => input = Some(next_path(&mut args, "--input")?),
                "--package-name" => {
                    let package_name = next_value(&mut args, "--package-name")?;
                    if package_name.trim().is_empty() {
                        return Err(CliError::InvalidPackageName(package_name));
                    }
                    package_names.push(package_name);
                }
                "--limit" => limit = Some(parse_limit(next_value(&mut args, "--limit")?)?),
                "--apply" => apply = true,
                other => return Err(CliError::UnknownArgument(other.to_string())),
            }
        }

        Ok(Self {
            input: input.ok_or(CliError::MissingArgument("--input"))?,
            package_names,
            limit,
            apply,
        })
    }
}

impl MatchPackagesArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let mut input = None;
        let mut project_id = None;
        let mut apply = false;
        let mut package_names = Vec::new();
        let mut package_source_roots = Vec::new();
        let mut materialize_package_sources = false;
        let mut args = args.into_iter().collect::<Vec<_>>();
        if args
            .first()
            .is_some_and(|argument| argument == help::MATCH_PACKAGES_COMMAND)
        {
            args.remove(0);
        }
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--input" => input = Some(next_path(&mut args, "--input")?),
                "--project-id" => {
                    project_id = Some(parse_project_id(next_value(&mut args, "--project-id")?)?);
                }
                "--apply" => apply = true,
                "--package-name" => {
                    let package_name = next_value(&mut args, "--package-name")?;
                    if package_name.trim().is_empty() {
                        return Err(CliError::InvalidPackageName(package_name));
                    }
                    package_names.push(package_name);
                }
                "--package-source-root" => {
                    package_source_roots.push(next_path(&mut args, "--package-source-root")?);
                }
                "--materialize-package-sources" => materialize_package_sources = true,
                other => return Err(CliError::UnknownArgument(other.to_string())),
            }
        }

        Ok(Self {
            input: input.ok_or(CliError::MissingArgument("--input"))?,
            project_id: project_id.ok_or(CliError::MissingArgument("--project-id"))?,
            apply,
            package_names,
            package_source_roots,
            materialize_package_sources,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractAssetsArgs {
    pub input: PathBuf,
    pub project_id: u32,
    pub apply: bool,
    pub asset_roots: Vec<PathBuf>,
}

impl ExtractAssetsArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let mut input = None;
        let mut project_id = None;
        let mut apply = false;
        let mut asset_roots = Vec::new();
        let mut args = args.into_iter().collect::<Vec<_>>();
        if args
            .first()
            .is_some_and(|argument| argument == help::EXTRACT_ASSETS_COMMAND)
        {
            args.remove(0);
        }
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--input" => input = Some(next_path(&mut args, "--input")?),
                "--project-id" => {
                    project_id = Some(parse_project_id(next_value(&mut args, "--project-id")?)?);
                }
                "--asset-root" => asset_roots.push(next_path(&mut args, "--asset-root")?),
                "--apply" => apply = true,
                other => return Err(CliError::UnknownArgument(other.to_string())),
            }
        }

        Ok(Self {
            input: input.ok_or(CliError::MissingArgument("--input"))?,
            project_id: project_id.ok_or(CliError::MissingArgument("--project-id"))?,
            apply,
            asset_roots,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeInventoryArgs {
    pub input: PathBuf,
    pub project_id: Option<u32>,
    pub all_projects: bool,
    pub limit: Option<u32>,
    pub newest: bool,
    pub max_source_bytes: Option<u64>,
    pub setter_blockers: bool,
    pub runtime_attribution: bool,
}

impl RuntimeInventoryArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let mut input = None;
        let mut project_id = None;
        let mut all_projects = false;
        let mut limit = None;
        let mut newest = false;
        let mut max_source_bytes = None;
        let mut setter_blockers = false;
        let mut runtime_attribution = false;
        let mut args = args.into_iter().collect::<Vec<_>>();
        if args
            .first()
            .is_some_and(|argument| argument == help::RUNTIME_INVENTORY_COMMAND)
        {
            args.remove(0);
        }
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--input" => input = Some(next_path(&mut args, "--input")?),
                "--project-id" => {
                    project_id = Some(parse_project_id(next_value(&mut args, "--project-id")?)?);
                }
                "--all-projects" => all_projects = true,
                "--limit" => limit = Some(parse_limit(next_value(&mut args, "--limit")?)?),
                "--newest" => newest = true,
                "--setter-blockers" => setter_blockers = true,
                "--runtime-attribution" => runtime_attribution = true,
                "--max-source-bytes" => {
                    max_source_bytes = Some(parse_byte_limit(next_value(
                        &mut args,
                        "--max-source-bytes",
                    )?)?);
                }
                other => return Err(CliError::UnknownArgument(other.to_string())),
            }
        }

        match (project_id, all_projects) {
            (Some(_), true) => Err(CliError::UnknownArgument("--all-projects".to_string())),
            (None, false) => Err(CliError::MissingArgument("--project-id")),
            _ => Ok(Self {
                input: input.ok_or(CliError::MissingArgument("--input"))?,
                project_id,
                all_projects,
                limit,
                newest,
                max_source_bytes,
                setter_blockers,
                runtime_attribution,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliCommand {
    Help(HelpTopic),
    Version,
    GenerateProjectV2(GenerateProjectV2Args),
    MatchPackages(MatchPackagesArgs),
    MatchPackagesReport(MatchPackagesReportArgs),
    PackageCacheAudit(PackageCacheArgs),
    PackageCachePruneStale(PackageCacheArgs),
    PackageExternalizationHints(PackageExternalizationHintsArgs),
    ExtractAssets(ExtractAssetsArgs),
    RuntimeInventory(RuntimeInventoryArgs),
}

impl CliCommand {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let args = args.into_iter().collect::<Vec<_>>();
        match args.first().map(String::as_str) {
            Some(argument) if is_help_flag(argument) => parse_top_level_help(args.as_slice()),
            Some(argument) if is_version_flag(argument) => parse_version(args.as_slice()),
            Some("help") => parse_help_command(args.as_slice()),
            Some("version") => parse_version(args.as_slice()),
            Some(command) => {
                if let Some(topic) = help::command_topic(command) {
                    if is_command_help(args.as_slice()) {
                        return Ok(Self::Help(topic));
                    }
                    match topic {
                        HelpTopic::GenerateProjectV2 => {
                            Ok(Self::GenerateProjectV2(GenerateProjectV2Args::parse(args)?))
                        }
                        HelpTopic::MatchPackages => {
                            Ok(Self::MatchPackages(MatchPackagesArgs::parse(args)?))
                        }
                        HelpTopic::MatchPackagesReport => Ok(Self::MatchPackagesReport(
                            MatchPackagesReportArgs::parse(args)?,
                        )),
                        HelpTopic::PackageCacheAudit => Ok(Self::PackageCacheAudit(
                            PackageCacheArgs::parse(args, help::PACKAGE_CACHE_AUDIT_COMMAND)?,
                        )),
                        HelpTopic::PackageCachePruneStale => Ok(Self::PackageCachePruneStale(
                            PackageCacheArgs::parse(args, help::PACKAGE_CACHE_PRUNE_STALE_COMMAND)?,
                        )),
                        HelpTopic::PackageExternalizationHints => {
                            Ok(Self::PackageExternalizationHints(
                                PackageExternalizationHintsArgs::parse(args)?,
                            ))
                        }
                        HelpTopic::ExtractAssets => {
                            Ok(Self::ExtractAssets(ExtractAssetsArgs::parse(args)?))
                        }
                        HelpTopic::RuntimeInventory => {
                            Ok(Self::RuntimeInventory(RuntimeInventoryArgs::parse(args)?))
                        }
                        HelpTopic::TopLevel => Ok(Self::Help(HelpTopic::TopLevel)),
                    }
                } else if command.starts_with("--") {
                    Ok(Self::GenerateProjectV2(GenerateProjectV2Args::parse(args)?))
                } else {
                    Err(CliError::UnknownCommand(command.to_string()))
                }
            }
            None => Ok(Self::Help(HelpTopic::TopLevel)),
        }
    }
}

fn parse_top_level_help(args: &[String]) -> Result<CliCommand, CliError> {
    match args {
        [_] => Ok(CliCommand::Help(HelpTopic::TopLevel)),
        [_, command] => parse_named_help_topic(command.as_str()).map(CliCommand::Help),
        [_, extra, ..] => Err(CliError::UnknownArgument(extra.clone())),
        [] => Ok(CliCommand::Help(HelpTopic::TopLevel)),
    }
}

fn parse_help_command(args: &[String]) -> Result<CliCommand, CliError> {
    match args {
        [_] => Ok(CliCommand::Help(HelpTopic::TopLevel)),
        [_, command] => parse_named_help_topic(command.as_str()).map(CliCommand::Help),
        [_, _, extra, ..] => Err(CliError::UnknownArgument(extra.clone())),
        [] => Ok(CliCommand::Help(HelpTopic::TopLevel)),
    }
}

fn parse_version(args: &[String]) -> Result<CliCommand, CliError> {
    match args {
        [_] => Ok(CliCommand::Version),
        [_, extra, ..] => Err(CliError::UnknownArgument(extra.clone())),
        [] => Ok(CliCommand::Version),
    }
}

fn parse_named_help_topic(command: &str) -> Result<HelpTopic, CliError> {
    help::command_topic(command).ok_or_else(|| CliError::UnknownCommand(command.to_string()))
}

fn is_command_help(args: &[String]) -> bool {
    matches!(args, [_, argument] if is_help_flag(argument) || argument == "help")
}

fn is_help_flag(argument: &str) -> bool {
    matches!(argument, "--help" | "-h")
}

fn is_version_flag(argument: &str) -> bool {
    matches!(argument, "--version" | "-V")
}

pub(crate) fn next_path(
    args: &mut impl Iterator<Item = String>,
    flag: &'static str,
) -> Result<PathBuf, CliError> {
    args.next()
        .map(PathBuf::from)
        .ok_or(CliError::MissingArgument(flag))
}

pub(crate) fn next_value(
    args: &mut impl Iterator<Item = String>,
    flag: &'static str,
) -> Result<String, CliError> {
    args.next().ok_or(CliError::MissingArgument(flag))
}

pub(crate) fn parse_project_id(value: String) -> Result<u32, CliError> {
    let parsed = value
        .parse::<u32>()
        .map_err(|_error| CliError::InvalidProjectId(value.clone()))?;
    if parsed == 0 {
        Err(CliError::InvalidProjectId(value))
    } else {
        Ok(parsed)
    }
}

pub(crate) fn parse_limit(value: String) -> Result<u32, CliError> {
    let parsed = value
        .parse::<u32>()
        .map_err(|_error| CliError::InvalidLimit(value.clone()))?;
    if parsed == 0 {
        Err(CliError::InvalidLimit(value))
    } else {
        Ok(parsed)
    }
}

pub(crate) fn parse_byte_limit(value: String) -> Result<u64, CliError> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_error| CliError::InvalidByteLimit(value.clone()))?;
    if parsed == 0 {
        Err(CliError::InvalidByteLimit(value))
    } else {
        Ok(parsed)
    }
}

pub fn run(args: impl IntoIterator<Item = String>) -> Result<(), CliRunError> {
    match CliCommand::parse(args).map_err(CliRunError::Args)? {
        CliCommand::Help(topic) => {
            println!("{}", help_text(topic));
            Ok(())
        }
        CliCommand::Version => {
            println!("{}", version_text());
            Ok(())
        }
        CliCommand::GenerateProjectV2(args) => commands::generate_project::run(args),
        CliCommand::MatchPackages(args) => run_match_packages(args),
        CliCommand::MatchPackagesReport(args) => run_match_packages_report(args),
        CliCommand::PackageCacheAudit(args) => run_package_cache_audit(args),
        CliCommand::PackageCachePruneStale(args) => run_package_cache_prune_stale(args),
        CliCommand::PackageExternalizationHints(args) => run_package_externalization_hints(args),
        CliCommand::ExtractAssets(args) => run_extract_assets(args),
        CliCommand::RuntimeInventory(args) => run_runtime_inventory(args),
    }
}

fn run_match_packages(args: MatchPackagesArgs) -> Result<(), CliRunError> {
    let outcome = match_packages_from_sqlite(&args).map_err(CliRunError::MatchPackages)?;
    println!(
        "matched packages for project {} from {} package source(s): {} module attribution(s), {} direct external import module attribution(s), {} private source-suppressed package module(s), {} package source eliminated ({:.2}%), {} package source remaining, {} external import candidate(s), {} unsafe external import candidate(s) removed, {} package surface(s), {} attribution(s) written, {} surface(s) written, {} function attribution(s) ({} written), {} function ownership match(es), {} trusted / {} weak / {} invalid / {} missing package module source slice(s), {} audit finding(s)",
        outcome.project_id,
        outcome.loaded_package_sources,
        outcome.matched_modules,
        outcome.external_import_modules,
        outcome.private_source_suppressed_package_modules,
        outcome.source_eliminated_package_modules,
        pct(
            outcome.source_eliminated_package_modules,
            outcome.loaded_package_modules
        ),
        outcome.remaining_package_source_modules,
        outcome.external_import_candidates,
        outcome.unsafe_external_import_modules,
        outcome.matched_package_surfaces,
        outcome.written_attributions,
        outcome.written_surfaces,
        outcome.function_attributions,
        outcome.written_function_attributions,
        outcome.function_ownership_matches,
        outcome.package_source_quality_trusted,
        outcome.package_source_quality_weak,
        outcome.package_source_quality_invalid,
        outcome.package_source_quality_missing,
        outcome.audit.findings().len()
    );
    if !outcome.audit.is_clean() {
        println!("{}", format_audit_findings(&outcome.audit));
    }
    print_external_import_blockers(&outcome.external_import_blockers);
    Ok(())
}

fn print_external_import_blockers(blockers: &[ExternalImportBlockerSummary]) {
    if blockers.is_empty() {
        return;
    }
    println!("external import blockers:");
    for blocker in blockers.iter().take(12) {
        println!(
            "  {}: {} ({})",
            blocker.reason, blocker.consumer, blocker.count
        );
    }
}

fn run_match_packages_report(args: MatchPackagesReportArgs) -> Result<(), CliRunError> {
    let outcome = match_packages_report_from_sqlite(&args).map_err(CliRunError::MatchPackages)?;
    println!(
        "package match report: projects={}, package_modules={}, matched={} ({:.2}%), direct_externalized={} ({:.2}% of package modules), private_source_suppressed={}, source_eliminated={} ({:.2}% of package modules), source_remaining={}, candidates={}, unsafe_removed={}, surfaces={}, audit_findings={}",
        outcome.projects.len(),
        outcome.totals.package_modules,
        outcome.totals.matched_modules,
        pct(
            outcome.totals.matched_modules,
            outcome.totals.package_modules
        ),
        outcome.totals.external_import_modules,
        pct(
            outcome.totals.external_import_modules,
            outcome.totals.package_modules
        ),
        outcome.totals.private_source_suppressed_package_modules,
        outcome.totals.source_eliminated_package_modules,
        pct(
            outcome.totals.source_eliminated_package_modules,
            outcome.totals.package_modules
        ),
        outcome.totals.remaining_package_source_modules,
        outcome.totals.external_import_candidates,
        outcome.totals.unsafe_external_import_modules,
        outcome.totals.package_surfaces,
        outcome.totals.audit_findings,
    );
    for project in &outcome.projects {
        println!(
            "  project {}: package_modules={}, matched={} ({:.2}%), direct_externalized={} ({:.2}% of package modules), private_source_suppressed={}, source_eliminated={} ({:.2}% of package modules), source_remaining={}, candidates={}, unsafe_removed={}, surfaces={}, audit_findings={}",
            project.project_id,
            project.loaded_package_modules,
            project.matched_modules,
            pct(project.matched_modules, project.loaded_package_modules),
            project.external_import_modules,
            pct(
                project.external_import_modules,
                project.loaded_package_modules
            ),
            project.private_source_suppressed_package_modules,
            project.source_eliminated_package_modules,
            pct(
                project.source_eliminated_package_modules,
                project.loaded_package_modules
            ),
            project.remaining_package_source_modules,
            project.external_import_candidates,
            project.unsafe_external_import_modules,
            project.matched_package_surfaces,
            project.audit.findings().len(),
        );
    }
    print_external_import_blockers(&outcome.blockers);
    Ok(())
}

fn run_package_cache_audit(args: PackageCacheArgs) -> Result<(), CliRunError> {
    let outcome =
        package_cache_audit_from_sqlite(&args, false).map_err(CliRunError::MatchPackages)?;
    print_package_cache_audit(&outcome, false);
    Ok(())
}

fn run_package_cache_prune_stale(args: PackageCacheArgs) -> Result<(), CliRunError> {
    let outcome =
        package_cache_audit_from_sqlite(&args, true).map_err(CliRunError::MatchPackages)?;
    print_package_cache_audit(&outcome, args.apply);
    Ok(())
}

fn run_package_externalization_hints(
    args: PackageExternalizationHintsArgs,
) -> Result<(), CliRunError> {
    let outcome =
        package_externalization_hints_from_sqlite(&args).map_err(CliRunError::MatchPackages)?;
    println!(
        "package externalization hints: scanned={}, verified={}, skipped_invalid_specifier={}, skipped_invalid_versions={}, skipped_hash_mismatch={}, skipped_normalize_errors={}, {}={}",
        outcome.scanned_rows,
        outcome.verified_rows,
        outcome.invalid_export_specifier_rows,
        outcome.invalid_version_rows,
        outcome.content_hash_mismatch_rows,
        outcome.normalize_error_rows,
        if args.apply { "written" } else { "would_write" },
        outcome.written_rows,
    );
    Ok(())
}

fn print_package_cache_audit(outcome: &PackageCacheAuditOutcome, applied: bool) {
    println!(
        "package source cache audit: rows={}, missing_identity={}, invalid_versions={}, stale_policy={}, missing_policy={}, missing_export_specifier={}, parse_errors={}, {}={}",
        outcome.rows,
        outcome.missing_identity_rows,
        outcome.invalid_version_rows,
        outcome.stale_policy_rows,
        outcome.missing_import_policy_rows,
        outcome.missing_export_specifier_rows,
        outcome.parse_error_rows,
        if applied { "deleted" } else { "would_delete" },
        outcome.deleted_rows,
    );
}

fn pct(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        (numerator as f64 * 100.0) / denominator as f64
    }
}

fn run_extract_assets(args: ExtractAssetsArgs) -> Result<(), CliRunError> {
    let outcome = extract_assets_from_sqlite(&args).map_err(CliRunError::ExtractAssets)?;
    println!(
        "extracted assets for project {}: {} reference(s), {} matched, {} missing, {} written",
        outcome.project_id,
        outcome.referenced_assets,
        outcome.matched_assets,
        outcome.missing_assets,
        outcome.written_assets
    );
    Ok(())
}

fn run_runtime_inventory(args: RuntimeInventoryArgs) -> Result<(), CliRunError> {
    let outcome = runtime_inventory_from_sqlite(&args).map_err(CliRunError::RuntimeInventory)?;
    println!(
        "runtime inventory for {} project(s), {} skipped",
        outcome.projects.len(),
        outcome.skipped_projects
    );
    for project in &outcome.projects {
        println!(
            "project {}: source_bytes={}, skipped={}, files={}, source_lines={}, runtime_files={}, runtime_lines={}, runtime_reexport_only_files={}, runtime_imports={}, runtime_reexports={}, setters={}, setter_imports={}, setter_functions={}, reverts_internal_names={}, named_imports={}, named_exports={}, audit_findings={}",
            project.project_id,
            project.source_bytes,
            project.skipped,
            project.counts.files,
            project.counts.source_lines,
            project.counts.runtime_files,
            project.counts.runtime_lines,
            project.counts.runtime_reexport_only_files,
            project.counts.runtime_import_statements,
            project.counts.runtime_reexport_statements,
            project.counts.setter_occurrences,
            project.counts.setter_import_statements,
            project.counts.setter_function_definitions,
            project.counts.reverts_internal_occurrences,
            project.counts.named_import_statements,
            project.counts.named_export_statements,
            project.audit_findings,
        );
        if project.audit_findings > 0 {
            let top: Vec<String> = project
                .audit_finding_codes
                .iter()
                .take(8)
                .map(|(code, count)| format!("{code}={count}"))
                .collect();
            println!(
                "  audit: errors={}, warnings={}, top=[{}]",
                project.audit_errors,
                project.audit_warnings,
                top.join(", ")
            );
        }
        if let Some(report) = &project.setter_blockers {
            print_runtime_setter_blocker_report("  setter_blockers", report);
        }
        if let Some(report) = &project.emitted_setter_blockers {
            print_runtime_setter_blocker_report("  emitted_setter_blockers", report);
        }
        if let Some(report) = &project.runtime_attribution {
            print_runtime_line_attribution_report("  runtime_line_attribution", report);
        }
    }
    println!(
        "total: files={}, source_lines={}, runtime_files={}, runtime_lines={}, runtime_reexport_only_files={}, runtime_imports={}, runtime_reexports={}, setters={}, setter_imports={}, setter_functions={}, reverts_internal_names={}, named_imports={}, named_exports={}, audit_findings={}, skipped_source_bytes={}",
        outcome.totals.files,
        outcome.totals.source_lines,
        outcome.totals.runtime_files,
        outcome.totals.runtime_lines,
        outcome.totals.runtime_reexport_only_files,
        outcome.totals.runtime_import_statements,
        outcome.totals.runtime_reexport_statements,
        outcome.totals.setter_occurrences,
        outcome.totals.setter_import_statements,
        outcome.totals.setter_function_definitions,
        outcome.totals.reverts_internal_occurrences,
        outcome.totals.named_import_statements,
        outcome.totals.named_export_statements,
        outcome.audit_findings,
        outcome.skipped_source_bytes,
    );
    if let Some(report) = &outcome.setter_blockers {
        print_runtime_setter_blocker_report("total setter_blockers", report);
    }
    if let Some(report) = &outcome.emitted_setter_blockers {
        print_runtime_setter_blocker_report("total emitted_setter_blockers", report);
    }
    if let Some(report) = &outcome.runtime_attribution {
        print_runtime_line_attribution_report("total runtime_line_attribution", report);
    }
    Ok(())
}

fn print_runtime_setter_blocker_report(label: &str, report: &RuntimeSetterMigrationBlockerReport) {
    println!(
        "{label}: total={}, accepted={}, blocked={}",
        report.total_bindings, report.accepted_bindings, report.blocked_bindings
    );
    let mut reasons = report
        .reasons
        .iter()
        .map(|(reason, count)| (*reason, *count))
        .collect::<Vec<_>>();
    reasons.sort_by(|(left_reason, left_count), (right_reason, right_count)| {
        right_count
            .cmp(left_count)
            .then_with(|| left_reason.as_str().cmp(right_reason.as_str()))
    });
    for (reason, count) in reasons {
        println!("  {}={}", reason.as_str(), count);
        let mut sub_reasons = report
            .sub_reasons
            .iter()
            .filter_map(|((r, label), count)| (*r == reason).then_some((*label, *count)))
            .collect::<Vec<_>>();
        sub_reasons.sort_by(|(left_label, left_count), (right_label, right_count)| {
            right_count
                .cmp(left_count)
                .then_with(|| left_label.cmp(right_label))
        });
        for (label, sub_count) in sub_reasons {
            println!("    {label}={sub_count}");
        }
        if std::env::var_os("REVERTS_RUNTIME_BLOCKER_EXAMPLES").is_some() {
            let examples = report
                .binding_statuses
                .iter()
                .filter_map(|(key, status)| match status {
                    RuntimeSetterMigrationBindingStatus::Blocked(blocked_reason)
                        if *blocked_reason == reason =>
                    {
                        Some(format!("{}:{}", key.source_file_id, key.binding.as_str()))
                    }
                    _ => None,
                })
                .take(10)
                .collect::<Vec<_>>();
            if !examples.is_empty() {
                println!("    examples: {}", examples.join(", "));
            }
        }
    }
}

fn print_runtime_line_attribution_report(label: &str, report: &RuntimeLineAttributionReport) {
    println!(
        "{label}: runtime_lines={}, attributed_lines={}, unattributed_lines={}, items={}",
        report.total_runtime_lines,
        report.attributed_lines,
        report.unattributed_lines,
        report.items.len()
    );
    let mut buckets = report
        .by_kind
        .iter()
        .map(|(kind, bucket)| (kind.as_str(), *bucket))
        .collect::<Vec<_>>();
    buckets.sort_by(|(left_kind, left_bucket), (right_kind, right_bucket)| {
        right_bucket
            .lines
            .cmp(&left_bucket.lines)
            .then_with(|| left_kind.cmp(right_kind))
    });
    for (kind, bucket) in buckets {
        println!(
            "  kind {kind}: lines={}, items={}",
            bucket.lines, bucket.items
        );
    }
    let mut package_buckets = report
        .by_package
        .iter()
        .map(|(package, bucket)| (package.as_str(), *bucket))
        .collect::<Vec<_>>();
    package_buckets.sort_by(
        |(left_package, left_bucket), (right_package, right_bucket)| {
            right_bucket
                .lines
                .cmp(&left_bucket.lines)
                .then_with(|| left_package.cmp(right_package))
        },
    );
    for (package, bucket) in package_buckets.iter().take(20) {
        let runtime_pct = if report.total_runtime_lines == 0 {
            0.0
        } else {
            (bucket.lines as f64 * 100.0) / report.total_runtime_lines as f64
        };
        println!(
            "  package {package}: lines={}, pct={runtime_pct:.2}%, items={}",
            bucket.lines, bucket.items
        );
    }
    let mut items = report.items.clone();
    items.sort_by(|left, right| {
        right
            .lines
            .cmp(&left.lines)
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.binding.cmp(&right.binding))
    });
    for item in items.iter().take(20) {
        println!(
            "  top {} {} package={} {}:{}-{} lines={}",
            item.kind,
            item.binding,
            item.package,
            item.path,
            item.line_start,
            item.line_end,
            item.lines
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchPackagesOutcome {
    pub project_id: u32,
    pub loaded_package_modules: usize,
    pub loaded_package_sources: usize,
    pub matched_modules: usize,
    /// Accepted direct package-import modules in the loaded project rows,
    /// including existing persisted external attributions and fresh matches.
    pub external_import_modules: usize,
    pub private_source_suppressed_package_modules: usize,
    /// Package modules whose source can be eliminated because they are either
    /// directly emitted as an external import or are private package modules
    /// reachable only through such externalized public modules.
    pub source_eliminated_package_modules: usize,
    pub remaining_package_source_modules: usize,
    pub external_import_candidates: usize,
    pub unsafe_external_import_modules: usize,
    pub matched_package_surfaces: usize,
    pub written_attributions: usize,
    pub written_surfaces: usize,
    /// Number of function-level attribution rows produced while matching
    /// (computed regardless of whether persistence ran).
    pub function_attributions: usize,
    /// Number of accepted function-level package ownership matches, including
    /// source-only evidence that is not safe to emit as an external import.
    pub function_ownership_matches: usize,
    /// Number of function-level match rows actually written to
    /// `package_function_attributions`. Zero when `apply=false`.
    pub written_function_attributions: usize,
    pub package_source_quality_trusted: usize,
    pub package_source_quality_weak: usize,
    pub package_source_quality_invalid: usize,
    pub package_source_quality_missing: usize,
    pub audit: AuditReport,
    pub external_import_blockers: Vec<ExternalImportBlockerSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExternalImportSafetyReport {
    pub removed_modules: usize,
    pub blockers: Vec<ExternalImportBlockerSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalImportBlockerSummary {
    pub reason: String,
    pub consumer: String,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MatchPackagesReportOutcome {
    pub projects: Vec<MatchPackagesOutcome>,
    pub totals: MatchPackagesReportTotals,
    pub blockers: Vec<ExternalImportBlockerSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MatchPackagesReportTotals {
    pub package_modules: usize,
    pub matched_modules: usize,
    pub external_import_modules: usize,
    pub private_source_suppressed_package_modules: usize,
    pub source_eliminated_package_modules: usize,
    pub remaining_package_source_modules: usize,
    pub external_import_candidates: usize,
    pub unsafe_external_import_modules: usize,
    pub package_surfaces: usize,
    pub audit_findings: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PackageCacheAuditOutcome {
    pub rows: usize,
    pub missing_identity_rows: usize,
    pub invalid_version_rows: usize,
    pub stale_policy_rows: usize,
    pub missing_export_specifier_rows: usize,
    pub missing_import_policy_rows: usize,
    pub parse_error_rows: usize,
    pub deleted_rows: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PackageExternalizationHintsOutcome {
    pub scanned_rows: usize,
    pub verified_rows: usize,
    pub invalid_export_specifier_rows: usize,
    pub invalid_version_rows: usize,
    pub content_hash_mismatch_rows: usize,
    pub normalize_error_rows: usize,
    pub written_rows: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct PackageModuleSourceQualityCounts {
    trusted: usize,
    weak: usize,
    invalid: usize,
    missing: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceUnitPackagePathHint {
    package_name: String,
    package_version: Option<String>,
    semantic_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractAssetsOutcome {
    pub project_id: u32,
    pub referenced_assets: usize,
    pub matched_assets: usize,
    pub missing_assets: usize,
    pub written_assets: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeInventoryOutcome {
    pub projects: Vec<RuntimeInventoryProject>,
    pub totals: RuntimeInventoryCounts,
    pub audit_findings: usize,
    pub skipped_projects: usize,
    pub skipped_source_bytes: u64,
    pub setter_blockers: Option<RuntimeSetterMigrationBlockerReport>,
    pub emitted_setter_blockers: Option<RuntimeSetterMigrationBlockerReport>,
    pub runtime_attribution: Option<RuntimeLineAttributionReport>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeInventoryProject {
    pub project_id: u32,
    pub source_bytes: u64,
    pub skipped: bool,
    pub counts: RuntimeInventoryCounts,
    pub audit_findings: usize,
    pub audit_errors: usize,
    pub audit_warnings: usize,
    pub audit_finding_codes: Vec<(String, usize)>,
    pub setter_blockers: Option<RuntimeSetterMigrationBlockerReport>,
    pub emitted_setter_blockers: Option<RuntimeSetterMigrationBlockerReport>,
    pub runtime_attribution: Option<RuntimeLineAttributionReport>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeInventoryCounts {
    pub files: usize,
    pub source_lines: usize,
    pub runtime_files: usize,
    pub runtime_lines: usize,
    pub runtime_reexport_only_files: usize,
    pub runtime_import_statements: usize,
    pub runtime_reexport_statements: usize,
    pub setter_occurrences: usize,
    pub setter_import_statements: usize,
    pub setter_function_definitions: usize,
    pub reverts_internal_occurrences: usize,
    pub named_import_statements: usize,
    pub named_export_statements: usize,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RuntimeLineAttributionReport {
    pub total_runtime_lines: usize,
    pub attributed_lines: usize,
    pub unattributed_lines: usize,
    pub items: Vec<RuntimeLineAttributionItem>,
    pub by_kind: BTreeMap<String, RuntimeLineAttributionBucket>,
    pub by_package: BTreeMap<String, RuntimeLineAttributionBucket>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeLineAttributionBucket {
    pub items: usize,
    pub lines: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeLineAttributionItem {
    pub path: String,
    pub line_start: usize,
    pub line_end: usize,
    pub lines: usize,
    pub kind: String,
    pub binding: String,
    pub package: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RuntimeInventoryProjectSelection {
    project_id: u32,
    source_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiscoveredProjectAsset {
    reference: AssetReference,
    source: DiscoveredAssetSource,
    output_path: String,
    kind: AssetKind,
    executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DiscoveredAssetSource {
    File(PathBuf),
    EmbeddedBunFile { bytes: Vec<u8> },
}

pub fn runtime_inventory_from_sqlite(
    args: &RuntimeInventoryArgs,
) -> Result<RuntimeInventoryOutcome, RuntimeInventoryError> {
    let selections = runtime_inventory_project_selections(args)?;
    let mut projects = Vec::with_capacity(selections.len());
    let mut totals = RuntimeInventoryCounts::default();
    let mut audit_findings = 0usize;
    let mut skipped_projects = 0usize;
    let mut skipped_source_bytes = 0u64;
    let mut setter_blockers = args
        .setter_blockers
        .then(RuntimeSetterMigrationBlockerReport::default);
    let mut emitted_setter_blockers = args
        .setter_blockers
        .then(RuntimeSetterMigrationBlockerReport::default);
    let mut runtime_attribution = args
        .runtime_attribution
        .then(RuntimeLineAttributionReport::default);

    for selection in selections {
        if args
            .max_source_bytes
            .is_some_and(|max_source_bytes| selection.source_bytes > max_source_bytes)
        {
            skipped_projects += 1;
            skipped_source_bytes += selection.source_bytes;
            projects.push(RuntimeInventoryProject {
                project_id: selection.project_id,
                source_bytes: selection.source_bytes,
                skipped: true,
                counts: RuntimeInventoryCounts::default(),
                audit_findings: 0,
                audit_errors: 0,
                audit_warnings: 0,
                audit_finding_codes: Vec::new(),
                setter_blockers: None,
                emitted_setter_blockers: None,
                runtime_attribution: None,
            });
            continue;
        }

        let input = load_project_bundle_from_sqlite(args.input.as_path(), selection.project_id)
            .map_err(RuntimeInventoryError::LoadInput)?;
        let runtime_package_ownership = args
            .runtime_attribution
            .then(|| runtime_package_ownership_by_binding(&input));
        let prepared = prepare_and_enrich(input).map_err(RuntimeInventoryError::Pipeline)?;
        let project_setter_blockers = args
            .setter_blockers
            .then(|| runtime_setter_migration_blocker_report_from_prepared(&prepared));
        if let (Some(total), Some(project_report)) =
            (setter_blockers.as_mut(), project_setter_blockers.as_ref())
        {
            total.add(project_report);
        }
        let run =
            generate_project_from_prepared(prepared).map_err(RuntimeInventoryError::Pipeline)?;
        let counts = runtime_inventory_counts_from_files(&run.project.files);
        let project_runtime_attribution = runtime_package_ownership
            .as_ref()
            .map(|ownership| runtime_line_attribution_from_files(&run.project.files, ownership));
        if let (Some(total), Some(project_report)) = (
            runtime_attribution.as_mut(),
            project_runtime_attribution.as_ref(),
        ) {
            total.add(project_report);
        }
        let project_emitted_setter_blockers = project_setter_blockers
            .as_ref()
            .map(|report| runtime_emitted_setter_blockers_from_files(&run.project.files, report));
        if let (Some(total), Some(project_report)) = (
            emitted_setter_blockers.as_mut(),
            project_emitted_setter_blockers.as_ref(),
        ) {
            total.add(project_report);
        }
        let project_audit_findings = run.audit.findings().len();
        let project_audit_errors = run.audit.error_count();
        let project_audit_warnings = run.audit.warning_count();
        let mut finding_code_counts: BTreeMap<String, usize> = BTreeMap::new();
        for finding in run.audit.findings() {
            *finding_code_counts
                .entry(format!("{:?}", finding.code))
                .or_insert(0) += 1;
        }
        let mut audit_finding_codes: Vec<(String, usize)> =
            finding_code_counts.into_iter().collect();
        audit_finding_codes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        if project_audit_findings > 0 && project_audit_findings <= 20 {
            for finding in run.audit.findings() {
                eprintln!(
                    "  finding {:?} [{:?}] module={:?} binding={:?} :: {}",
                    finding.severity,
                    finding.code,
                    finding.module,
                    finding.binding,
                    finding.message
                );
            }
        }
        totals.add(counts);
        audit_findings += project_audit_findings;
        projects.push(RuntimeInventoryProject {
            project_id: selection.project_id,
            source_bytes: selection.source_bytes,
            skipped: false,
            counts,
            audit_findings: project_audit_findings,
            audit_errors: project_audit_errors,
            audit_warnings: project_audit_warnings,
            audit_finding_codes,
            setter_blockers: project_setter_blockers,
            emitted_setter_blockers: project_emitted_setter_blockers,
            runtime_attribution: project_runtime_attribution,
        });
    }

    Ok(RuntimeInventoryOutcome {
        projects,
        totals,
        audit_findings,
        skipped_projects,
        skipped_source_bytes,
        setter_blockers,
        emitted_setter_blockers,
        runtime_attribution,
    })
}

fn runtime_emitted_setter_blockers_from_files(
    files: &[EmittedFile],
    report: &RuntimeSetterMigrationBlockerReport,
) -> RuntimeSetterMigrationBlockerReport {
    let mut emitted = RuntimeSetterMigrationBlockerReport::default();
    for file in files {
        let Some(source_file_id) = runtime_helper_source_file_id(file.path.as_str()) else {
            continue;
        };
        for binding in runtime_setter_targets_in_source(file.source.as_str()) {
            emitted.total_bindings += 1;
            let key = RuntimeSetterMigrationBindingKey {
                source_file_id,
                binding: binding.clone(),
            };
            match report.binding_statuses.get(&key).copied() {
                Some(RuntimeSetterMigrationBindingStatus::Accepted) => {
                    emitted.add_accepted(source_file_id, binding);
                }
                Some(RuntimeSetterMigrationBindingStatus::Blocked(reason)) => {
                    emitted.add_reason(source_file_id, binding, reason);
                }
                None => emitted.add_reason(
                    source_file_id,
                    binding,
                    RuntimeSetterMigrationBlockerReason::NoDiagnosticStatus,
                ),
            }
        }
    }
    emitted
}

fn runtime_helper_source_file_id(path: &str) -> Option<u32> {
    let filename = Path::new(path).file_name()?.to_str()?;
    let rest = filename.strip_prefix("source-")?;
    let source_id = rest.strip_suffix("-helpers.ts")?;
    source_id.parse().ok()
}

fn runtime_setter_targets_in_source(source: &str) -> Vec<BindingName> {
    // When the emitted runtime helper file can't be re-parsed (rare bundle
    // shapes the TS parser refuses, e.g. JSX comma patterns), return no
    // setters from this source — the file already has an UnparseableOutput
    // audit finding; we don't want to take the whole inventory run down.
    let Ok(facts) = collect_top_level_statement_facts(source, None, ParseGoal::TypeScript) else {
        return Vec::new();
    };
    facts
        .into_iter()
        .filter(|fact| fact.kind == TopLevelStatementKind::Setter)
        .flat_map(|fact| fact.bindings)
        .filter_map(|binding| runtime_setter_target_from_binding(binding.as_str()))
        .collect()
}

fn runtime_setter_target_from_binding(binding: &str) -> Option<BindingName> {
    let target = binding.strip_prefix("__reverts_set_")?;
    (!target.is_empty()).then(|| BindingName::new(target))
}

fn runtime_inventory_project_selections(
    args: &RuntimeInventoryArgs,
) -> Result<Vec<RuntimeInventoryProjectSelection>, RuntimeInventoryError> {
    let connection =
        Connection::open_with_flags(args.input.as_path(), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|source| RuntimeInventoryError::OpenDatabase {
                path: args.input.clone(),
                source,
            })?;

    if let Some(project_id) = args.project_id {
        let selection = connection
            .query_row(
                r"
                SELECT ?1, COALESCE(SUM(sf.file_size), 0)
                FROM project_files pf
                JOIN source_files sf ON sf.id = pf.file_id
                WHERE pf.project_id = ?1
                ",
                params![i64::from(project_id)],
                runtime_inventory_project_selection_from_row,
            )
            .map_err(RuntimeInventoryError::QueryProjects)?;
        return Ok(vec![selection]);
    }

    let order = if args.newest { "DESC" } else { "ASC" };
    let sql = if args.limit.is_some() {
        format!(
            r"
            SELECT p.id, COALESCE(SUM(sf.file_size), 0)
            FROM projects p
            LEFT JOIN project_files pf ON pf.project_id = p.id
            LEFT JOIN source_files sf ON sf.id = pf.file_id
            GROUP BY p.id
            ORDER BY p.id {order}
            LIMIT ?1
            "
        )
    } else {
        format!(
            r"
            SELECT p.id, COALESCE(SUM(sf.file_size), 0)
            FROM projects p
            LEFT JOIN project_files pf ON pf.project_id = p.id
            LEFT JOIN source_files sf ON sf.id = pf.file_id
            GROUP BY p.id
            ORDER BY p.id {order}
            "
        )
    };
    let mut statement = connection
        .prepare(sql.as_str())
        .map_err(RuntimeInventoryError::QueryProjects)?;
    if let Some(limit) = args.limit {
        let rows = statement
            .query_map(
                [i64::from(limit)],
                runtime_inventory_project_selection_from_row,
            )
            .map_err(RuntimeInventoryError::QueryProjects)?;
        collect_sqlite_rows(rows).map_err(RuntimeInventoryError::QueryProjects)
    } else {
        let rows = statement
            .query_map([], runtime_inventory_project_selection_from_row)
            .map_err(RuntimeInventoryError::QueryProjects)?;
        collect_sqlite_rows(rows).map_err(RuntimeInventoryError::QueryProjects)
    }
}

fn runtime_inventory_project_selection_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<RuntimeInventoryProjectSelection> {
    Ok(RuntimeInventoryProjectSelection {
        project_id: row.get(0)?,
        source_bytes: row.get(1)?,
    })
}

fn runtime_inventory_counts_from_files(files: &[EmittedFile]) -> RuntimeInventoryCounts {
    let mut counts = RuntimeInventoryCounts {
        files: files.len(),
        ..Default::default()
    };
    for file in files {
        let is_runtime_file = file.path.starts_with("modules/runtime/");
        counts.source_lines += file.source.lines().count();
        if is_runtime_file {
            counts.runtime_files += 1;
            counts.runtime_lines += file.source.lines().count();
            if is_runtime_reexport_only_source(file.source.as_str()) {
                counts.runtime_reexport_only_files += 1;
            }
        }
        counts.setter_occurrences += count_substring(file.source.as_str(), "__reverts_set_");
        counts.reverts_internal_occurrences += count_substring(file.source.as_str(), "__reverts_");
        for line in file.source.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("import {") {
                counts.named_import_statements += 1;
            }
            if trimmed.starts_with("export {") {
                counts.named_export_statements += 1;
            }
            if trimmed.starts_with("import ") && line_contains_runtime_path(trimmed) {
                counts.runtime_import_statements += 1;
            }
            if trimmed.starts_with("import ") && trimmed.contains("__reverts_set_") {
                counts.setter_import_statements += 1;
            }
            if is_runtime_file && trimmed.starts_with("export {") && trimmed.contains(" from ") {
                counts.runtime_reexport_statements += 1;
            }
        }
        counts.setter_function_definitions +=
            runtime_setter_targets_in_source(file.source.as_str()).len();
    }
    counts
}

fn runtime_line_attribution_from_files(
    files: &[EmittedFile],
    package_ownership: &BTreeMap<(u32, String), String>,
) -> RuntimeLineAttributionReport {
    let mut report = RuntimeLineAttributionReport::default();
    for file in files {
        if !file.path.starts_with("modules/runtime/") {
            continue;
        }
        let runtime_lines = file.source.lines().count();
        report.total_runtime_lines += runtime_lines;
        let mut covered_lines = BTreeSet::<usize>::new();
        let line_starts = line_start_offsets(file.source.as_str());
        let facts = collect_top_level_statement_facts(
            file.source.as_str(),
            Some(Path::new(file.path.as_str())),
            ParseGoal::TypeScript,
        )
        .expect("runtime line attribution requires parseable generated TypeScript source");
        for fact in facts {
            let line_start = line_number_for_offset(&line_starts, fact.byte_start as usize);
            let line_end = line_number_for_statement_end(&line_starts, fact.byte_end as usize);
            if line_end < line_start {
                continue;
            }
            for line in line_start..=line_end {
                covered_lines.insert(line);
            }
            let lines = line_end - line_start + 1;
            let binding = runtime_attribution_binding_label(&fact.bindings);
            let kind = fact.kind.as_str().to_string();
            let package = runtime_attribution_package_label(
                runtime_helper_source_file_id(file.path.as_str()),
                &fact.bindings,
                package_ownership,
            );
            let bucket = report.by_kind.entry(kind.clone()).or_default();
            bucket.items += 1;
            bucket.lines += lines;
            let package_bucket = report.by_package.entry(package.clone()).or_default();
            package_bucket.items += 1;
            package_bucket.lines += lines;
            report.items.push(RuntimeLineAttributionItem {
                path: file.path.clone(),
                line_start,
                line_end,
                lines,
                kind,
                binding,
                package,
            });
        }
        report.attributed_lines += covered_lines.len();
        report.unattributed_lines += runtime_lines.saturating_sub(covered_lines.len());
    }
    report
}

fn runtime_attribution_package_label(
    source_file_id: Option<u32>,
    bindings: &[String],
    package_ownership: &BTreeMap<(u32, String), String>,
) -> String {
    let Some(source_file_id) = source_file_id else {
        return "<runtime-glue>".to_string();
    };
    let Some(primary_binding) = bindings.first() else {
        return "<runtime-glue>".to_string();
    };
    let target_binding = primary_binding
        .strip_prefix("__reverts_set_")
        .unwrap_or(primary_binding);
    package_ownership
        .get(&(source_file_id, target_binding.to_string()))
        .cloned()
        .unwrap_or_else(|| "<unknown>".to_string())
}

fn runtime_package_ownership_by_binding(input: &InputBundle) -> BTreeMap<(u32, String), String> {
    let source_owners = runtime_source_span_owners(input);
    let original_name_owners = runtime_original_name_owners_by_binding(&input.modules);
    let enrichment = enrich_program(ProgramModel::from_input(input.clone()));
    let mut consumer_owners = BTreeMap::<(u32, String), BTreeSet<String>>::new();
    for module in enrichment.program.model().modules() {
        let owner = runtime_module_owner_label(module);
        for import in enrichment
            .program
            .model()
            .graph()
            .runtime_imports_for(module.id)
        {
            consumer_owners
                .entry((import.source_file_id, import.binding.as_str().to_string()))
                .or_default()
                .insert(owner.clone());
        }
    }
    let mut ownership = BTreeMap::<(u32, String), String>::new();
    for (key, owners) in &original_name_owners {
        ownership.insert(key.clone(), runtime_consumer_owner_label(owners));
    }
    for (source_file_id, prelude) in enrichment.program.model().graph().runtime_preludes() {
        let Some(owners) = source_owners.get(source_file_id) else {
            for binding in prelude.snippets.keys() {
                let key = (*source_file_id, binding.as_str().to_string());
                let owner = consumer_owners
                    .get(&key)
                    .map(runtime_consumer_owner_label)
                    .or_else(|| {
                        original_name_owners
                            .get(&key)
                            .map(runtime_consumer_owner_label)
                    })
                    .unwrap_or_else(|| "<unknown>".to_string());
                ownership.insert(key, owner);
            }
            continue;
        };
        for (binding, snippet) in &prelude.snippets {
            let key = (*source_file_id, binding.as_str().to_string());
            let snippet_byte_end = snippet
                .byte_start
                .saturating_add(u32::try_from(snippet.source.len()).unwrap_or(u32::MAX));
            let owner = consumer_owners
                .get(&key)
                .map(runtime_consumer_owner_label)
                .or_else(|| {
                    original_name_owners
                        .get(&key)
                        .map(runtime_consumer_owner_label)
                })
                .or_else(|| {
                    runtime_source_span_owner_label_for_range(
                        owners,
                        snippet.byte_start,
                        snippet_byte_end,
                    )
                })
                .unwrap_or_else(|| "<unknown>".to_string());
            ownership.insert(key, owner);
        }
    }
    ownership
}

fn runtime_original_name_owners_by_binding(
    modules: &[ModuleInput],
) -> BTreeMap<(u32, String), BTreeSet<String>> {
    let mut owners = BTreeMap::<(u32, String), BTreeSet<String>>::new();
    for module in modules {
        let Some(source_file_id) = module.source_file_id else {
            continue;
        };
        owners
            .entry((source_file_id, module.original_name.clone()))
            .or_default()
            .insert(runtime_module_owner_label(module));
    }
    owners
}

fn runtime_consumer_owner_label(owners: &BTreeSet<String>) -> String {
    match owners.len() {
        0 => "<unknown>".to_string(),
        1 => owners
            .iter()
            .next()
            .cloned()
            .unwrap_or_else(|| "<unknown>".to_string()),
        _ => "<shared>".to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeSourceSpanOwner {
    byte_start: u32,
    byte_end: u32,
    label: String,
}

fn runtime_source_span_owners(input: &InputBundle) -> BTreeMap<u32, Vec<RuntimeSourceSpanOwner>> {
    let mut owners = BTreeMap::<u32, Vec<RuntimeSourceSpanOwner>>::new();
    for module in &input.modules {
        let Some(source_file_id) = module.source_file_id else {
            continue;
        };
        let Some(span) = module.source_span else {
            continue;
        };
        owners
            .entry(source_file_id)
            .or_default()
            .push(RuntimeSourceSpanOwner {
                byte_start: span.byte_start,
                byte_end: span.byte_end,
                label: runtime_module_owner_label(module),
            });
    }
    for source_owners in owners.values_mut() {
        source_owners.sort_by_key(|owner| (owner.byte_start, owner.byte_end));
    }
    owners
}

fn runtime_source_span_owner_label_for_range(
    owners: &[RuntimeSourceSpanOwner],
    byte_start: u32,
    byte_end: u32,
) -> Option<String> {
    let mut overlapping = BTreeSet::<String>::new();
    for owner in owners {
        if owner.byte_start < byte_end && byte_start < owner.byte_end {
            overlapping.insert(owner.label.clone());
        }
    }
    if overlapping.is_empty() {
        None
    } else {
        Some(runtime_consumer_owner_label(&overlapping))
    }
}

fn runtime_module_owner_label(module: &ModuleInput) -> String {
    if let Some(package_name) = module
        .package_name
        .as_deref()
        .filter(|name| !name.is_empty())
    {
        return format!(
            "{}@{}",
            package_name,
            module.package_version.as_deref().unwrap_or("unknown")
        );
    }
    match module.kind {
        ModuleKind::Package => "<package>".to_string(),
        ModuleKind::Application => "<application>".to_string(),
        ModuleKind::Builtin => "<builtin>".to_string(),
    }
}

fn runtime_attribution_binding_label(bindings: &[String]) -> String {
    match bindings {
        [] => "<anonymous>".to_string(),
        [binding] => binding.clone(),
        [first, rest @ ..] => format!("{first}+{}", rest.len()),
    }
}

fn line_start_offsets(source: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (index, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(index + 1);
        }
    }
    starts
}

fn line_number_for_offset(line_starts: &[usize], offset: usize) -> usize {
    match line_starts.binary_search(&offset) {
        Ok(index) => index + 1,
        Err(index) => index.max(1),
    }
}

fn line_number_for_statement_end(line_starts: &[usize], end_offset: usize) -> usize {
    line_number_for_offset(line_starts, end_offset.saturating_sub(1))
}

fn is_runtime_reexport_only_source(source: &str) -> bool {
    let mut saw_relevant_line = false;
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        saw_relevant_line = true;
        if !(trimmed.starts_with("export {") && trimmed.contains(" from ")) {
            return false;
        }
    }
    saw_relevant_line
}

fn line_contains_runtime_path(line: &str) -> bool {
    line.contains("/runtime/")
}

fn count_substring(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

impl RuntimeInventoryCounts {
    fn add(&mut self, other: Self) {
        self.files += other.files;
        self.source_lines += other.source_lines;
        self.runtime_files += other.runtime_files;
        self.runtime_lines += other.runtime_lines;
        self.runtime_reexport_only_files += other.runtime_reexport_only_files;
        self.runtime_import_statements += other.runtime_import_statements;
        self.runtime_reexport_statements += other.runtime_reexport_statements;
        self.setter_occurrences += other.setter_occurrences;
        self.setter_import_statements += other.setter_import_statements;
        self.setter_function_definitions += other.setter_function_definitions;
        self.reverts_internal_occurrences += other.reverts_internal_occurrences;
        self.named_import_statements += other.named_import_statements;
        self.named_export_statements += other.named_export_statements;
    }
}

impl RuntimeLineAttributionReport {
    fn add(&mut self, other: &Self) {
        self.total_runtime_lines += other.total_runtime_lines;
        self.attributed_lines += other.attributed_lines;
        self.unattributed_lines += other.unattributed_lines;
        self.items.extend(other.items.iter().cloned());
        for (kind, bucket) in &other.by_kind {
            let target = self.by_kind.entry(kind.clone()).or_default();
            target.items += bucket.items;
            target.lines += bucket.lines;
        }
        for (package, bucket) in &other.by_package {
            let target = self.by_package.entry(package.clone()).or_default();
            target.items += bucket.items;
            target.lines += bucket.lines;
        }
    }
}

pub fn match_packages_from_sqlite(
    args: &MatchPackagesArgs,
) -> Result<MatchPackagesOutcome, MatchPackagesError> {
    let flags = if args.apply {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let mut connection =
        Connection::open_with_flags(args.input.as_path(), flags).map_err(|source| {
            MatchPackagesError::OpenDatabase {
                path: args.input.clone(),
                source,
            }
        })?;
    connection
        .busy_timeout(Duration::from_secs(30))
        .map_err(MatchPackagesError::ConfigureDatabase)?;
    connection
        .execute_batch("PRAGMA foreign_keys = ON")
        .map_err(MatchPackagesError::ConfigureDatabase)?;
    match_packages_from_connection(&mut connection, args)
}

pub fn match_packages_report_from_sqlite(
    args: &MatchPackagesReportArgs,
) -> Result<MatchPackagesReportOutcome, MatchPackagesError> {
    let project_ids = match_package_report_project_ids(args)?;
    let mut outcome = MatchPackagesReportOutcome::default();
    let mut blocker_counts = BTreeMap::<(String, String), usize>::new();
    for project_id in project_ids {
        let project_outcome = match_packages_from_sqlite(&MatchPackagesArgs {
            input: args.input.clone(),
            project_id,
            apply: false,
            package_names: args.package_names.clone(),
            package_source_roots: args.package_source_roots.clone(),
            materialize_package_sources: args.materialize_package_sources,
        })?;
        outcome.totals.package_modules += project_outcome.loaded_package_modules;
        outcome.totals.matched_modules += project_outcome.matched_modules;
        outcome.totals.external_import_modules += project_outcome.external_import_modules;
        outcome.totals.private_source_suppressed_package_modules +=
            project_outcome.private_source_suppressed_package_modules;
        outcome.totals.source_eliminated_package_modules +=
            project_outcome.source_eliminated_package_modules;
        outcome.totals.remaining_package_source_modules +=
            project_outcome.remaining_package_source_modules;
        outcome.totals.external_import_candidates += project_outcome.external_import_candidates;
        outcome.totals.unsafe_external_import_modules +=
            project_outcome.unsafe_external_import_modules;
        outcome.totals.package_surfaces += project_outcome.matched_package_surfaces;
        outcome.totals.audit_findings += project_outcome.audit.findings().len();
        for blocker in &project_outcome.external_import_blockers {
            *blocker_counts
                .entry((blocker.reason.clone(), blocker.consumer.clone()))
                .or_default() += blocker.count;
        }
        outcome.projects.push(project_outcome);
    }
    outcome.blockers = blocker_counts
        .into_iter()
        .map(|((reason, consumer), count)| ExternalImportBlockerSummary {
            reason,
            consumer,
            count,
        })
        .collect();
    outcome.blockers.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.reason.cmp(&right.reason))
            .then_with(|| left.consumer.cmp(&right.consumer))
    });
    Ok(outcome)
}

fn match_package_report_project_ids(
    args: &MatchPackagesReportArgs,
) -> Result<Vec<u32>, MatchPackagesError> {
    let connection =
        Connection::open_with_flags(args.input.as_path(), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|source| MatchPackagesError::OpenDatabase {
                path: args.input.clone(),
                source,
            })?;
    let order = if args.newest { "DESC" } else { "ASC" };
    let mut sql = format!("SELECT id FROM projects ORDER BY id {order}");
    if let Some(limit) = args.limit {
        use std::fmt::Write as _;
        let _ = write!(sql, " LIMIT {limit}");
    }
    let mut statement = connection
        .prepare(sql.as_str())
        .map_err(MatchPackagesError::QueryPackageSources)?;
    let rows = statement
        .query_map([], |row| row.get::<_, u32>(0))
        .map_err(MatchPackagesError::QueryPackageSources)?;
    collect_sqlite_rows(rows).map_err(MatchPackagesError::QueryPackageSources)
}

pub fn package_cache_audit_from_sqlite(
    args: &PackageCacheArgs,
    prune_stale: bool,
) -> Result<PackageCacheAuditOutcome, MatchPackagesError> {
    let flags = if prune_stale && args.apply {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let mut connection =
        Connection::open_with_flags(args.input.as_path(), flags).map_err(|source| {
            MatchPackagesError::OpenDatabase {
                path: args.input.clone(),
                source,
            }
        })?;
    connection
        .busy_timeout(Duration::from_secs(30))
        .map_err(MatchPackagesError::ConfigureDatabase)?;
    if !sqlite_table_exists(&connection, "package_source_cache")
        .map_err(MatchPackagesError::QueryPackageSources)?
    {
        return Err(MatchPackagesError::MissingTable("package_source_cache"));
    }
    let audit = package_cache_audit_from_connection(&connection)?;
    if prune_stale && args.apply && audit.deleted_rows > 0 {
        prune_package_source_cache_rows(&mut connection)?;
    }
    Ok(audit)
}

pub fn package_externalization_hints_from_sqlite(
    args: &PackageExternalizationHintsArgs,
) -> Result<PackageExternalizationHintsOutcome, MatchPackagesError> {
    let flags = if args.apply {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let mut connection =
        Connection::open_with_flags(args.input.as_path(), flags).map_err(|source| {
            MatchPackagesError::OpenDatabase {
                path: args.input.clone(),
                source,
            }
        })?;
    connection
        .busy_timeout(Duration::from_secs(30))
        .map_err(MatchPackagesError::ConfigureDatabase)?;
    if !sqlite_table_exists(&connection, "package_source_cache")
        .map_err(MatchPackagesError::QueryPackageSources)?
    {
        return Err(MatchPackagesError::MissingTable("package_source_cache"));
    }
    package_externalization_hints_from_connection(&mut connection, args)
}

fn package_externalization_hints_from_connection(
    connection: &mut Connection,
    args: &PackageExternalizationHintsArgs,
) -> Result<PackageExternalizationHintsOutcome, MatchPackagesError> {
    let package_names = args
        .package_names
        .iter()
        .map(|package_name| package_name.trim().to_string())
        .filter(|package_name| !package_name.is_empty())
        .collect::<BTreeSet<_>>();
    let candidates =
        externalization_hint_candidates_from_cache(connection, &package_names, args.limit)?;
    let mut outcome = PackageExternalizationHintsOutcome {
        scanned_rows: candidates.len(),
        ..PackageExternalizationHintsOutcome::default()
    };
    let mut hints = Vec::new();
    let mut hint_keys = BTreeSet::<(String, String, String, String)>::new();
    let mut verified_sources = Vec::<VerifiedExternalizationHintSource>::new();
    for candidate in candidates {
        if !is_exact_package_version_hint(candidate.package_version.as_str()) {
            outcome.invalid_version_rows += 1;
            continue;
        }
        if !hint_export_specifier_matches_package(
            candidate.package_name.as_str(),
            candidate.export_specifier.as_str(),
        ) {
            outcome.invalid_export_specifier_rows += 1;
            continue;
        }
        let actual_content_hash = stable_hash(candidate.source_content.as_bytes());
        if actual_content_hash != candidate.content_hash {
            outcome.content_hash_mismatch_rows += 1;
            continue;
        }
        let source_path = format!(
            "{}@{}/{}",
            candidate.package_name, candidate.package_version, candidate.entry_path
        );
        let Some(normalized_source_hash) =
            package_source_normalized_hash(source_path.as_str(), candidate.source_content.as_str())
        else {
            outcome.normalize_error_rows += 1;
            continue;
        };
        let public_members = package_source_exported_members(
            source_path.as_str(),
            candidate.source_content.as_str(),
        );
        let source = if candidate.external_importable {
            PackageSource::external(
                candidate.package_name.as_str(),
                candidate.package_version.as_str(),
                candidate.export_specifier.as_str(),
                source_path.as_str(),
                candidate.source_content.as_str(),
            )
        } else {
            PackageSource::source_only(
                candidate.package_name.as_str(),
                candidate.package_version.as_str(),
                candidate.export_specifier.as_str(),
                source_path.as_str(),
                candidate.source_content.as_str(),
            )
        };
        let verified = VerifiedExternalizationHintSource {
            package_name: candidate.package_name,
            package_version: candidate.package_version,
            entry_path: candidate.entry_path,
            source,
            content_hash: actual_content_hash,
            normalized_source_hash,
            public_members,
        };
        if verified.source.external_importable {
            push_package_externalization_hint(
                &mut hints,
                &mut hint_keys,
                PackageExternalizationHint {
                    package_name: verified.package_name.clone(),
                    package_version: verified.package_version.clone(),
                    entry_path: verified.entry_path.clone(),
                    export_specifier: verified.source.export_specifier.clone(),
                    content_hash: Some(verified.content_hash.clone()),
                    normalized_source_hash: Some(verified.normalized_source_hash.clone()),
                    public_members: verified.public_members.clone(),
                    proof_policy_version: Some(PACKAGE_EXTERNALIZATION_HINT_POLICY_VERSION),
                },
            );
        }
        verified_sources.push(verified);
    }
    let package_sources = verified_sources
        .iter()
        .map(|source| source.source.clone())
        .collect::<Vec<_>>();
    let verified_sources_by_path = verified_sources
        .iter()
        .map(|source| (source.source.source_path.clone(), source))
        .collect::<BTreeMap<_, _>>();
    for proof in package_source_public_export_proofs(&package_sources) {
        if !hint_export_specifier_matches_package(
            proof.package_name.as_str(),
            proof.export_specifier.as_str(),
        ) {
            outcome.invalid_export_specifier_rows += 1;
            continue;
        }
        let Some(source) = verified_sources_by_path.get(proof.source_path.as_str()) else {
            continue;
        };
        if proof.public_members.is_empty() {
            continue;
        }
        push_package_externalization_hint(
            &mut hints,
            &mut hint_keys,
            PackageExternalizationHint {
                package_name: source.package_name.clone(),
                package_version: source.package_version.clone(),
                entry_path: source.entry_path.clone(),
                export_specifier: proof.export_specifier,
                content_hash: Some(source.content_hash.clone()),
                normalized_source_hash: Some(source.normalized_source_hash.clone()),
                public_members: proof.public_members,
                proof_policy_version: Some(PACKAGE_EXTERNALIZATION_HINT_POLICY_VERSION),
            },
        );
    }
    outcome.verified_rows = hints.len();
    if args.apply && !hints.is_empty() {
        outcome.written_rows = persist_package_externalization_hints(connection, &hints)?;
    } else {
        outcome.written_rows = hints.len();
    }
    Ok(outcome)
}

#[derive(Debug)]
struct VerifiedExternalizationHintSource {
    package_name: String,
    package_version: String,
    entry_path: String,
    source: PackageSource,
    content_hash: String,
    normalized_source_hash: String,
    public_members: BTreeSet<String>,
}

fn push_package_externalization_hint(
    hints: &mut Vec<PackageExternalizationHint>,
    keys: &mut BTreeSet<(String, String, String, String)>,
    hint: PackageExternalizationHint,
) {
    let key = (
        hint.package_name.clone(),
        hint.package_version.clone(),
        hint.entry_path.clone(),
        hint.export_specifier.clone(),
    );
    if keys.insert(key) {
        hints.push(hint);
    }
}

fn package_cache_audit_from_connection(
    connection: &Connection,
) -> Result<PackageCacheAuditOutcome, MatchPackagesError> {
    let rows = audited_package_source_cache_rows(connection)?;
    let mut outcome = PackageCacheAuditOutcome {
        rows: rows.len(),
        ..PackageCacheAuditOutcome::default()
    };
    for row in rows {
        if row.missing_identity {
            outcome.missing_identity_rows += 1;
        }
        if !is_exact_package_version_hint(row.package_version.as_str()) {
            outcome.invalid_version_rows += 1;
        }
        if row.missing_policy {
            outcome.missing_import_policy_rows += 1;
        }
        if row.missing_export_specifier {
            outcome.missing_export_specifier_rows += 1;
        }
        if row.stale_policy {
            outcome.stale_policy_rows += 1;
        }
        let parse_error = normalize_source_for_pipeline(
            row.source_content.as_str(),
            Some(Path::new(row.source_path.as_str())),
        )
        .is_err();
        if parse_error {
            outcome.parse_error_rows += 1;
        }
        if row.prune_candidate || parse_error {
            outcome.deleted_rows += 1;
        }
    }
    Ok(outcome)
}

#[derive(Debug)]
struct AuditedPackageSourceCacheRow {
    rowid: i64,
    package_version: String,
    source_path: String,
    source_content: String,
    missing_identity: bool,
    missing_policy: bool,
    missing_export_specifier: bool,
    stale_policy: bool,
    prune_candidate: bool,
}

fn audited_package_source_cache_rows(
    connection: &Connection,
) -> Result<Vec<AuditedPackageSourceCacheRow>, MatchPackagesError> {
    let has_policy = sqlite_table_has_column(
        connection,
        "package_source_cache",
        "external_import_policy_version",
    )
    .map_err(MatchPackagesError::QueryPackageSources)?;
    let has_export_specifier =
        sqlite_table_has_column(connection, "package_source_cache", "export_specifier")
            .map_err(MatchPackagesError::QueryPackageSources)?;
    let policy_expr = if has_policy {
        "external_import_policy_version"
    } else {
        "0"
    };
    let export_expr = if has_export_specifier {
        "export_specifier"
    } else {
        "''"
    };
    let sql = format!(
        r"
        SELECT rowid, package_name, package_version, entry_path, source_content,
               {policy_expr} AS policy_version,
               {export_expr} AS export_specifier
          FROM package_source_cache
        "
    );
    let mut statement = connection
        .prepare(sql.as_str())
        .map_err(MatchPackagesError::QueryPackageSources)?;
    let rows = statement
        .query_map([], |row| {
            let rowid = row.get::<_, i64>(0)?;
            let package_name = row.get::<_, Option<String>>(1)?.unwrap_or_default();
            let package_version = row.get::<_, Option<String>>(2)?.unwrap_or_default();
            let entry_path = row.get::<_, Option<String>>(3)?.unwrap_or_default();
            let source_content = row.get::<_, Option<String>>(4)?.unwrap_or_default();
            let policy_version = row.get::<_, Option<i64>>(5)?.unwrap_or_default();
            let export_specifier = row.get::<_, Option<String>>(6)?.unwrap_or_default();
            let missing_identity = package_name.trim().is_empty()
                || package_version.trim().is_empty()
                || entry_path.trim().is_empty();
            let missing_policy = !has_policy || policy_version == 0;
            let missing_export_specifier =
                !has_export_specifier || export_specifier.trim().is_empty();
            let stale_policy = missing_policy
                || policy_version != PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION
                || missing_export_specifier;
            let invalid_version = !is_exact_package_version_hint(package_version.as_str());
            Ok(AuditedPackageSourceCacheRow {
                rowid,
                source_path: format!("{package_name}@{package_version}/{entry_path}"),
                package_version,
                source_content,
                missing_identity,
                missing_policy,
                missing_export_specifier,
                stale_policy,
                prune_candidate: missing_identity || invalid_version || stale_policy,
            })
        })
        .map_err(MatchPackagesError::QueryPackageSources)?;
    collect_sqlite_rows(rows).map_err(MatchPackagesError::QueryPackageSources)
}

fn prune_package_source_cache_rows(connection: &mut Connection) -> Result<(), MatchPackagesError> {
    let mut rowids_to_delete = Vec::<i64>::new();
    for row in audited_package_source_cache_rows(connection)? {
        let parse_error = normalize_source_for_pipeline(
            row.source_content.as_str(),
            Some(Path::new(row.source_path.as_str())),
        )
        .is_err();
        if row.prune_candidate || parse_error {
            rowids_to_delete.push(row.rowid);
        }
    }
    if rowids_to_delete.is_empty() {
        return Ok(());
    }
    let transaction = connection
        .transaction()
        .map_err(MatchPackagesError::WritePackageSourceCache)?;
    {
        let mut statement = transaction
            .prepare("DELETE FROM package_source_cache WHERE rowid = ?1")
            .map_err(MatchPackagesError::WritePackageSourceCache)?;
        for rowid in rowids_to_delete {
            statement
                .execute([rowid])
                .map_err(MatchPackagesError::WritePackageSourceCache)?;
        }
    }
    transaction
        .commit()
        .map_err(MatchPackagesError::WritePackageSourceCache)
}

pub fn match_packages_from_connection(
    connection: &mut Connection,
    args: &MatchPackagesArgs,
) -> Result<MatchPackagesOutcome, MatchPackagesError> {
    let timing_enabled = std::env::var_os("REVERTS_MATCH_TIMING").is_some();
    let timing_started = Instant::now();
    let mut timing_last = timing_started;
    macro_rules! mark_timing {
        ($stage:literal) => {
            if timing_enabled {
                let now = Instant::now();
                eprintln!(
                    "match-packages timing: {} stage={:.3}s total={:.3}s",
                    $stage,
                    now.duration_since(timing_last).as_secs_f64(),
                    now.duration_since(timing_started).as_secs_f64()
                );
                timing_last = now;
            }
        };
    }
    let mut rows = load_project_rows_from_connection(connection, args.project_id)
        .map_err(MatchPackagesError::LoadInput)?;
    mark_timing!("load_project_rows");

    // Shared bundle-aware row preparation: split recognised bundle wrappers
    // into per-module rows before either matcher or generator sees them.
    let prepared = prepare_input_rows_for_pipeline(rows);
    let extraction_audit = prepared.audit;
    // Snapshot new_modules from the shared preparation — we need them later
    // to persist synthetic rows into the SQLite modules table so
    // function-level attributions can FK them.
    let synthetic_modules = prepared.synthetic_modules;
    rows = prepared.rows;
    enrich_package_modules_from_source_units(connection, &mut rows, args.project_id)?;
    mark_timing!("bundle_extract_enrich");

    let mut source_import_audit = AuditReport::default();
    let package_names =
        package_source_load_scope(&rows, &args.package_names, &mut source_import_audit);
    remove_package_attributions_for_revalidation(&mut rows, &package_names);
    let mut package_sources = load_package_sources(
        connection,
        &rows,
        &package_names,
        &args.package_source_roots,
        args.materialize_package_sources,
        args.apply,
    )?;
    mark_timing!("load_package_sources");
    let package_versions_before_resolution = package_versions_by_module(&rows);
    resolve_package_version_hints_to_available_sources(
        &mut rows,
        &package_sources,
        &package_names,
    )?;
    let package_version_resolutions =
        package_version_resolution_evidence(&package_versions_before_resolution, &rows);
    mark_timing!("resolve_versions");
    filter_package_sources_to_referenced_package_versions(&rows, &mut package_sources);
    mark_timing!("filter_referenced_versions");
    let source_quality_counts = package_module_source_quality_counts(
        &rows,
        (!args.package_names.is_empty()).then_some(&package_names),
    );
    mark_timing!("source_quality_counts");
    let pipeline_report = match_packages_with_pipeline(
        &rows,
        &package_sources,
        (!args.package_names.is_empty()).then_some(&package_names),
    );
    mark_timing!("match_pipeline");
    let mut report = pipeline_report.package_report;
    report.audit.extend(source_import_audit);
    let external_import_candidates = report.attributions.len();
    let external_import_safety =
        filter_unsafe_interpackage_external_attributions(&rows, &mut report);
    let function_attributions = pipeline_report.function_attributions;
    let function_ownership_matches = pipeline_report.function_ownership_matches;

    let (written_attributions, written_surfaces, written_function_attributions) = if args.apply {
        // Persist synthetic modules first so the FK from
        // package_attributions.module_id and
        // package_function_attributions.module_id resolves.
        persist_synthetic_modules(connection, &synthetic_modules)?;
        // A few synthetic modules may have been skipped by `INSERT OR
        // IGNORE` due to `UNIQUE(file_id, original_name)` collisions
        // with pre-existing rows. Build the set of module ids
        // that actually live in the SQLite `modules` table now, and
        // filter function-level attributions to only those — the alternative
        // is a foreign-key violation that aborts the whole apply.
        let mut persisted_ids = std::collections::BTreeSet::new();
        {
            let mut stmt = connection
                .prepare("SELECT id FROM modules")
                .map_err(MatchPackagesError::WriteAttribution)?;
            let mut rs = stmt
                .query([])
                .map_err(MatchPackagesError::WriteAttribution)?;
            while let Some(row) = rs.next().map_err(MatchPackagesError::WriteAttribution)? {
                let id: u32 = row.get(0).map_err(MatchPackagesError::WriteAttribution)?;
                persisted_ids.insert(reverts_ir::ModuleId(id));
            }
        }
        let persistable_function_attributions = function_attributions
            .iter()
            .filter(|attribution| persisted_ids.contains(&attribution.module_id))
            .cloned()
            .collect::<Vec<_>>();
        (
            persist_package_attributions(
                connection,
                &rows,
                &report,
                &package_names,
                &package_version_resolutions,
            )?,
            persist_package_surfaces(connection, &rows, &report)?,
            persist_function_attributions(connection, &rows, &persistable_function_attributions)?,
        )
    } else {
        (0, 0, 0)
    };
    mark_timing!("persist");
    if timing_enabled {
        let _ = timing_last;
    }

    let matched_modules = report.matches.len();
    let loaded_package_modules = rows
        .modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
        .count();
    let source_elimination =
        package_source_elimination_stats_for_report(&rows, &report, loaded_package_modules);
    let matched_package_surfaces = report.surfaces.len();
    let mut audit = extraction_audit;
    audit.extend(report.audit);
    let audit = dedup_audit_report(audit);

    Ok(MatchPackagesOutcome {
        project_id: args.project_id,
        loaded_package_modules,
        loaded_package_sources: package_sources.len(),
        matched_modules,
        external_import_modules: source_elimination.direct_external_import_modules,
        private_source_suppressed_package_modules: source_elimination
            .private_source_suppressed_package_modules,
        source_eliminated_package_modules: source_elimination.source_eliminated_package_modules,
        remaining_package_source_modules: source_elimination.remaining_package_source_modules,
        external_import_candidates,
        unsafe_external_import_modules: external_import_safety.removed_modules,
        matched_package_surfaces,
        written_attributions,
        written_surfaces,
        function_attributions: function_attributions.len(),
        function_ownership_matches,
        written_function_attributions,
        package_source_quality_trusted: source_quality_counts.trusted,
        package_source_quality_weak: source_quality_counts.weak,
        package_source_quality_invalid: source_quality_counts.invalid,
        package_source_quality_missing: source_quality_counts.missing,
        audit,
        external_import_blockers: external_import_safety.blockers,
    })
}

fn remove_package_attributions_for_revalidation(
    rows: &mut InputRows,
    requested_package_names: &BTreeSet<String>,
) -> usize {
    let before = rows.package_attributions.len();
    rows.package_attributions.retain(|attribution| {
        (!requested_package_names.is_empty()
            && !requested_package_names.contains(&attribution.package_name))
            || attribution.emission_mode != PackageEmissionMode::ExternalImport
    });
    before.saturating_sub(rows.package_attributions.len())
}

fn package_module_source_quality_counts(
    rows: &InputRows,
    package_filter: Option<&BTreeSet<String>>,
) -> PackageModuleSourceQualityCounts {
    let mut counts = PackageModuleSourceQualityCounts::default();
    for module in &rows.modules {
        if module.kind != ModuleKind::Package {
            continue;
        }
        if let Some(package_filter) = package_filter
            && !module
                .package_name
                .as_deref()
                .is_some_and(|package_name| package_filter.contains(package_name))
        {
            continue;
        }
        let Some(slice) = rows.module_source_slice(module.id) else {
            counts.missing += 1;
            continue;
        };
        match package_module_source_quality(module, slice.source_file_path, slice.source) {
            PackageModuleSourceQuality::Trusted => counts.trusted += 1,
            PackageModuleSourceQuality::Weak => counts.weak += 1,
            PackageModuleSourceQuality::Invalid => counts.invalid += 1,
        }
    }
    counts
}

fn enrich_package_modules_from_source_units(
    connection: &Connection,
    rows: &mut InputRows,
    project_id: u32,
) -> Result<(), MatchPackagesError> {
    let hints = load_source_unit_package_path_hints(connection, project_id)?;
    if hints.is_empty() {
        return Ok(());
    }
    for module in &mut rows.modules {
        if module.kind != ModuleKind::Package {
            continue;
        }
        let Some(source_file_id) = module.source_file_id else {
            continue;
        };
        let Some(package_name) = module.package_name.as_deref() else {
            continue;
        };
        let Some(file_hints) = hints.get(&source_file_id) else {
            continue;
        };
        let Some(hint) = file_hints
            .iter()
            .find(|hint| hint.package_name == package_name)
        else {
            continue;
        };
        if clean_package_semantic_path_hint(package_name, module.semantic_path.as_str()).is_none()
            || hint.semantic_path.len() > module.semantic_path.len()
        {
            module.semantic_path = hint.semantic_path.clone();
        }
        if module.package_version.is_none() {
            module.package_version = hint.package_version.clone();
        }
    }
    Ok(())
}

fn load_source_unit_package_path_hints(
    connection: &Connection,
    project_id: u32,
) -> Result<BTreeMap<u32, Vec<SourceUnitPackagePathHint>>, MatchPackagesError> {
    if !sqlite_table_exists(connection, "source_units")
        .map_err(MatchPackagesError::QueryPackageSources)?
    {
        return Ok(BTreeMap::new());
    }
    for column in ["file_id", "logical_path", "package_name", "package_version"] {
        if !sqlite_table_has_column(connection, "source_units", column)
            .map_err(MatchPackagesError::QueryPackageSources)?
        {
            return Ok(BTreeMap::new());
        }
    }

    let mut statement = connection
        .prepare(
            r"
            SELECT file_id, logical_path, package_name, package_version
              FROM source_units
             WHERE project_id = ?1
               AND file_id IS NOT NULL
               AND TRIM(COALESCE(logical_path, '')) != ''
            ",
        )
        .map_err(MatchPackagesError::QueryPackageSources)?;
    let rows = statement
        .query_map([i64::from(project_id)], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(MatchPackagesError::QueryPackageSources)?;
    let raw_rows = collect_sqlite_rows(rows).map_err(MatchPackagesError::QueryPackageSources)?;
    let mut hints = BTreeMap::<u32, Vec<SourceUnitPackagePathHint>>::new();
    for (file_id, logical_path, package_name, package_version) in raw_rows {
        let Ok(file_id) = u32::try_from(file_id) else {
            continue;
        };
        let Some((package_name, semantic_path)) =
            source_unit_package_semantic_path(package_name.as_deref(), logical_path.as_str())
        else {
            continue;
        };
        hints
            .entry(file_id)
            .or_default()
            .push(SourceUnitPackagePathHint {
                package_name,
                package_version: package_version
                    .map(|version| version.trim().to_string())
                    .filter(|version| is_exact_package_version_hint(version)),
                semantic_path,
            });
    }
    for file_hints in hints.values_mut() {
        file_hints.sort_by(|left, right| {
            right
                .semantic_path
                .len()
                .cmp(&left.semantic_path.len())
                .then_with(|| left.semantic_path.cmp(&right.semantic_path))
        });
        file_hints.dedup();
    }
    Ok(hints)
}

fn source_unit_package_semantic_path(
    package_name_hint: Option<&str>,
    logical_path: &str,
) -> Option<(String, String)> {
    let clean = logical_path
        .trim()
        .split(['?', '#'])
        .next()
        .unwrap_or(logical_path)
        .replace('\\', "/");
    if clean.is_empty() {
        return None;
    }

    if let Some(package_name) = package_name_hint
        .map(str::trim)
        .filter(|package_name| is_valid_package_name(package_name))
        && let Some(rest) = package_subpath_from_logical_path(package_name, clean.as_str())
    {
        return Some((package_name.to_string(), format!("{package_name}/{rest}")));
    }

    let marker = "node_modules/";
    let index = clean.rfind(marker)?;
    let after = clean.get(index + marker.len()..)?.trim_start_matches('/');
    let mut segments = after.split('/').filter(|segment| !segment.is_empty());
    let first = segments.next()?;
    let package_name = if first.starts_with('@') {
        let second = segments.next()?;
        format!("{first}/{second}")
    } else {
        first.to_string()
    };
    if !is_valid_package_name(package_name.as_str()) {
        return None;
    }
    let rest = segments.collect::<Vec<_>>().join("/");
    if rest.is_empty() {
        return None;
    }
    Some((
        package_name.clone(),
        format!("{package_name}/{}", strip_source_extension(rest.as_str())),
    ))
}

fn package_subpath_from_logical_path<'a>(
    package_name: &str,
    logical_path: &'a str,
) -> Option<&'a str> {
    let package_path = package_name.trim_start_matches('@');
    for marker in [
        format!("node_modules/{package_name}/"),
        format!("node_modules/{package_path}/"),
        format!("/{package_name}/"),
        format!("/{package_path}/"),
        format!("{package_name}/"),
        format!("{package_path}/"),
    ] {
        if let Some(index) = logical_path.find(marker.as_str()) {
            let rest = logical_path.get(index + marker.len()..)?;
            if !rest.trim_matches('/').is_empty() {
                return Some(strip_source_extension(rest.trim_matches('/')));
            }
        }
    }
    None
}

pub fn extract_assets_from_sqlite(
    args: &ExtractAssetsArgs,
) -> Result<ExtractAssetsOutcome, ExtractAssetsError> {
    let flags = if args.apply {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let mut connection =
        Connection::open_with_flags(args.input.as_path(), flags).map_err(|source| {
            ExtractAssetsError::OpenDatabase {
                path: args.input.clone(),
                source,
            }
        })?;
    connection
        .busy_timeout(Duration::from_secs(30))
        .map_err(ExtractAssetsError::ConfigureDatabase)?;
    connection
        .execute_batch("PRAGMA foreign_keys = ON")
        .map_err(ExtractAssetsError::ConfigureDatabase)?;
    extract_assets_from_connection(&mut connection, args)
}

pub fn extract_assets_from_connection(
    connection: &mut Connection,
    args: &ExtractAssetsArgs,
) -> Result<ExtractAssetsOutcome, ExtractAssetsError> {
    let rows = load_project_rows_from_connection(connection, args.project_id)
        .map_err(ExtractAssetsError::LoadInput)?;
    let references = collect_required_asset_references_from_rows(&rows);
    let referenced_logical_paths = references
        .iter()
        .map(|reference| reference.logical_path.as_str())
        .collect::<BTreeSet<_>>();
    let discovered = discover_project_assets(&rows, &references, &args.asset_roots)?;
    let written_assets = if args.apply {
        let materialized_root = materialized_asset_root(args.input.as_path(), rows.project.id);
        persist_project_assets(
            connection,
            rows.project.id,
            &discovered,
            materialized_root.as_path(),
        )?
    } else {
        0
    };

    Ok(ExtractAssetsOutcome {
        project_id: rows.project.id,
        referenced_assets: referenced_logical_paths.len(),
        matched_assets: discovered.len(),
        missing_assets: referenced_logical_paths
            .len()
            .saturating_sub(discovered.len()),
        written_assets,
    })
}

fn discover_project_assets(
    rows: &InputRows,
    references: &[AssetReference],
    asset_roots: &[PathBuf],
) -> Result<Vec<DiscoveredProjectAsset>, ExtractAssetsError> {
    let default_asset_root =
        common_source_root(&rows.source_files).ok_or(ExtractAssetsError::CannotInferAssetRoot {
            project_id: rows.project.id,
        })?;
    let effective_asset_roots = if asset_roots.is_empty() {
        vec![default_asset_root]
    } else {
        asset_roots.to_vec()
    };
    let modules = rows
        .modules
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let source_files = rows
        .source_files
        .iter()
        .map(|source_file| (source_file.id, source_file))
        .collect::<BTreeMap<_, _>>();
    let mut discovered = Vec::new();
    let mut seen_logical_paths = BTreeSet::new();

    for reference in references {
        if !seen_logical_paths.insert(reference.logical_path.as_str()) {
            continue;
        }
        let Some(module) = modules.get(&reference.module_id).copied() else {
            continue;
        };
        let Some(source_file) = module
            .source_file_id
            .and_then(|source_file_id| source_files.get(&source_file_id).copied())
        else {
            continue;
        };
        let source = discover_asset_source_from_roots(
            reference.logical_path.as_str(),
            source_file.path.as_str(),
            &effective_asset_roots,
        )?;
        let Some(source) = source else {
            continue;
        };
        let Some(output_path) = asset_output_path(module, reference.logical_path.as_str()) else {
            continue;
        };
        discovered.push(DiscoveredProjectAsset {
            reference: reference.clone(),
            source,
            output_path,
            kind: infer_asset_kind(reference.logical_path.as_str()),
            executable: infer_asset_executable(reference.logical_path.as_str()),
        });
    }

    Ok(discovered)
}

fn discover_asset_source_from_roots(
    logical_path: &str,
    source_file_path: &str,
    asset_roots: &[PathBuf],
) -> Result<Option<DiscoveredAssetSource>, ExtractAssetsError> {
    let mut matches = Vec::new();
    for asset_root in asset_roots {
        if let Some(source) =
            discover_asset_source(logical_path, source_file_path, asset_root.as_path())?
        {
            matches.push(source);
        }
    }

    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        _ => Err(ExtractAssetsError::AmbiguousAsset {
            logical_path: logical_path.to_string(),
            candidates: matches
                .iter()
                .map(DiscoveredAssetSource::description)
                .collect(),
        }),
    }
}

fn discover_asset_source(
    logical_path: &str,
    source_file_path: &str,
    asset_root: &Path,
) -> Result<Option<DiscoveredAssetSource>, ExtractAssetsError> {
    if bun_root_relative_path(logical_path).is_some()
        && asset_root.is_file()
        && let Some(bytes) = extract_bun_embedded_asset(asset_root, logical_path)?
    {
        return Ok(Some(DiscoveredAssetSource::EmbeddedBunFile { bytes }));
    }

    let physical_asset_root = if asset_root.is_file() {
        asset_root.parent().unwrap_or_else(|| Path::new(""))
    } else {
        asset_root
    };
    let source_path = asset_source_path(logical_path, source_file_path, physical_asset_root);
    if source_path.is_file() {
        Ok(Some(DiscoveredAssetSource::File(source_path)))
    } else {
        Ok(None)
    }
}

impl DiscoveredAssetSource {
    fn description(&self) -> String {
        match self {
            Self::File(path) => path.to_string_lossy().into_owned(),
            Self::EmbeddedBunFile { bytes } => {
                format!("embedded bun payload ({} bytes)", bytes.len())
            }
        }
    }
}

fn asset_source_path(logical_path: &str, source_file_path: &str, asset_root: &Path) -> PathBuf {
    if let Some(root_relative) = bun_root_relative_path(logical_path) {
        return asset_root.join(root_relative);
    }
    let logical = Path::new(logical_path);
    if logical_path.starts_with("./") || logical_path.starts_with("../") {
        return Path::new(source_file_path)
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(logical);
    }
    asset_root.join(logical)
}

fn asset_output_path(module: &ModuleInput, logical_path: &str) -> Option<String> {
    let module_dir = Path::new(module.semantic_path.as_str())
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let relative = output_relative_asset_path(logical_path)?;
    let mut output = module_dir.to_path_buf();
    output.push(relative);
    Some(path_to_forward_slashes(output.as_path()))
}

fn output_relative_asset_path(logical_path: &str) -> Option<PathBuf> {
    let logical = bun_root_relative_path(logical_path).unwrap_or(logical_path);
    let mut output = PathBuf::new();
    for component in Path::new(logical).components() {
        match component {
            Component::Normal(part) => output.push(part),
            Component::CurDir | Component::ParentDir => {}
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!output.as_os_str().is_empty()).then_some(output)
}

fn bun_root_relative_path(logical_path: &str) -> Option<&str> {
    logical_path
        .strip_prefix("/$bunfs/root/")
        .or_else(|| logical_path.strip_prefix("bun:/root/"))
}

fn common_source_root(source_files: &[SourceFileInput]) -> Option<PathBuf> {
    let mut parents = source_files
        .iter()
        .map(|source_file| Path::new(source_file.path.as_str()))
        .filter_map(Path::parent);
    let first = parents.next()?.to_path_buf();
    Some(parents.fold(first, common_path_prefix))
}

fn common_path_prefix(left: PathBuf, right: &Path) -> PathBuf {
    let left_components = left.components().collect::<Vec<_>>();
    let right_components = right.components().collect::<Vec<_>>();
    let mut output = PathBuf::new();
    for (left, right) in left_components.iter().zip(right_components.iter()) {
        if left != right {
            break;
        }
        output.push(left.as_os_str());
    }
    output
}

fn path_to_forward_slashes(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            Component::CurDir => None,
            Component::ParentDir => Some("..".to_string()),
            Component::RootDir | Component::Prefix(_) => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn infer_asset_kind(logical_path: &str) -> AssetKind {
    let extension = Path::new(logical_path)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("wasm") => AssetKind::Wasm,
        Some("node") => AssetKind::NativeNode,
        Some("exe") => AssetKind::Executable,
        Some("png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "avif" | "ico") => AssetKind::Image,
        Some("ttf" | "otf" | "woff" | "woff2") => AssetKind::Font,
        Some("css") => AssetKind::Css,
        Some("html" | "htm") => AssetKind::Html,
        _ if infer_asset_executable(logical_path) => AssetKind::Executable,
        _ => AssetKind::Data,
    }
}

fn infer_asset_executable(logical_path: &str) -> bool {
    Path::new(logical_path)
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .map(|name| matches!(name, "rg" | "rg.exe" | "ripgrep" | "ripgrep.exe"))
        .unwrap_or(false)
        || Path::new(logical_path)
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
}

fn extract_bun_embedded_asset(
    executable_path: &Path,
    logical_path: &str,
) -> Result<Option<Vec<u8>>, ExtractAssetsError> {
    let bytes = fs::read(executable_path).map_err(|source| ExtractAssetsError::ReadAsset {
        path: executable_path.to_path_buf(),
        source,
    })?;
    Ok(extract_bun_embedded_asset_from_bytes(
        bytes.as_slice(),
        logical_path,
    ))
}

fn extract_bun_embedded_asset_from_bytes(executable: &[u8], logical_path: &str) -> Option<Vec<u8>> {
    let needle = logical_path.as_bytes();
    if needle.is_empty() {
        return None;
    }
    let mut cursor = 0usize;
    while let Some(relative) = find_bytes(&executable[cursor..], needle) {
        let path_start = cursor + relative;
        let data_start = path_start.checked_add(needle.len())?.checked_add(1)?;
        if executable.get(path_start + needle.len()).copied() != Some(0) {
            cursor = path_start + 1;
            continue;
        }
        let payload = executable.get(data_start..)?;
        if let Some(size) = embedded_asset_payload_size(payload)
            && data_start.checked_add(size)? <= executable.len()
        {
            return Some(payload[..size].to_vec());
        }
        cursor = path_start + 1;
    }
    None
}

fn embedded_asset_payload_size(payload: &[u8]) -> Option<usize> {
    parse_elf_file_size(payload)
        .or_else(|| parse_wasm_file_size(payload))
        .filter(|size| *size > 0 && *size <= payload.len())
}

fn parse_elf_file_size(payload: &[u8]) -> Option<usize> {
    if payload.len() < 0x40 || &payload[..4] != b"\x7fELF" || payload.get(5).copied()? != 1 {
        return None;
    }
    match payload.get(4).copied()? {
        1 => parse_elf32_file_size(payload),
        2 => parse_elf64_file_size(payload),
        _ => None,
    }
}

fn parse_elf64_file_size(payload: &[u8]) -> Option<usize> {
    let phoff = read_u64(payload, 0x20)?;
    let shoff = read_u64(payload, 0x28)?;
    let ehsize = u64::from(read_u16(payload, 0x34)?);
    let phentsize = u64::from(read_u16(payload, 0x36)?);
    let phnum = u64::from(read_u16(payload, 0x38)?);
    let shentsize = u64::from(read_u16(payload, 0x3a)?);
    let shnum = u64::from(read_u16(payload, 0x3c)?);
    let mut size = ehsize;
    size = size.max(table_end(phoff, phentsize, phnum)?);
    size = size.max(table_end(shoff, shentsize, shnum)?);
    for index in 0..phnum {
        let header = phoff.checked_add(index.checked_mul(phentsize)?)?;
        let p_offset = read_u64(payload, usize::try_from(header.checked_add(0x08)?).ok()?)?;
        let p_filesz = read_u64(payload, usize::try_from(header.checked_add(0x20)?).ok()?)?;
        size = size.max(p_offset.checked_add(p_filesz)?);
    }
    usize::try_from(size).ok()
}

fn parse_elf32_file_size(payload: &[u8]) -> Option<usize> {
    if payload.len() < 0x34 {
        return None;
    }
    let phoff = u64::from(read_u32(payload, 0x1c)?);
    let shoff = u64::from(read_u32(payload, 0x20)?);
    let ehsize = u64::from(read_u16(payload, 0x28)?);
    let phentsize = u64::from(read_u16(payload, 0x2a)?);
    let phnum = u64::from(read_u16(payload, 0x2c)?);
    let shentsize = u64::from(read_u16(payload, 0x2e)?);
    let shnum = u64::from(read_u16(payload, 0x30)?);
    let mut size = ehsize;
    size = size.max(table_end(phoff, phentsize, phnum)?);
    size = size.max(table_end(shoff, shentsize, shnum)?);
    for index in 0..phnum {
        let header = phoff.checked_add(index.checked_mul(phentsize)?)?;
        let p_offset = u64::from(read_u32(
            payload,
            usize::try_from(header.checked_add(0x04)?).ok()?,
        )?);
        let p_filesz = u64::from(read_u32(
            payload,
            usize::try_from(header.checked_add(0x10)?).ok()?,
        )?);
        size = size.max(p_offset.checked_add(p_filesz)?);
    }
    usize::try_from(size).ok()
}

fn parse_wasm_file_size(payload: &[u8]) -> Option<usize> {
    if payload.len() < 8 || &payload[..4] != b"\0asm" {
        return None;
    }
    let mut cursor = 8usize;
    let mut last_non_custom_section = 0u8;
    while cursor < payload.len() {
        let section_start = cursor;
        let section_id = *payload.get(cursor)?;
        if section_id > 12 {
            return Some(section_start);
        }
        cursor = cursor.checked_add(1)?;
        let Some((section_len, next)) = read_leb128_usize(payload, cursor) else {
            return Some(section_start);
        };
        if section_id != 0 {
            if section_id <= last_non_custom_section {
                return Some(section_start);
            }
            last_non_custom_section = section_id;
        }
        let Some(next_cursor) = next.checked_add(section_len) else {
            return Some(section_start);
        };
        if next_cursor > payload.len() {
            return Some(section_start);
        }
        cursor = next_cursor;
    }
    Some(cursor)
}

fn read_leb128_usize(payload: &[u8], mut cursor: usize) -> Option<(usize, usize)> {
    let mut value = 0usize;
    let mut shift = 0usize;
    loop {
        let byte = *payload.get(cursor)?;
        cursor += 1;
        value |= usize::from(byte & 0x7f).checked_shl(u32::try_from(shift).ok()?)?;
        if byte & 0x80 == 0 {
            return Some((value, cursor));
        }
        shift = shift.checked_add(7)?;
        if shift >= usize::BITS as usize {
            return None;
        }
    }
}

fn table_end(offset: u64, entry_size: u64, count: u64) -> Option<u64> {
    if offset == 0 || entry_size == 0 || count == 0 {
        return Some(0);
    }
    offset.checked_add(entry_size.checked_mul(count)?)
}

fn read_u16(payload: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        payload
            .get(offset..offset.checked_add(2)?)?
            .try_into()
            .ok()?,
    ))
}

fn read_u32(payload: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        payload
            .get(offset..offset.checked_add(4)?)?
            .try_into()
            .ok()?,
    ))
}

fn read_u64(payload: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        payload
            .get(offset..offset.checked_add(8)?)?
            .try_into()
            .ok()?,
    ))
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn materialized_asset_root(database_path: &Path, project_id: u32) -> PathBuf {
    let database_dir = database_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    database_dir
        .join("project-assets")
        .join(project_id.to_string())
}

fn persist_project_assets(
    connection: &mut Connection,
    project_id: u32,
    assets: &[DiscoveredProjectAsset],
    materialized_root: &Path,
) -> Result<usize, ExtractAssetsError> {
    if assets.is_empty() {
        return Ok(0);
    }
    ensure_project_assets_table(connection)?;
    let transaction = connection
        .transaction()
        .map_err(ExtractAssetsError::WriteAsset)?;
    let mut written = 0;
    for asset in assets {
        persist_project_asset(&transaction, project_id, asset, materialized_root)?;
        written += 1;
    }
    transaction
        .commit()
        .map_err(ExtractAssetsError::WriteAsset)?;
    Ok(written)
}

fn ensure_project_assets_table(connection: &Connection) -> Result<(), ExtractAssetsError> {
    connection
        .execute_batch(
            r"
            CREATE TABLE IF NOT EXISTS project_assets (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_id INTEGER NOT NULL,
                logical_path TEXT NOT NULL,
                output_path TEXT NOT NULL,
                source_path TEXT NOT NULL,
                kind TEXT NOT NULL,
                executable INTEGER NOT NULL DEFAULT 0,
                platform TEXT,
                arch TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE (project_id, logical_path),
                UNIQUE (project_id, output_path)
            );
            ",
        )
        .map_err(ExtractAssetsError::WriteAsset)
}

fn persist_project_asset(
    connection: &Connection,
    project_id: u32,
    asset: &DiscoveredProjectAsset,
    materialized_root: &Path,
) -> Result<(), ExtractAssetsError> {
    let source_path = materialize_project_asset_source(asset, materialized_root)?;
    connection
        .execute(
            "DELETE FROM project_assets WHERE project_id = ?1 AND logical_path = ?2",
            params![i64::from(project_id), asset.reference.logical_path.as_str()],
        )
        .map_err(ExtractAssetsError::WriteAsset)?;
    connection
        .execute(
            r"
            INSERT INTO project_assets
                (project_id, logical_path, output_path, source_path, kind, executable,
                 platform, arch, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, datetime('now'), datetime('now'))
            ",
            params![
                i64::from(project_id),
                asset.reference.logical_path.as_str(),
                asset.output_path.as_str(),
                source_path.to_string_lossy().as_ref(),
                asset.kind.as_str(),
                if asset.executable { 1_i64 } else { 0_i64 },
            ],
        )
        .map_err(ExtractAssetsError::WriteAsset)?;
    Ok(())
}

fn materialize_project_asset_source(
    asset: &DiscoveredProjectAsset,
    materialized_root: &Path,
) -> Result<PathBuf, ExtractAssetsError> {
    match &asset.source {
        DiscoveredAssetSource::File(path) => Ok(path.clone()),
        DiscoveredAssetSource::EmbeddedBunFile { bytes, .. } => {
            let relative = output_relative_asset_path(asset.reference.logical_path.as_str())
                .ok_or_else(|| ExtractAssetsError::InvalidAssetPath {
                    logical_path: asset.reference.logical_path.clone(),
                })?;
            let path = materialized_root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|source| {
                    ExtractAssetsError::WriteMaterializedAsset {
                        path: parent.to_path_buf(),
                        source,
                    }
                })?;
            }
            fs::write(path.as_path(), bytes).map_err(|source| {
                ExtractAssetsError::WriteMaterializedAsset {
                    path: path.clone(),
                    source,
                }
            })?;
            set_materialized_executable_bit(path.as_path(), asset.executable).map_err(
                |source| ExtractAssetsError::WriteMaterializedAsset {
                    path: path.clone(),
                    source,
                },
            )?;
            Ok(path)
        }
    }
}

#[cfg(unix)]
fn set_materialized_executable_bit(path: &Path, executable: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if !executable {
        return Ok(());
    }
    let metadata = fs::metadata(path)?;
    let mut permissions = metadata.permissions();
    permissions.set_mode(permissions.mode() | 0o755);
    fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn set_materialized_executable_bit(_path: &Path, _executable: bool) -> io::Result<()> {
    Ok(())
}

fn package_source_load_scope(
    rows: &InputRows,
    requested_package_names: &[String],
    audit: &mut AuditReport,
) -> BTreeSet<String> {
    if !requested_package_names.is_empty() {
        return package_graph_component_scope(rows, requested_package_names);
    }

    let mut package_names = rows
        .modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
        .filter(|module| !has_accepted_external_attribution(rows, module.id))
        .filter_map(|module| module.package_name.clone())
        .collect::<BTreeSet<_>>();
    match package_import_names_from_sources(rows) {
        Ok(source_import_packages) => package_names.extend(source_import_packages),
        Err(source) => {
            audit.push(
                AuditFinding::error(
                    FindingCode::AstFactExtractionFailed,
                    format!(
                        "failed to parse source-backed package import sites: {}",
                        source.source
                    ),
                )
                .with_module(source.source_file_path),
            );
        }
    }
    package_names
}

fn package_graph_component_scope(
    rows: &InputRows,
    requested_package_names: &[String],
) -> BTreeSet<String> {
    let requested = requested_package_names
        .iter()
        .map(|package_name| package_name.trim())
        .filter(|package_name| !package_name.is_empty())
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>();
    if requested.is_empty() {
        return BTreeSet::new();
    }

    let package_by_module = rows
        .modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
        .filter_map(|module| {
            module
                .package_name
                .as_deref()
                .map(str::trim)
                .filter(|package_name| !package_name.is_empty())
                .map(|package_name| (module.id, package_name.to_string()))
        })
        .collect::<BTreeMap<_, _>>();
    if package_by_module.is_empty() {
        return requested;
    }

    let mut neighbors = BTreeMap::<String, BTreeSet<String>>::new();
    for package_name in package_by_module.values() {
        neighbors.entry(package_name.clone()).or_default();
    }
    for dependency in &rows.dependencies {
        let ModuleDependencyTarget::Module(target_id) = dependency.target else {
            continue;
        };
        let Some(from_package) = package_by_module.get(&dependency.from_module_id) else {
            continue;
        };
        let Some(to_package) = package_by_module.get(&target_id) else {
            continue;
        };
        if from_package == to_package {
            continue;
        }
        neighbors
            .entry(from_package.clone())
            .or_default()
            .insert(to_package.clone());
        neighbors
            .entry(to_package.clone())
            .or_default()
            .insert(from_package.clone());
    }

    let mut scope = requested.clone();
    let mut stack = requested.into_iter().collect::<Vec<_>>();
    while let Some(package_name) = stack.pop() {
        for neighbor in neighbors
            .get(package_name.as_str())
            .into_iter()
            .flatten()
            .cloned()
        {
            if scope.insert(neighbor.clone()) {
                stack.push(neighbor);
            }
        }
    }
    scope
}

fn load_package_sources(
    connection: &mut Connection,
    rows: &InputRows,
    package_names: &BTreeSet<String>,
    package_source_roots: &[PathBuf],
    materialize_package_sources: bool,
    persist_materialized_package_sources: bool,
) -> Result<Vec<PackageSource>, MatchPackagesError> {
    if package_names.is_empty() {
        return Ok(Vec::new());
    }

    let has_package_source_cache = sqlite_table_exists(connection, "package_source_cache")
        .map_err(MatchPackagesError::QueryPackageSources)?;
    if !has_package_source_cache && package_source_roots.is_empty() && !materialize_package_sources
    {
        return Err(MatchPackagesError::MissingTable("package_source_cache"));
    }

    let stale_cache_versions = if has_package_source_cache && materialize_package_sources {
        stale_package_source_cache_versions(connection, package_names)?
    } else {
        BTreeSet::new()
    };

    let mut package_sources = if has_package_source_cache {
        let has_external_importable_column =
            sqlite_table_has_column(connection, "package_source_cache", "external_importable")
                .map_err(MatchPackagesError::QueryPackageSources)?;
        let has_external_import_policy_version_column = sqlite_table_has_column(
            connection,
            "package_source_cache",
            "external_import_policy_version",
        )
        .map_err(MatchPackagesError::QueryPackageSources)?;
        let has_export_specifier_column =
            sqlite_table_has_column(connection, "package_source_cache", "export_specifier")
                .map_err(MatchPackagesError::QueryPackageSources)?;
        let current_import_policy_predicate = if has_external_import_policy_version_column
            && has_export_specifier_column
        {
            format!(
                "external_import_policy_version = {PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION}"
            )
        } else {
            "0".to_string()
        };
        let cache_import_policy_predicate = if materialize_package_sources {
            current_import_policy_predicate
        } else {
            "1".to_string()
        };
        let external_importable_expr = if has_external_importable_column {
            if has_external_import_policy_version_column && has_export_specifier_column {
                format!(
                    "CASE WHEN external_import_policy_version = {PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION} THEN external_importable ELSE 0 END"
                )
            } else {
                "0".to_string()
            }
        } else {
            "0".to_string()
        };
        let export_specifier_expr = if has_external_import_policy_version_column
            && has_export_specifier_column
        {
            format!(
                "CASE WHEN external_import_policy_version = {PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION} AND TRIM(COALESCE(export_specifier, '')) != '' THEN export_specifier ELSE '' END"
            )
        } else {
            "''".to_string()
        };
        let mut sql = String::from(
            format!(
                r"
            SELECT package_name, package_version, entry_path, source_content,
                   {external_importable_expr},
                   {export_specifier_expr}
              FROM package_source_cache
             WHERE TRIM(COALESCE(package_name, '')) != ''
               AND TRIM(COALESCE(package_version, '')) != ''
               AND TRIM(COALESCE(entry_path, '')) != ''
               AND TRIM(COALESCE(source_content, '')) != ''
               AND ({cache_import_policy_predicate})
            "
            )
            .as_str(),
        );
        if !package_names.is_empty() {
            use std::fmt::Write as _;
            let _ = write!(
                sql,
                " AND package_name IN ({})",
                sqlite_placeholders(package_names.len())
            );
        }
        sql.push_str(" ORDER BY package_name, package_version, entry_path");

        let mut statement = connection
            .prepare(sql.as_str())
            .map_err(MatchPackagesError::QueryPackageSources)?;
        if package_names.is_empty() {
            let rows = statement
                .query_map([], package_source_from_row)
                .map_err(MatchPackagesError::QueryPackageSources)?;
            collect_sqlite_rows(rows).map_err(MatchPackagesError::QueryPackageSources)?
        } else {
            let rows = statement
                .query_map(
                    params_from_iter(package_names.iter()),
                    package_source_from_row,
                )
                .map_err(MatchPackagesError::QueryPackageSources)?;
            collect_sqlite_rows(rows).map_err(MatchPackagesError::QueryPackageSources)?
        }
    } else {
        Vec::new()
    };
    package_sources.extend(load_package_sources_from_roots(
        package_names,
        package_source_roots,
    )?);
    if materialize_package_sources {
        let materialized_sources = materialize_package_sources_from_hints(
            rows,
            package_names,
            &package_sources,
            &stale_cache_versions,
        )?;
        if persist_materialized_package_sources && !materialized_sources.is_empty() {
            persist_package_source_cache(connection, &materialized_sources)?;
        }
        package_sources.extend(materialized_sources);
    }
    promote_package_sources_with_externalization_hints(
        connection,
        package_names,
        &mut package_sources,
    )?;
    filter_package_sources_to_best_build_variants(rows, &mut package_sources);
    filter_package_sources_to_relevant_path_hints(rows, &mut package_sources);
    dedup_package_sources(&mut package_sources);
    Ok(package_sources)
}

fn stale_package_source_cache_versions(
    connection: &Connection,
    package_names: &BTreeSet<String>,
) -> Result<BTreeSet<(String, String)>, MatchPackagesError> {
    let has_external_import_policy_version_column = sqlite_table_has_column(
        connection,
        "package_source_cache",
        "external_import_policy_version",
    )
    .map_err(MatchPackagesError::QueryPackageSources)?;
    let has_export_specifier_column =
        sqlite_table_has_column(connection, "package_source_cache", "export_specifier")
            .map_err(MatchPackagesError::QueryPackageSources)?;
    let stale_predicate = if has_external_import_policy_version_column
        && has_export_specifier_column
    {
        format!(
            "external_import_policy_version != {PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION} OR TRIM(COALESCE(export_specifier, '')) = ''"
        )
    } else {
        "1".to_string()
    };
    let mut sql = format!(
        r"
        SELECT DISTINCT package_name, package_version
          FROM package_source_cache
         WHERE TRIM(COALESCE(package_name, '')) != ''
           AND TRIM(COALESCE(package_version, '')) != ''
           AND TRIM(COALESCE(source_content, '')) != ''
           AND ({stale_predicate})
        "
    );
    if !package_names.is_empty() {
        use std::fmt::Write as _;
        let _ = write!(
            sql,
            " AND package_name IN ({})",
            sqlite_placeholders(package_names.len())
        );
    }
    let mut statement = connection
        .prepare(sql.as_str())
        .map_err(MatchPackagesError::QueryPackageSources)?;
    let rows = if package_names.is_empty() {
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(MatchPackagesError::QueryPackageSources)?
            .collect::<rusqlite::Result<Vec<(String, String)>>>()
    } else {
        statement
            .query_map(params_from_iter(package_names.iter()), |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .map_err(MatchPackagesError::QueryPackageSources)?
            .collect::<rusqlite::Result<Vec<(String, String)>>>()
    }
    .map_err(MatchPackagesError::QueryPackageSources)?;
    Ok(rows.into_iter().collect())
}

fn package_source_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PackageSource> {
    let package_name = row.get::<_, String>(0)?;
    let package_version = row.get::<_, String>(1)?;
    let entry_path = row.get::<_, String>(2)?;
    let source = row.get::<_, String>(3)?;
    let external_importable = row.get::<_, i64>(4)? != 0;
    let cached_export_specifier = row.get::<_, String>(5)?;
    let export_specifier = if cached_export_specifier.trim().is_empty() {
        package_export_specifier(package_name.as_str(), entry_path.as_str())
    } else {
        cached_export_specifier.trim().to_string()
    };
    let source_path = format!("{package_name}@{package_version}/{entry_path}");
    if external_importable {
        Ok(PackageSource::external(
            package_name,
            package_version,
            export_specifier,
            source_path,
            source,
        ))
    } else {
        Ok(PackageSource::source_only(
            package_name,
            package_version,
            export_specifier,
            source_path,
            source,
        ))
    }
}

const PACKAGE_EXTERNALIZATION_HINT_POLICY_VERSION: i64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PackageExternalizationHint {
    package_name: String,
    package_version: String,
    entry_path: String,
    export_specifier: String,
    content_hash: Option<String>,
    normalized_source_hash: Option<String>,
    public_members: BTreeSet<String>,
    proof_policy_version: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExternalizationHintCandidate {
    package_name: String,
    package_version: String,
    entry_path: String,
    source_content: String,
    content_hash: String,
    export_specifier: String,
    external_importable: bool,
}

fn externalization_hint_candidates_from_cache(
    connection: &Connection,
    package_names: &BTreeSet<String>,
    limit: Option<u32>,
) -> Result<Vec<ExternalizationHintCandidate>, MatchPackagesError> {
    for required in [
        "external_importable",
        "external_import_policy_version",
        "export_specifier",
    ] {
        if !sqlite_table_has_column(connection, "package_source_cache", required)
            .map_err(MatchPackagesError::QueryPackageSources)?
        {
            return Ok(Vec::new());
        }
    }

    let mut sql = format!(
        r"
        SELECT package_name, package_version, entry_path, source_content,
               content_hash, export_specifier, external_importable
          FROM package_source_cache
         WHERE TRIM(COALESCE(package_name, '')) != ''
           AND TRIM(COALESCE(package_version, '')) != ''
           AND TRIM(COALESCE(entry_path, '')) != ''
           AND TRIM(COALESCE(source_content, '')) != ''
           AND TRIM(COALESCE(content_hash, '')) != ''
           AND TRIM(COALESCE(export_specifier, '')) != ''
           AND external_import_policy_version = {PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION}
        "
    );
    if !package_names.is_empty() {
        use std::fmt::Write as _;
        let _ = write!(
            sql,
            " AND package_name IN ({})",
            sqlite_placeholders(package_names.len())
        );
    }
    sql.push_str(" ORDER BY package_name, package_version, entry_path");
    if let Some(limit) = limit {
        use std::fmt::Write as _;
        let _ = write!(sql, " LIMIT {limit}");
    }

    let mut statement = connection
        .prepare(sql.as_str())
        .map_err(MatchPackagesError::QueryPackageSources)?;
    let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<ExternalizationHintCandidate> {
        Ok(ExternalizationHintCandidate {
            package_name: row.get::<_, String>(0)?.trim().to_string(),
            package_version: row.get::<_, String>(1)?.trim().to_string(),
            entry_path: clean_package_entry_path(row.get::<_, String>(2)?.as_str()),
            source_content: row.get(3)?,
            content_hash: row.get::<_, String>(4)?.trim().to_string(),
            export_specifier: row.get::<_, String>(5)?.trim().to_string(),
            external_importable: row.get::<_, i64>(6)? != 0,
        })
    };
    if package_names.is_empty() {
        let rows = statement
            .query_map([], map_row)
            .map_err(MatchPackagesError::QueryPackageSources)?;
        collect_sqlite_rows(rows).map_err(MatchPackagesError::QueryPackageSources)
    } else {
        let rows = statement
            .query_map(params_from_iter(package_names.iter()), map_row)
            .map_err(MatchPackagesError::QueryPackageSources)?;
        collect_sqlite_rows(rows).map_err(MatchPackagesError::QueryPackageSources)
    }
}

fn promote_package_sources_with_externalization_hints(
    connection: &Connection,
    package_names: &BTreeSet<String>,
    package_sources: &mut Vec<PackageSource>,
) -> Result<usize, MatchPackagesError> {
    let hints = load_package_externalization_hints(connection, package_names)?;
    if hints.is_empty() || package_sources.is_empty() {
        return Ok(0);
    }
    let mut promoted = Vec::new();
    for hint in hints {
        if hint
            .proof_policy_version
            .is_some_and(|version| version != PACKAGE_EXTERNALIZATION_HINT_POLICY_VERSION)
        {
            continue;
        }
        if !hint_export_specifier_matches_package(
            hint.package_name.as_str(),
            hint.export_specifier.as_str(),
        ) {
            continue;
        }
        if hint.content_hash.is_none()
            && hint.normalized_source_hash.is_none()
            && hint.public_members.is_empty()
        {
            continue;
        }
        let Some(source) = package_sources.iter().find(|source| {
            source.package_name == hint.package_name
                && source.package_version == hint.package_version
                && package_source_entry_path(source) == hint.entry_path
        }) else {
            continue;
        };
        if source.external_importable {
            continue;
        }
        let direct_specifier_match = source.export_specifier == hint.export_specifier;
        if !direct_specifier_match && hint.public_members.is_empty() {
            continue;
        }
        if let Some(content_hash) = hint.content_hash.as_deref()
            && stable_hash(source.source.as_bytes()) != content_hash
        {
            continue;
        }
        if let Some(normalized_source_hash) = hint.normalized_source_hash.as_deref()
            && package_source_normalized_hash(source.source_path.as_str(), source.source.as_str())
                .as_deref()
                != Some(normalized_source_hash)
        {
            continue;
        }
        if !hint.public_members.is_empty() {
            let exported_members = package_source_exported_members(
                source.source_path.as_str(),
                source.source.as_str(),
            );
            if !hint.public_members.is_subset(&exported_members) {
                continue;
            }
        }
        promoted.push(PackageSource::external(
            source.package_name.as_str(),
            source.package_version.as_str(),
            hint.export_specifier.as_str(),
            source.source_path.as_str(),
            source.source.as_str(),
        ));
    }
    let promoted_len = promoted.len();
    package_sources.extend(promoted);
    Ok(promoted_len)
}

fn load_package_externalization_hints(
    connection: &Connection,
    package_names: &BTreeSet<String>,
) -> Result<Vec<PackageExternalizationHint>, MatchPackagesError> {
    if !sqlite_table_exists(connection, "package_externalization_hints")
        .map_err(MatchPackagesError::QueryPackageSources)?
    {
        return Ok(Vec::new());
    }
    let has_content_hash =
        sqlite_table_has_column(connection, "package_externalization_hints", "content_hash")
            .map_err(MatchPackagesError::QueryPackageSources)?;
    let has_normalized_source_hash = sqlite_table_has_column(
        connection,
        "package_externalization_hints",
        "normalized_source_hash",
    )
    .map_err(MatchPackagesError::QueryPackageSources)?;
    let has_public_members = sqlite_table_has_column(
        connection,
        "package_externalization_hints",
        "public_members_json",
    )
    .map_err(MatchPackagesError::QueryPackageSources)?;
    let has_policy_version = sqlite_table_has_column(
        connection,
        "package_externalization_hints",
        "proof_policy_version",
    )
    .map_err(MatchPackagesError::QueryPackageSources)?;
    for required in [
        "package_name",
        "package_version",
        "entry_path",
        "export_specifier",
    ] {
        if !sqlite_table_has_column(connection, "package_externalization_hints", required)
            .map_err(MatchPackagesError::QueryPackageSources)?
        {
            return Ok(Vec::new());
        }
    }
    let content_hash_expr = if has_content_hash {
        "content_hash"
    } else {
        "NULL"
    };
    let normalized_hash_expr = if has_normalized_source_hash {
        "normalized_source_hash"
    } else {
        "NULL"
    };
    let public_members_expr = if has_public_members {
        "public_members_json"
    } else {
        "NULL"
    };
    let policy_version_expr = if has_policy_version {
        "proof_policy_version"
    } else {
        "NULL"
    };
    let mut sql = format!(
        r"
        SELECT package_name, package_version, entry_path, export_specifier,
               {content_hash_expr}, {normalized_hash_expr},
               {public_members_expr}, {policy_version_expr}
          FROM package_externalization_hints
         WHERE TRIM(COALESCE(package_name, '')) != ''
           AND TRIM(COALESCE(package_version, '')) != ''
           AND TRIM(COALESCE(entry_path, '')) != ''
           AND TRIM(COALESCE(export_specifier, '')) != ''
        "
    );
    if !package_names.is_empty() {
        use std::fmt::Write as _;
        let _ = write!(
            sql,
            " AND package_name IN ({})",
            sqlite_placeholders(package_names.len())
        );
    }
    let mut statement = connection
        .prepare(sql.as_str())
        .map_err(MatchPackagesError::QueryPackageSources)?;
    let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<PackageExternalizationHint> {
        let package_name = row.get::<_, String>(0)?.trim().to_string();
        let package_version = row.get::<_, String>(1)?.trim().to_string();
        let raw_entry_path = row.get::<_, String>(2)?;
        Ok(PackageExternalizationHint {
            package_name: package_name.clone(),
            package_version: package_version.clone(),
            entry_path: clean_package_entry_path(
                hint_entry_path(
                    package_name.as_str(),
                    package_version.as_str(),
                    raw_entry_path.as_str(),
                )
                .trim()
                .trim_matches('/'),
            ),
            export_specifier: row.get::<_, String>(3)?.trim().to_string(),
            content_hash: row
                .get::<_, Option<String>>(4)?
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            normalized_source_hash: row
                .get::<_, Option<String>>(5)?
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            public_members: parse_public_members_hint(row.get::<_, Option<String>>(6)?.as_deref()),
            proof_policy_version: row.get::<_, Option<i64>>(7)?,
        })
    };
    if package_names.is_empty() {
        let rows = statement
            .query_map([], map_row)
            .map_err(MatchPackagesError::QueryPackageSources)?;
        collect_sqlite_rows(rows).map_err(MatchPackagesError::QueryPackageSources)
    } else {
        let rows = statement
            .query_map(params_from_iter(package_names.iter()), map_row)
            .map_err(MatchPackagesError::QueryPackageSources)?;
        collect_sqlite_rows(rows).map_err(MatchPackagesError::QueryPackageSources)
    }
}

fn hint_entry_path<'a>(package_name: &str, package_version: &str, entry_path: &'a str) -> &'a str {
    let clean = entry_path.trim();
    clean
        .strip_prefix(format!("{package_name}@{package_version}/").as_str())
        .unwrap_or(clean)
}

fn parse_public_members_hint(value: Option<&str>) -> BTreeSet<String> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return BTreeSet::new();
    };
    serde_json::from_str::<Vec<String>>(value)
        .unwrap_or_default()
        .into_iter()
        .map(|member| member.trim().to_string())
        .filter(|member| !member.is_empty())
        .collect()
}

fn persist_package_externalization_hints(
    connection: &mut Connection,
    hints: &[PackageExternalizationHint],
) -> Result<usize, MatchPackagesError> {
    ensure_package_externalization_hints_table(connection)?;
    let transaction = connection
        .transaction()
        .map_err(MatchPackagesError::WritePackageExternalizationHints)?;
    let mut written = 0usize;
    for hint in hints {
        let public_members_json = serde_json::to_string(
            &hint.public_members.iter().collect::<Vec<_>>(),
        )
        .map_err(|source| {
            MatchPackagesError::WritePackageExternalizationHints(
                rusqlite::Error::ToSqlConversionFailure(Box::new(source)),
            )
        })?;
        written += transaction
            .execute(
                r"
                INSERT INTO package_externalization_hints
                    (package_name, package_version, entry_path, export_specifier,
                     content_hash, normalized_source_hash, public_members_json,
                     proof_policy_version, created_at, updated_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, datetime('now'), datetime('now'))
                ON CONFLICT(package_name, package_version, entry_path, export_specifier)
                DO UPDATE SET
                    content_hash = excluded.content_hash,
                    normalized_source_hash = excluded.normalized_source_hash,
                    public_members_json = excluded.public_members_json,
                    proof_policy_version = excluded.proof_policy_version,
                    updated_at = datetime('now')
                ",
                params![
                    hint.package_name.as_str(),
                    hint.package_version.as_str(),
                    hint.entry_path.as_str(),
                    hint.export_specifier.as_str(),
                    hint.content_hash.as_deref(),
                    hint.normalized_source_hash.as_deref(),
                    public_members_json.as_str(),
                    hint.proof_policy_version
                        .unwrap_or(PACKAGE_EXTERNALIZATION_HINT_POLICY_VERSION),
                ],
            )
            .map_err(MatchPackagesError::WritePackageExternalizationHints)?;
    }
    transaction
        .commit()
        .map_err(MatchPackagesError::WritePackageExternalizationHints)?;
    Ok(written)
}

fn ensure_package_externalization_hints_table(
    connection: &Connection,
) -> Result<(), MatchPackagesError> {
    connection
        .execute_batch(PACKAGE_EXTERNALIZATION_HINTS_CREATE_SQL)
        .map_err(MatchPackagesError::WritePackageExternalizationHints)
}

const PACKAGE_EXTERNALIZATION_HINTS_CREATE_SQL: &str = r"
CREATE TABLE IF NOT EXISTS package_externalization_hints (
    package_name TEXT NOT NULL,
    package_version TEXT NOT NULL,
    entry_path TEXT NOT NULL,
    export_specifier TEXT NOT NULL,
    content_hash TEXT,
    normalized_source_hash TEXT,
    public_members_json TEXT NOT NULL DEFAULT '[]',
    proof_policy_version INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (package_name, package_version, entry_path, export_specifier),
    CHECK (TRIM(package_name) != ''),
    CHECK (TRIM(package_version) != ''),
    CHECK (TRIM(entry_path) != ''),
    CHECK (TRIM(export_specifier) != ''),
    CHECK (content_hash IS NULL OR TRIM(content_hash) != ''),
    CHECK (normalized_source_hash IS NULL OR TRIM(normalized_source_hash) != ''),
    CHECK (
        content_hash IS NOT NULL
        OR normalized_source_hash IS NOT NULL
        OR TRIM(public_members_json) NOT IN ('', '[]')
    )
);
CREATE INDEX IF NOT EXISTS idx_package_externalization_hints_package
    ON package_externalization_hints(package_name, package_version);
";

fn hint_export_specifier_matches_package(package_name: &str, export_specifier: &str) -> bool {
    split_bare_specifier(export_specifier).is_some_and(|(specifier_package, _subpath)| {
        specifier_package == package_name && !is_node_builtin(specifier_package.as_str())
    })
}

fn filter_package_sources_to_referenced_package_versions(
    rows: &InputRows,
    package_sources: &mut Vec<PackageSource>,
) -> usize {
    let referenced_versions = rows
        .modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
        .filter_map(|module| {
            let package_name = module.package_name.as_deref()?.trim();
            let package_version = module
                .package_version
                .as_deref()
                .map(str::trim)
                .filter(|version| Version::parse(version).is_ok())?;
            Some((package_name.to_string(), package_version.to_string()))
        })
        .fold(
            BTreeMap::<String, BTreeSet<String>>::new(),
            |mut versions, (package_name, package_version)| {
                versions
                    .entry(package_name)
                    .or_default()
                    .insert(package_version);
                versions
            },
        );
    if referenced_versions.is_empty() {
        return 0;
    }

    let before = package_sources.len();
    package_sources.retain(|source| {
        referenced_versions
            .get(source.package_name.as_str())
            .is_none_or(|versions| versions.contains(source.package_version.as_str()))
    });
    before.saturating_sub(package_sources.len())
}

fn persist_package_source_cache(
    connection: &mut Connection,
    package_sources: &[PackageSource],
) -> Result<usize, MatchPackagesError> {
    ensure_package_source_cache_table(connection)?;
    let transaction = connection
        .transaction()
        .map_err(MatchPackagesError::WritePackageSourceCache)?;
    let mut written = 0;
    for source in package_sources {
        let entry_path = package_source_cache_entry_path(source);
        let content_hash = stable_hash(source.source.as_bytes());
        written += transaction
            .execute(
                r"
                INSERT INTO package_source_cache
                    (package_name, package_version, entry_path, source_content,
                     content_hash, external_importable, external_import_policy_version,
                     export_specifier, fetched_at, expires_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, datetime('now'), datetime('now', '+30 days'))
                ON CONFLICT(package_name, package_version, entry_path) DO UPDATE SET
                    source_content = excluded.source_content,
                    content_hash = excluded.content_hash,
                    external_importable = excluded.external_importable,
                    external_import_policy_version = excluded.external_import_policy_version,
                    export_specifier = excluded.export_specifier,
                    fetched_at = excluded.fetched_at,
                    expires_at = excluded.expires_at
                ",
                params![
                    source.package_name.as_str(),
                    source.package_version.as_str(),
                    entry_path.as_str(),
                    source.source.as_str(),
                    content_hash.as_str(),
                    if source.external_importable {
                        1_i64
                    } else {
                        0_i64
                    },
                    PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION,
                    source.export_specifier.as_str(),
                ],
            )
            .map_err(MatchPackagesError::WritePackageSourceCache)?;
    }
    transaction
        .commit()
        .map_err(MatchPackagesError::WritePackageSourceCache)?;
    Ok(written)
}

fn ensure_package_source_cache_table(
    connection: &mut Connection,
) -> Result<(), MatchPackagesError> {
    connection
        .execute_batch(PACKAGE_SOURCE_CACHE_CREATE_SQL)
        .map_err(MatchPackagesError::WritePackageSourceCache)?;
    if package_source_cache_needs_entry_path_pk_migration(connection)
        .map_err(MatchPackagesError::WritePackageSourceCache)?
    {
        migrate_package_source_cache_entry_path_primary_key(connection)?;
    }
    if !sqlite_table_has_column(connection, "package_source_cache", "external_importable")
        .map_err(MatchPackagesError::WritePackageSourceCache)?
    {
        connection
            .execute_batch(
                r"
                ALTER TABLE package_source_cache
                    ADD COLUMN external_importable INTEGER NOT NULL DEFAULT 1;
                ",
            )
            .map_err(MatchPackagesError::WritePackageSourceCache)?;
    }
    if !sqlite_table_has_column(
        connection,
        "package_source_cache",
        "external_import_policy_version",
    )
    .map_err(MatchPackagesError::WritePackageSourceCache)?
    {
        connection
            .execute_batch(
                r"
                ALTER TABLE package_source_cache
                    ADD COLUMN external_import_policy_version INTEGER NOT NULL DEFAULT 0;
                ",
            )
            .map_err(MatchPackagesError::WritePackageSourceCache)?;
    }
    if !sqlite_table_has_column(connection, "package_source_cache", "export_specifier")
        .map_err(MatchPackagesError::WritePackageSourceCache)?
    {
        connection
            .execute_batch(
                r"
                ALTER TABLE package_source_cache
                    ADD COLUMN export_specifier TEXT NOT NULL DEFAULT '';
                ",
            )
            .map_err(MatchPackagesError::WritePackageSourceCache)?;
    }
    connection
        .execute_batch(PACKAGE_SOURCE_CACHE_INDEX_SQL)
        .map_err(MatchPackagesError::WritePackageSourceCache)?;
    Ok(())
}

const PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION: i64 = 4;

const PACKAGE_SOURCE_CACHE_CREATE_SQL: &str = r"
CREATE TABLE IF NOT EXISTS package_source_cache (
    package_name TEXT NOT NULL,
    package_version TEXT NOT NULL,
    entry_path TEXT NOT NULL,
    source_content TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    external_importable INTEGER NOT NULL DEFAULT 1,
    external_import_policy_version INTEGER NOT NULL DEFAULT 0,
    export_specifier TEXT NOT NULL DEFAULT '',
    fetched_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    PRIMARY KEY (package_name, package_version, entry_path)
);
";

const PACKAGE_SOURCE_CACHE_INDEX_SQL: &str = r"
CREATE INDEX IF NOT EXISTS idx_package_cache_expires
    ON package_source_cache(expires_at);
";

fn package_source_cache_needs_entry_path_pk_migration(
    connection: &Connection,
) -> rusqlite::Result<bool> {
    let mut statement = connection.prepare("PRAGMA table_info(package_source_cache)")?;
    let columns = statement.query_map([], |row| {
        Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
    })?;
    let mut primary_key_columns = BTreeMap::<i64, String>::new();
    for column in columns {
        let (name, primary_key_ordinal) = column?;
        if primary_key_ordinal > 0 {
            primary_key_columns.insert(primary_key_ordinal, name);
        }
    }
    let ordered_primary_key = primary_key_columns.into_values().collect::<Vec<_>>();
    Ok(ordered_primary_key
        != vec![
            "package_name".to_string(),
            "package_version".to_string(),
            "entry_path".to_string(),
        ])
}

fn migrate_package_source_cache_entry_path_primary_key(
    connection: &mut Connection,
) -> Result<(), MatchPackagesError> {
    let has_external_importable =
        sqlite_table_has_column(connection, "package_source_cache", "external_importable")
            .map_err(MatchPackagesError::WritePackageSourceCache)?;
    let external_importable_expr = if has_external_importable {
        "external_importable"
    } else {
        "1"
    };
    let has_external_import_policy_version = sqlite_table_has_column(
        connection,
        "package_source_cache",
        "external_import_policy_version",
    )
    .map_err(MatchPackagesError::WritePackageSourceCache)?;
    let external_import_policy_version_expr = if has_external_import_policy_version {
        "external_import_policy_version"
    } else {
        "0"
    };
    let has_export_specifier =
        sqlite_table_has_column(connection, "package_source_cache", "export_specifier")
            .map_err(MatchPackagesError::WritePackageSourceCache)?;
    let export_specifier_expr = if has_export_specifier {
        "export_specifier"
    } else {
        "''"
    };
    let transaction = connection
        .transaction()
        .map_err(MatchPackagesError::WritePackageSourceCache)?;
    transaction
        .execute_batch(
            r"
            ALTER TABLE package_source_cache RENAME TO package_source_cache__reverts_old;
            ",
        )
        .map_err(MatchPackagesError::WritePackageSourceCache)?;
    transaction
        .execute_batch(PACKAGE_SOURCE_CACHE_CREATE_SQL)
        .map_err(MatchPackagesError::WritePackageSourceCache)?;
    transaction
        .execute_batch(
            format!(
                r"
            INSERT OR IGNORE INTO package_source_cache (
                package_name,
                package_version,
                entry_path,
                source_content,
                content_hash,
                external_importable,
                external_import_policy_version,
                export_specifier,
                fetched_at,
                expires_at
            )
            SELECT
                package_name,
                package_version,
                entry_path,
                source_content,
                content_hash,
                {external_importable_expr},
                {external_import_policy_version_expr},
                {export_specifier_expr},
                fetched_at,
                expires_at
              FROM package_source_cache__reverts_old
             WHERE TRIM(COALESCE(package_name, '')) != ''
               AND TRIM(COALESCE(package_version, '')) != ''
               AND TRIM(COALESCE(entry_path, '')) != ''
               AND TRIM(COALESCE(source_content, '')) != '';
            DROP TABLE package_source_cache__reverts_old;
            "
            )
            .as_str(),
        )
        .map_err(MatchPackagesError::WritePackageSourceCache)?;
    transaction
        .commit()
        .map_err(MatchPackagesError::WritePackageSourceCache)?;
    Ok(())
}

fn package_source_cache_entry_path(source: &PackageSource) -> String {
    package_source_entry_path(source)
}

fn package_export_specifier(package_name: &str, entry_path: &str) -> String {
    let clean_path = clean_package_entry_path(entry_path);
    if clean_path.is_empty() || clean_path == "." {
        package_name.to_string()
    } else {
        format!("{package_name}/{clean_path}")
    }
}

fn clean_package_entry_path(entry_path: &str) -> String {
    entry_path
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

fn load_package_sources_from_roots(
    package_names: &BTreeSet<String>,
    package_source_roots: &[PathBuf],
) -> Result<Vec<PackageSource>, MatchPackagesError> {
    let mut sources = Vec::new();
    for root in package_source_roots {
        for package_name in package_names {
            for package_dir in package_dir_candidates(root.as_path(), package_name.as_str()) {
                let Some(metadata) = local_package_metadata(package_dir.as_path())? else {
                    continue;
                };
                if metadata.name != *package_name {
                    continue;
                }
                collect_local_package_sources(package_dir.as_path(), &metadata, &mut sources)?;
            }
        }
    }
    Ok(sources)
}

#[derive(Debug)]
struct PackageVersionResolutionPlan {
    available_versions: BTreeMap<String, BTreeSet<Version>>,
    project_exact_versions: BTreeMap<String, BTreeMap<Version, usize>>,
    source_identity_versions: BTreeMap<ModuleId, Version>,
}

impl PackageVersionResolutionPlan {
    fn build(
        rows: &InputRows,
        package_names: &BTreeSet<String>,
        existing_sources: &[PackageSource],
    ) -> Result<Self, MatchPackagesError> {
        let available_versions = exact_package_source_versions_by_package(existing_sources)?;
        let project_exact_versions = exact_project_version_counts_by_package(rows, package_names);
        let source_identity_versions = source_identity_versions_by_module(
            rows,
            existing_sources,
            package_names,
            &available_versions,
            &project_exact_versions,
        )?;
        Ok(Self {
            available_versions,
            project_exact_versions,
            source_identity_versions,
        })
    }

    fn materialization_hints(
        &self,
        rows: &InputRows,
        package_names: &BTreeSet<String>,
    ) -> BTreeSet<(String, String)> {
        rows.modules
            .iter()
            .filter(|module| module.kind == ModuleKind::Package)
            .filter_map(|module| {
                let package_name = scoped_package_name(module, package_names)?;
                let package_version = module.package_version.as_deref().map(str::trim);
                let Some(package_version) = package_version.filter(|version| !version.is_empty())
                else {
                    let resolved = self
                        .best_project_version_candidate(package_name, "latest", None)
                        .or_else(|| {
                            self.available_versions
                                .get(package_name)
                                .and_then(|versions| {
                                    best_matching_package_version_by_binary_search(
                                        "latest", versions,
                                    )
                                })
                        })?;
                    return (!self.package_source_versions_contain(package_name, &resolved))
                        .then(|| (package_name.to_string(), resolved.to_string()));
                };
                if is_exact_package_version_hint(package_version) {
                    return (!package_source_versions_contain(
                        &self.available_versions,
                        package_name,
                        package_version,
                    ))
                    .then(|| (package_name.to_string(), package_version.to_string()));
                }
                let resolved = self
                    .best_project_version_candidate(package_name, package_version, None)
                    .or_else(|| {
                        self.available_versions
                            .get(package_name)
                            .and_then(|versions| {
                                best_matching_package_version_by_binary_search(
                                    package_version,
                                    versions,
                                )
                            })
                    })?;
                (!self.package_source_versions_contain(package_name, &resolved))
                    .then(|| (package_name.to_string(), resolved.to_string()))
            })
            .collect()
    }

    fn stale_cache_materialization_hints(
        &self,
        rows: &InputRows,
        package_names: &BTreeSet<String>,
        stale_cache_versions: &BTreeSet<(String, String)>,
    ) -> BTreeSet<(String, String)> {
        if stale_cache_versions.is_empty() {
            return BTreeSet::new();
        }
        let project_resolved_versions = self.resolved_project_versions(rows, package_names);
        stale_cache_versions
            .iter()
            .filter_map(|(package_name, stale_version)| {
                let versions = self.available_versions.get(package_name)?;
                let resolved = if is_exact_package_version_hint(stale_version) {
                    Version::parse(stale_version).ok()
                } else {
                    self.best_project_version_candidate(package_name, stale_version, Some(versions))
                        .or_else(|| {
                            best_matching_package_version_by_binary_search(stale_version, versions)
                        })
                }?;
                project_resolved_versions
                    .get(package_name)
                    .is_some_and(|needed| needed.contains(&resolved))
                    .then(|| (package_name.clone(), resolved.to_string()))
            })
            .collect()
    }

    fn network_resolution_hints(
        &self,
        rows: &InputRows,
        package_names: &BTreeSet<String>,
    ) -> BTreeSet<(String, String)> {
        rows.modules
            .iter()
            .filter(|module| module.kind == ModuleKind::Package)
            .filter_map(|module| {
                let package_name = scoped_package_name(module, package_names)?;
                let requested_version = module
                    .package_version
                    .as_deref()
                    .map(str::trim)
                    .filter(|version| !version.is_empty());
                let Some(package_version) = requested_version else {
                    let has_project_candidate = self
                        .best_project_version_candidate(package_name, "latest", None)
                        .is_some();
                    let has_available_candidate = self
                        .available_versions
                        .get(package_name)
                        .is_some_and(|versions| {
                            best_matching_package_version_by_binary_search("latest", versions)
                                .is_some()
                        });
                    return (!has_project_candidate && !has_available_candidate)
                        .then(|| (package_name.to_string(), "latest".to_string()));
                };
                if is_exact_package_version_hint(package_version) {
                    return None;
                }
                let has_project_candidate = self
                    .best_project_version_candidate(package_name, package_version, None)
                    .is_some();
                let has_available_candidate = self
                    .available_versions
                    .get(package_name)
                    .is_some_and(|versions| {
                        best_matching_package_version_by_binary_search(package_version, versions)
                            .is_some()
                    });
                (!has_project_candidate && !has_available_candidate)
                    .then(|| (package_name.to_string(), package_version.to_string()))
            })
            .collect()
    }

    fn resolved_project_versions(
        &self,
        rows: &InputRows,
        package_names: &BTreeSet<String>,
    ) -> BTreeMap<String, BTreeSet<Version>> {
        let mut resolved = BTreeMap::<String, BTreeSet<Version>>::new();
        for module in &rows.modules {
            if module.kind != ModuleKind::Package {
                continue;
            }
            let Some(package_name) = scoped_package_name(module, package_names) else {
                continue;
            };
            let Some(versions) = self.available_versions.get(package_name) else {
                continue;
            };
            let requested_version = module
                .package_version
                .as_deref()
                .map(str::trim)
                .filter(|version| !version.is_empty())
                .unwrap_or("latest");
            let selected = if is_exact_package_version_hint(requested_version) {
                Version::parse(requested_version)
                    .ok()
                    .filter(|version| versions.contains(version))
            } else {
                self.best_project_version_candidate(package_name, requested_version, Some(versions))
                    .or_else(|| {
                        best_matching_package_version_by_binary_search(requested_version, versions)
                    })
            };
            if let Some(selected) = selected {
                resolved
                    .entry(package_name.to_string())
                    .or_default()
                    .insert(selected);
            }
        }
        resolved
    }

    fn apply_to_rows(&self, rows: &mut InputRows, package_names: &BTreeSet<String>) -> usize {
        let mut resolved = 0usize;
        for module in &mut rows.modules {
            if module.kind != ModuleKind::Package {
                continue;
            }
            let Some(package_name) = scoped_package_name(module, package_names).map(str::to_string)
            else {
                continue;
            };
            let requested_version = module
                .package_version
                .as_deref()
                .map(str::trim)
                .filter(|version| !version.is_empty());
            let Some(versions) = self.available_versions.get(package_name.as_str()) else {
                continue;
            };
            let source_identity_version = self.source_identity_versions.get(&module.id).cloned();
            let selected = match requested_version {
                Some(package_version) if is_exact_package_version_hint(package_version) => None,
                Some(package_version) => self
                    .best_project_version_candidate(
                        package_name.as_str(),
                        package_version,
                        Some(versions),
                    )
                    .or_else(|| {
                        source_identity_version
                            .filter(|version| version_hint_matches(package_version, version))
                    })
                    .or_else(|| {
                        best_matching_package_version_by_binary_search(package_version, versions)
                    }),
                None => source_identity_version
                    .or_else(|| {
                        self.best_project_version_candidate(
                            package_name.as_str(),
                            "latest",
                            Some(versions),
                        )
                    })
                    .or_else(|| best_matching_package_version_by_binary_search("latest", versions)),
            };
            if let Some(selected) = selected {
                module.package_version = Some(selected.to_string());
                resolved += 1;
            }
        }
        resolved
    }

    fn best_project_version_candidate(
        &self,
        package_name: &str,
        requested_version: &str,
        available_versions: Option<&BTreeSet<Version>>,
    ) -> Option<Version> {
        best_project_version_candidate(
            package_name,
            requested_version,
            &self.project_exact_versions,
            available_versions,
        )
    }

    fn package_source_versions_contain(
        &self,
        package_name: &str,
        package_version: &Version,
    ) -> bool {
        self.available_versions
            .get(package_name)
            .is_some_and(|versions| versions.contains(package_version))
    }
}

fn scoped_package_name<'a>(
    module: &'a ModuleInput,
    package_names: &BTreeSet<String>,
) -> Option<&'a str> {
    let package_name = module.package_name.as_deref()?.trim();
    (!package_name.is_empty()
        && is_valid_package_name(package_name)
        && (package_names.is_empty() || package_names.contains(package_name)))
    .then_some(package_name)
}

fn materialize_package_sources_from_hints(
    rows: &InputRows,
    package_names: &BTreeSet<String>,
    existing_sources: &[PackageSource],
    stale_cache_versions: &BTreeSet<(String, String)>,
) -> Result<Vec<PackageSource>, MatchPackagesError> {
    let plan = PackageVersionResolutionPlan::build(rows, package_names, existing_sources)?;
    let mut hints = plan.materialization_hints(rows, package_names);
    hints.extend(plan.stale_cache_materialization_hints(rows, package_names, stale_cache_versions));
    for (package_name, requested_version) in plan.network_resolution_hints(rows, package_names) {
        match resolve_package_version_hint_from_network(
            package_name.as_str(),
            requested_version.as_str(),
        ) {
            Ok(Some(resolved_version)) => {
                hints.insert((package_name, resolved_version));
            }
            Ok(None) => {
                eprintln!(
                    "skipping package source materialization for {package_name}@{requested_version}: no matching npm version"
                );
            }
            Err(error) => {
                eprintln!(
                    "skipping package source materialization for {package_name}@{requested_version}: {error}"
                );
            }
        }
    }
    if hints.is_empty() {
        return Ok(Vec::new());
    }

    let mut sources = Vec::new();
    for (idx, (package_name, package_version)) in hints.into_iter().enumerate() {
        let temp_root = std::env::temp_dir().join(format!(
            "reverts-package-source-{}-{idx}",
            std::process::id()
        ));
        if temp_root.exists() {
            fs::remove_dir_all(temp_root.as_path()).map_err(|source| {
                MatchPackagesError::ReadPackageSourceRoot {
                    path: temp_root.clone(),
                    source,
                }
            })?;
        }
        fs::create_dir_all(temp_root.as_path()).map_err(|source| {
            MatchPackagesError::ReadPackageSourceRoot {
                path: temp_root.clone(),
                source,
            }
        })?;

        let result = materialize_one_package_source(
            temp_root.as_path(),
            package_name.as_str(),
            package_version.as_str(),
            &mut sources,
        );
        let cleanup = fs::remove_dir_all(temp_root.as_path());
        if let Err(error) = result {
            eprintln!(
                "skipping package source materialization for {package_name}@{package_version}: {error}"
            );
        }
        if let Err(source) = cleanup {
            return Err(MatchPackagesError::ReadPackageSourceRoot {
                path: temp_root,
                source,
            });
        }
    }
    Ok(sources)
}

#[cfg(test)]
fn stale_cache_version_hints_for_materialization(
    rows: &InputRows,
    package_names: &BTreeSet<String>,
    existing_sources: &[PackageSource],
    stale_cache_versions: &BTreeSet<(String, String)>,
) -> BTreeSet<(String, String)> {
    PackageVersionResolutionPlan::build(rows, package_names, existing_sources)
        .expect("package version resolution plan should build")
        .stale_cache_materialization_hints(rows, package_names, stale_cache_versions)
}

fn materialize_one_package_source(
    temp_root: &Path,
    package_name: &str,
    package_version: &str,
    sources: &mut Vec<PackageSource>,
) -> Result<(), MatchPackagesError> {
    let package_spec = format!("{package_name}@{package_version}");
    let mut command = Command::new("npm");
    command
        .arg("install")
        .arg(package_spec.as_str())
        .arg("--prefix")
        .arg(temp_root)
        .arg("--ignore-scripts")
        .arg("--package-lock=false")
        .arg("--no-audit")
        .arg("--no-fund");
    let output =
        run_command_with_timeout(&mut command, npm_install_timeout()).map_err(|message| {
            MatchPackagesError::MaterializePackageSource {
                package_name: package_name.to_string(),
                package_version: package_version.to_string(),
                message,
            }
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let message = if stderr.is_empty() { stdout } else { stderr };
        return Err(MatchPackagesError::MaterializePackageSource {
            package_name: package_name.to_string(),
            package_version: package_version.to_string(),
            message,
        });
    }

    let mut matched = false;
    for package_dir in package_dir_candidates(temp_root, package_name) {
        let Some(metadata) = local_package_metadata(package_dir.as_path())? else {
            continue;
        };
        if metadata.name == package_name && metadata.version == package_version {
            collect_local_package_sources(package_dir.as_path(), &metadata, sources)?;
            matched = true;
            break;
        }
    }
    if !matched {
        return Err(MatchPackagesError::MaterializePackageSource {
            package_name: package_name.to_string(),
            package_version: package_version.to_string(),
            message: "npm install succeeded but the expected package directory was not found"
                .to_string(),
        });
    }
    Ok(())
}

fn npm_install_timeout() -> Duration {
    std::env::var("REVERTS_NPM_INSTALL_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(120))
}

fn run_command_with_timeout(command: &mut Command, timeout: Duration) -> Result<Output, String> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| format!("failed to run command: {source}"))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                return child
                    .wait_with_output()
                    .map_err(|source| format!("failed to collect command output: {source}"));
            }
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("command timed out after {}s", timeout.as_secs()));
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(source) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("failed to wait for command: {source}"));
            }
        }
    }
}

#[cfg(test)]
fn package_version_hints_for_materialization(
    rows: &InputRows,
    package_names: &BTreeSet<String>,
    existing_sources: &[PackageSource],
) -> BTreeSet<(String, String)> {
    PackageVersionResolutionPlan::build(rows, package_names, existing_sources)
        .expect("package version resolution plan should build")
        .materialization_hints(rows, package_names)
}

#[cfg(test)]
fn network_package_version_resolution_hints(
    rows: &InputRows,
    package_names: &BTreeSet<String>,
    existing_sources: &[PackageSource],
) -> BTreeSet<(String, String)> {
    PackageVersionResolutionPlan::build(rows, package_names, existing_sources)
        .expect("package version resolution plan should build")
        .network_resolution_hints(rows, package_names)
}

fn resolve_package_version_hints_to_available_sources(
    rows: &mut InputRows,
    package_sources: &[PackageSource],
    package_names: &BTreeSet<String>,
) -> Result<usize, MatchPackagesError> {
    let plan = PackageVersionResolutionPlan::build(rows, package_names, package_sources)?;
    Ok(plan.apply_to_rows(rows, package_names))
}

fn source_identity_versions_by_module(
    rows: &InputRows,
    package_sources: &[PackageSource],
    package_names: &BTreeSet<String>,
    available_versions: &BTreeMap<String, BTreeSet<Version>>,
    project_exact_versions: &BTreeMap<String, BTreeMap<Version, usize>>,
) -> Result<BTreeMap<ModuleId, Version>, MatchPackagesError> {
    let index = PackageSourceIdentityIndex::build(package_sources)?;
    if index.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut versions_by_module = BTreeMap::<ModuleId, Version>::new();
    for module in rows
        .modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
    {
        let Some(package_name) = module.package_name.as_deref().map(str::trim) else {
            continue;
        };
        if package_name.is_empty()
            || !is_valid_package_name(package_name)
            || (!package_names.is_empty() && !package_names.contains(package_name))
            || !index.contains_package(package_name)
        {
            continue;
        }
        let Some(versions) = available_versions.get(package_name) else {
            continue;
        };
        if !module_needs_source_identity_version(
            module,
            package_name,
            versions,
            project_exact_versions,
        ) {
            continue;
        }
        let Some(slice) = rows.module_source_slice(module.id) else {
            continue;
        };
        if package_module_source_quality(module, slice.source_file_path, slice.source)
            == PackageModuleSourceQuality::Invalid
        {
            continue;
        }
        if let Some(version) = index.best_version_for_source(
            package_name,
            module.package_version.as_deref().map(str::trim),
            slice.source_file_path,
            slice.source,
        )? {
            versions_by_module.insert(module.id, version);
        }
    }
    Ok(versions_by_module)
}

fn module_needs_source_identity_version(
    module: &ModuleInput,
    package_name: &str,
    versions: &BTreeSet<Version>,
    project_exact_versions: &BTreeMap<String, BTreeMap<Version, usize>>,
) -> bool {
    let requested_version = module
        .package_version
        .as_deref()
        .map(str::trim)
        .filter(|version| !version.is_empty());
    match requested_version {
        Some(package_version) if is_exact_package_version_hint(package_version) => false,
        Some(package_version) => best_project_version_candidate(
            package_name,
            package_version,
            project_exact_versions,
            Some(versions),
        )
        .is_none(),
        None => true,
    }
}

#[derive(Debug, Default)]
struct PackageSourceIdentityIndex {
    versions_by_package_hash: BTreeMap<String, BTreeMap<String, BTreeMap<Version, usize>>>,
}

impl PackageSourceIdentityIndex {
    fn build(package_sources: &[PackageSource]) -> Result<Self, MatchPackagesError> {
        let mut versions_by_package_hash =
            BTreeMap::<String, BTreeMap<String, BTreeMap<Version, usize>>>::new();
        for package_source in package_sources {
            let package_version = package_source_version(package_source)?;
            let normalized = normalize_source_for_pipeline(
                package_source.source.as_str(),
                Some(Path::new(package_source.source_path.as_str())),
            )
            .map_err(|source| {
                package_source_normalization_error(
                    package_source.package_name.as_str(),
                    Some(package_source.package_version.as_str()),
                    package_source.source_path.as_str(),
                    source,
                )
            })?;
            let normalized_source_hash = stable_hash(normalized.as_bytes());
            *versions_by_package_hash
                .entry(package_source.package_name.clone())
                .or_default()
                .entry(normalized_source_hash)
                .or_default()
                .entry(package_version)
                .or_default() += 1;
        }
        Ok(Self {
            versions_by_package_hash,
        })
    }

    fn is_empty(&self) -> bool {
        self.versions_by_package_hash.is_empty()
    }

    fn contains_package(&self, package_name: &str) -> bool {
        self.versions_by_package_hash.contains_key(package_name)
    }

    fn best_version_for_source(
        &self,
        package_name: &str,
        package_version: Option<&str>,
        source_file_path: &str,
        source: &str,
    ) -> Result<Option<Version>, MatchPackagesError> {
        let normalized =
            match normalize_source_for_pipeline(source, Some(Path::new(source_file_path))) {
                Ok(normalized) => normalized,
                Err(source) => {
                    return Err(package_source_normalization_error(
                        package_name,
                        package_version,
                        source_file_path,
                        source,
                    ));
                }
            };
        let normalized_source_hash = stable_hash(normalized.as_bytes());
        let Some(versions_by_hash) = self.versions_by_package_hash.get(package_name) else {
            return Ok(None);
        };
        let Some(versions) = versions_by_hash.get(normalized_source_hash.as_str()) else {
            return Ok(None);
        };
        Ok(versions
            .iter()
            .max_by(|left, right| left.1.cmp(right.1).then_with(|| left.0.cmp(right.0)))
            .map(|(version, _count)| version.clone()))
    }
}

fn package_source_normalization_error(
    package_name: &str,
    package_version: Option<&str>,
    source_path: &str,
    source: reverts_js::JsError,
) -> MatchPackagesError {
    MatchPackagesError::NormalizePackageSource {
        package_name: package_name.to_string(),
        package_version: package_version.map(str::to_string),
        source_path: source_path.to_string(),
        source,
    }
}

fn exact_package_source_versions_by_package(
    package_sources: &[PackageSource],
) -> Result<BTreeMap<String, BTreeSet<Version>>, MatchPackagesError> {
    let mut versions = BTreeMap::<String, BTreeSet<Version>>::new();
    for source in package_sources {
        let version = package_source_version(source)?;
        versions
            .entry(source.package_name.clone())
            .or_default()
            .insert(version);
    }
    Ok(versions)
}

fn package_source_version(source: &PackageSource) -> Result<Version, MatchPackagesError> {
    Version::parse(source.package_version.as_str()).map_err(|_| {
        MatchPackagesError::InvalidPackageSourceVersion {
            package_name: source.package_name.clone(),
            package_version: source.package_version.clone(),
            source_path: source.source_path.clone(),
        }
    })
}

fn exact_project_version_counts_by_package(
    rows: &InputRows,
    package_names: &BTreeSet<String>,
) -> BTreeMap<String, BTreeMap<Version, usize>> {
    let mut versions = BTreeMap::<String, BTreeMap<Version, usize>>::new();
    for module in &rows.modules {
        if module.kind != ModuleKind::Package {
            continue;
        }
        let Some(package_name) = module.package_name.as_deref().map(str::trim) else {
            continue;
        };
        if package_name.is_empty()
            || !is_valid_package_name(package_name)
            || (!package_names.is_empty() && !package_names.contains(package_name))
        {
            continue;
        }
        let Some(package_version) = module.package_version.as_deref().map(str::trim) else {
            continue;
        };
        let Ok(version) = Version::parse(package_version) else {
            continue;
        };
        *versions
            .entry(package_name.to_string())
            .or_default()
            .entry(version)
            .or_default() += 1;
    }
    versions
}

fn package_source_versions_contain(
    versions_by_package: &BTreeMap<String, BTreeSet<Version>>,
    package_name: &str,
    package_version: &str,
) -> bool {
    let Ok(version) = Version::parse(package_version) else {
        return false;
    };
    versions_by_package
        .get(package_name)
        .is_some_and(|versions| versions.contains(&version))
}

fn best_project_version_candidate(
    package_name: &str,
    requested_version: &str,
    project_exact_versions: &BTreeMap<String, BTreeMap<Version, usize>>,
    available_versions: Option<&BTreeSet<Version>>,
) -> Option<Version> {
    let versions = project_exact_versions.get(package_name)?;
    let mut candidates = versions
        .iter()
        .filter(|(version, _count)| {
            available_versions.is_none_or(|available| available.contains(version))
                && version_hint_matches(requested_version, version)
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.1
            .cmp(right.1)
            .then_with(|| left.0.cmp(right.0))
            .reverse()
    });
    candidates
        .into_iter()
        .map(|(version, _count)| version.clone())
        .next()
}

fn version_hint_matches(requested_version: &str, version: &Version) -> bool {
    let requested_version = requested_version.trim();
    if requested_version.eq_ignore_ascii_case("latest") {
        return true;
    }
    VersionReq::parse(requested_version).is_ok_and(|requirement| requirement.matches(version))
}

fn best_matching_package_version_by_binary_search(
    requested_version: &str,
    versions: &BTreeSet<Version>,
) -> Option<Version> {
    if versions.is_empty() {
        return None;
    }
    if requested_version.trim().eq_ignore_ascii_case("latest") {
        return versions.iter().next_back().cloned();
    }
    let requirement = VersionReq::parse(requested_version.trim()).ok()?;
    let sorted_versions = versions.iter().cloned().collect::<Vec<_>>();
    let search_end = version_req_upper_bound(&requirement)
        .map(|(upper_bound, inclusive)| {
            if inclusive {
                sorted_versions.partition_point(|version| version <= &upper_bound)
            } else {
                sorted_versions.partition_point(|version| version < &upper_bound)
            }
        })
        .unwrap_or(sorted_versions.len());
    sorted_versions[..search_end]
        .iter()
        .rev()
        .find(|version| requirement.matches(version))
        .cloned()
}

fn version_req_upper_bound(requirement: &VersionReq) -> Option<(Version, bool)> {
    requirement
        .comparators
        .iter()
        .filter_map(comparator_upper_bound)
        .min_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)))
}

fn comparator_upper_bound(comparator: &Comparator) -> Option<(Version, bool)> {
    match comparator.op {
        Op::Exact | Op::Wildcard => partial_exact_upper_bound(comparator),
        Op::Tilde => {
            let minor = comparator.minor?;
            Some((Version::new(comparator.major, minor + 1, 0), false))
        }
        Op::Caret => Some(caret_upper_bound(comparator)),
        Op::Less => Some((partial_version_floor(comparator), false)),
        Op::LessEq => {
            if comparator.patch.is_some() {
                Some((partial_version_floor(comparator), true))
            } else {
                partial_exact_upper_bound(comparator)
            }
        }
        Op::Greater | Op::GreaterEq => None,
        _ => None,
    }
}

fn partial_exact_upper_bound(comparator: &Comparator) -> Option<(Version, bool)> {
    match (comparator.minor, comparator.patch) {
        (Some(minor), Some(patch)) if comparator.op == Op::Exact => Some((
            Version {
                major: comparator.major,
                minor,
                patch,
                pre: comparator.pre.clone(),
                build: BuildMetadata::EMPTY,
            },
            true,
        )),
        (Some(minor), _) => Some((Version::new(comparator.major, minor + 1, 0), false)),
        (None, _) => Some((Version::new(comparator.major + 1, 0, 0), false)),
    }
}

fn caret_upper_bound(comparator: &Comparator) -> (Version, bool) {
    let minor = comparator.minor.unwrap_or(0);
    let patch = comparator.patch.unwrap_or(0);
    if comparator.major > 0 {
        (Version::new(comparator.major + 1, 0, 0), false)
    } else if minor > 0 {
        (Version::new(0, minor + 1, 0), false)
    } else if comparator.patch.is_some() {
        (Version::new(0, 0, patch + 1), false)
    } else if comparator.minor.is_some() {
        (Version::new(0, 1, 0), false)
    } else {
        (Version::new(1, 0, 0), false)
    }
}

fn partial_version_floor(comparator: &Comparator) -> Version {
    Version {
        major: comparator.major,
        minor: comparator.minor.unwrap_or(0),
        patch: comparator.patch.unwrap_or(0),
        pre: comparator.pre.clone(),
        build: BuildMetadata::EMPTY,
    }
}

fn resolve_package_version_hint_from_network(
    package_name: &str,
    requested_version: &str,
) -> Result<Option<String>, MatchPackagesError> {
    let versions = npm_package_versions(package_name, requested_version)?;
    Ok(
        resolve_package_version_hint_from_versions(requested_version, &versions)
            .map(|version| version.to_string()),
    )
}

fn resolve_package_version_hint_from_versions(
    requested_version: &str,
    versions: &BTreeSet<Version>,
) -> Option<Version> {
    best_matching_package_version_by_binary_search(requested_version, versions)
}

fn npm_package_versions(
    package_name: &str,
    requested_version: &str,
) -> Result<BTreeSet<Version>, MatchPackagesError> {
    let output = Command::new("npm")
        .arg("view")
        .arg(package_name)
        .arg("versions")
        .arg("--json")
        .output()
        .map_err(|source| MatchPackagesError::MaterializePackageSource {
            package_name: package_name.to_string(),
            package_version: requested_version.to_string(),
            message: format!("failed to run npm view: {source}"),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let message = if stderr.is_empty() { stdout } else { stderr };
        return Err(MatchPackagesError::MaterializePackageSource {
            package_name: package_name.to_string(),
            package_version: requested_version.to_string(),
            message,
        });
    }
    parse_npm_versions_json(package_name, requested_version, output.stdout.as_slice())
}

fn parse_npm_versions_json(
    package_name: &str,
    requested_version: &str,
    stdout: &[u8],
) -> Result<BTreeSet<Version>, MatchPackagesError> {
    let value = serde_json::from_slice::<serde_json::Value>(stdout).map_err(|source| {
        MatchPackagesError::MaterializePackageSource {
            package_name: package_name.to_string(),
            package_version: requested_version.to_string(),
            message: format!("failed to parse npm versions JSON: {source}"),
        }
    })?;
    let mut versions = BTreeSet::new();
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                let Some(version_text) = item.as_str() else {
                    return Err(MatchPackagesError::MaterializePackageSource {
                        package_name: package_name.to_string(),
                        package_version: requested_version.to_string(),
                        message: "npm versions JSON contained a non-string version entry"
                            .to_string(),
                    });
                };
                versions.insert(parse_npm_version_entry(
                    package_name,
                    requested_version,
                    version_text,
                )?);
            }
        }
        serde_json::Value::String(version) => {
            versions.insert(parse_npm_version_entry(
                package_name,
                requested_version,
                version.as_str(),
            )?);
        }
        _ => {
            return Err(MatchPackagesError::MaterializePackageSource {
                package_name: package_name.to_string(),
                package_version: requested_version.to_string(),
                message: "npm versions JSON must be a string or an array of strings".to_string(),
            });
        }
    }
    Ok(versions)
}

fn parse_npm_version_entry(
    package_name: &str,
    requested_version: &str,
    version: &str,
) -> Result<Version, MatchPackagesError> {
    Version::parse(version).map_err(|source| MatchPackagesError::MaterializePackageSource {
        package_name: package_name.to_string(),
        package_version: requested_version.to_string(),
        message: format!("npm versions JSON contained invalid semver {version}: {source}"),
    })
}

fn package_dir_candidates(root: &Path, package_name: &str) -> Vec<PathBuf> {
    let package_path = package_name
        .split('/')
        .fold(PathBuf::new(), |path, segment| path.join(segment));
    let candidates = vec![
        root.join("node_modules").join(&package_path),
        root.join(&package_path),
        root.to_path_buf(),
    ];
    let mut seen = BTreeSet::new();
    candidates
        .into_iter()
        .filter(|path| path.is_dir())
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalPackageMetadata {
    name: String,
    version: String,
    import_surface: LocalPackageImportSurface,
}

impl LocalPackageMetadata {
    fn importable_target_for(&self, rel_path: &str) -> Option<LocalPackageImportTarget> {
        if let Some(target) = self.import_surface.paths.get(rel_path) {
            return Some(target.clone());
        }
        let mut pattern_targets = BTreeMap::<String, LocalPackageImportKind>::new();
        for target in self
            .import_surface
            .patterns
            .iter()
            .filter_map(|pattern| pattern.target_for_path(rel_path))
        {
            pattern_targets
                .entry(target.specifier)
                .and_modify(|kind| *kind = kind.merge(target.kind))
                .or_insert(target.kind);
        }
        if pattern_targets.len() == 1 {
            let (specifier, kind) = pattern_targets
                .into_iter()
                .next()
                .expect("one pattern target");
            return Some(LocalPackageImportTarget { specifier, kind });
        }
        if self.import_surface.unrestricted_subpath_imports {
            unrestricted_subpath_import_target(self.name.as_str(), rel_path)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LocalPackageImportSurface {
    paths: BTreeMap<String, LocalPackageImportTarget>,
    patterns: Vec<LocalPackageImportPattern>,
    unrestricted_subpath_imports: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalPackageImportTarget {
    specifier: String,
    kind: LocalPackageImportKind,
}

impl LocalPackageImportTarget {
    const fn esm_external_importable(&self) -> bool {
        self.kind.esm_external_importable()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum LocalPackageImportKind {
    Esm,
    CommonJs,
    Universal,
}

impl LocalPackageImportKind {
    const fn esm_external_importable(self) -> bool {
        matches!(self, Self::Esm | Self::Universal)
    }

    const fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Universal, _) | (_, Self::Universal) => Self::Universal,
            (Self::Esm, Self::CommonJs) | (Self::CommonJs, Self::Esm) => Self::Universal,
            (Self::Esm, Self::Esm) => Self::Esm,
            (Self::CommonJs, Self::CommonJs) => Self::CommonJs,
        }
    }

    const fn and_condition(self, condition: Self) -> Option<Self> {
        match (self, condition) {
            (Self::Universal, nested) => Some(nested),
            (parent, Self::Universal) => Some(parent),
            (Self::Esm, Self::Esm) => Some(Self::Esm),
            (Self::CommonJs, Self::CommonJs) => Some(Self::CommonJs),
            (Self::Esm, Self::CommonJs) | (Self::CommonJs, Self::Esm) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct LocalPackageImportPattern {
    target_prefix: String,
    target_suffix: String,
    specifier_prefix: String,
    specifier_suffix: String,
    kind: LocalPackageImportKind,
}

impl LocalPackageImportPattern {
    fn target_for_path(&self, target_path: &str) -> Option<LocalPackageImportTarget> {
        if !target_path.starts_with(self.target_prefix.as_str())
            || !target_path.ends_with(self.target_suffix.as_str())
        {
            return None;
        }
        let wildcard_end = target_path.len().checked_sub(self.target_suffix.len())?;
        if wildcard_end < self.target_prefix.len() {
            return None;
        }
        let wildcard = &target_path[self.target_prefix.len()..wildcard_end];
        if wildcard.is_empty() {
            return None;
        }
        Some(LocalPackageImportTarget {
            specifier: format!(
                "{}{}{}",
                self.specifier_prefix, wildcard, self.specifier_suffix
            ),
            kind: self.kind,
        })
    }
}

fn local_package_metadata(
    package_dir: &Path,
) -> Result<Option<LocalPackageMetadata>, MatchPackagesError> {
    let package_json_path = package_dir.join("package.json");
    if !package_json_path.is_file() {
        return Ok(None);
    }
    let content = fs::read_to_string(package_json_path.as_path()).map_err(|source| {
        MatchPackagesError::ReadPackageSourceRoot {
            path: package_json_path.clone(),
            source,
        }
    })?;
    let value = serde_json::from_str::<serde_json::Value>(content.as_str()).map_err(|source| {
        MatchPackagesError::InvalidPackageMetadata {
            path: package_json_path.clone(),
            source,
        }
    })?;
    let Some(package_name) = value.get("name").and_then(serde_json::Value::as_str) else {
        return Ok(None);
    };
    let Some(package_version) = value
        .get("version")
        .and_then(serde_json::Value::as_str)
        .filter(|version| !version.trim().is_empty())
    else {
        return Ok(None);
    };
    let package_name = package_name.trim().to_string();
    Ok(Some(LocalPackageMetadata {
        import_surface: package_importable_surface(value.as_object(), package_name.as_str()),
        name: package_name,
        version: package_version.trim().to_string(),
    }))
}

fn collect_local_package_sources(
    package_dir: &Path,
    metadata: &LocalPackageMetadata,
    sources: &mut Vec<PackageSource>,
) -> Result<(), MatchPackagesError> {
    let source_files = collect_local_package_source_files(package_dir)?;
    let selected_rel_paths = select_runtime_package_source_paths(&source_files);
    for (rel_path, path) in source_files {
        if !selected_rel_paths.contains(rel_path.as_str()) {
            continue;
        }
        let source = fs::read_to_string(path.as_path()).map_err(|source| {
            MatchPackagesError::ReadPackageSourceRoot {
                path: path.clone(),
                source,
            }
        })?;
        let importable_target = metadata.importable_target_for(rel_path.as_str());
        if is_json_source_path(rel_path.as_str()) && importable_target.is_none() {
            continue;
        }
        let source = package_source_body_for_local_file(rel_path.as_str(), source.as_str())
            .unwrap_or(source);
        let source_path = format!("{}@{}/{}", metadata.name, metadata.version, rel_path);
        if let Some(export_target) = importable_target
            .as_ref()
            .filter(|target| target.esm_external_importable())
        {
            sources.push(PackageSource::external(
                metadata.name.as_str(),
                metadata.version.as_str(),
                export_target.specifier.as_str(),
                source_path,
                source,
            ));
        } else {
            let export_specifier = importable_target
                .as_ref()
                .map(|target| target.specifier.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    package_export_specifier(metadata.name.as_str(), rel_path.as_str())
                });
            sources.push(PackageSource::source_only(
                metadata.name.as_str(),
                metadata.version.as_str(),
                export_specifier,
                source_path,
                source,
            ));
        }
    }
    Ok(())
}

fn collect_local_package_source_files(
    package_dir: &Path,
) -> Result<Vec<(String, PathBuf)>, MatchPackagesError> {
    let mut stack = vec![package_dir.to_path_buf()];
    let mut source_files = Vec::new();
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(dir.as_path()).map_err(|source| {
            MatchPackagesError::ReadPackageSourceRoot {
                path: dir.clone(),
                source,
            }
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| MatchPackagesError::ReadPackageSourceRoot {
                path: dir.clone(),
                source,
            })?;
            let path = entry.path();
            let file_type =
                entry
                    .file_type()
                    .map_err(|source| MatchPackagesError::ReadPackageSourceRoot {
                        path: path.clone(),
                        source,
                    })?;
            if file_type.is_dir() {
                if should_descend_package_source_dir(path.as_path()) {
                    stack.push(path);
                }
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Ok(rel) = path.strip_prefix(package_dir) else {
                continue;
            };
            let rel_path = slash_path(rel);
            if !is_local_package_source_candidate(rel_path.as_str()) {
                continue;
            }
            source_files.push((rel_path, path));
        }
    }
    source_files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(source_files)
}

fn select_runtime_package_source_paths(source_files: &[(String, PathBuf)]) -> BTreeSet<String> {
    let has_compiled_runtime_sources = source_files.iter().any(|(rel_path, _path)| {
        is_javascript_source_path(rel_path)
            && runtime_build_family_score(rel_path)
                .is_some_and(|score| score <= RUNTIME_BUILD_FAMILY_MAX_SCORE)
    });
    source_files
        .iter()
        .filter_map(|(rel_path, _path)| {
            if has_compiled_runtime_sources && is_typescript_source_family_path(rel_path) {
                return None;
            }
            Some(rel_path.clone())
        })
        .collect()
}

fn package_importable_surface(
    package_json: Option<&serde_json::Map<String, serde_json::Value>>,
    package_name: &str,
) -> LocalPackageImportSurface {
    let mut import_surface = LocalPackageImportSurface::default();
    let Some(package_json) = package_json else {
        return import_surface;
    };

    if let Some(exports) = package_json.get("exports") {
        collect_exports_importable_paths(
            exports,
            package_name,
            ".",
            LocalPackageImportKind::Universal,
            &mut import_surface,
        );
        dedup_import_patterns(&mut import_surface.patterns);
        return import_surface;
    }

    // Packages without an `exports` map do not hide their files behind an
    // export whitelist in Node's package resolution. Treat collected runtime
    // files as importable subpaths (`pkg/lib/file.js`) so exact package-source
    // matches can be externalized instead of vendoring the recovered code.
    import_surface.unrestricted_subpath_imports = true;

    if let Some(target) = package_json
        .get("module")
        .and_then(serde_json::Value::as_str)
    {
        insert_importable_exact_target(
            &mut import_surface.paths,
            target,
            package_name,
            LocalPackageImportKind::Esm,
        );
    }
    if let Some(target) = package_json
        .get("browser")
        .and_then(serde_json::Value::as_str)
    {
        insert_importable_exact_target(
            &mut import_surface.paths,
            target,
            package_name,
            LocalPackageImportKind::Esm,
        );
    }
    if let Some(target) = package_json.get("main").and_then(serde_json::Value::as_str) {
        insert_importable_exact_target(
            &mut import_surface.paths,
            target,
            package_name,
            LocalPackageImportKind::Universal,
        );
    }
    insert_importable_exact_target(
        &mut import_surface.paths,
        "index.js",
        package_name,
        LocalPackageImportKind::Universal,
    );
    import_surface
}

fn unrestricted_subpath_import_target(
    package_name: &str,
    rel_path: &str,
) -> Option<LocalPackageImportTarget> {
    let clean = clean_package_entry_path(rel_path);
    if clean.is_empty()
        || clean == "."
        || clean.starts_with("../")
        || clean.contains("/../")
        || clean.ends_with(".d.ts")
    {
        return None;
    }
    let kind = unrestricted_subpath_import_kind(clean.as_str())?;
    Some(LocalPackageImportTarget {
        specifier: package_export_specifier(package_name, clean.as_str()),
        kind,
    })
}

fn unrestricted_subpath_import_kind(rel_path: &str) -> Option<LocalPackageImportKind> {
    match Path::new(rel_path).extension().and_then(|ext| ext.to_str()) {
        Some("mjs" | "ts" | "tsx") => Some(LocalPackageImportKind::Esm),
        Some("cjs") => Some(LocalPackageImportKind::CommonJs),
        Some("js") => Some(LocalPackageImportKind::Universal),
        _ => None,
    }
}

fn collect_exports_importable_paths(
    value: &serde_json::Value,
    package_name: &str,
    export_key: &str,
    kind: LocalPackageImportKind,
    import_surface: &mut LocalPackageImportSurface,
) {
    match value {
        serde_json::Value::String(target) => {
            insert_export_target(import_surface, target, package_name, export_key, kind);
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_exports_importable_paths(
                    item,
                    package_name,
                    export_key,
                    kind,
                    import_surface,
                );
            }
        }
        serde_json::Value::Object(object) => {
            if object.keys().any(|key| key == "." || key.starts_with("./")) {
                for (nested_export_key, nested_value) in object {
                    collect_exports_importable_paths(
                        nested_value,
                        package_name,
                        nested_export_key,
                        kind,
                        import_surface,
                    );
                }
            } else {
                for (condition, nested_kind) in [
                    ("import", LocalPackageImportKind::Esm),
                    ("require", LocalPackageImportKind::CommonJs),
                    ("default", LocalPackageImportKind::Universal),
                    ("node", LocalPackageImportKind::Universal),
                    ("browser", LocalPackageImportKind::Esm),
                ] {
                    if let Some(nested_value) = object.get(condition)
                        && let Some(kind) = kind.and_condition(nested_kind)
                    {
                        collect_exports_importable_paths(
                            nested_value,
                            package_name,
                            export_key,
                            kind,
                            import_surface,
                        );
                    }
                }
            }
        }
        _ => {}
    }
}

fn insert_export_target(
    import_surface: &mut LocalPackageImportSurface,
    target: &str,
    package_name: &str,
    export_key: &str,
    kind: LocalPackageImportKind,
) {
    if export_key.contains('*') || target.contains('*') {
        insert_importable_pattern(
            &mut import_surface.patterns,
            target,
            package_name,
            export_key,
            kind,
        );
        return;
    }
    let Some(export_specifier) = export_key_to_specifier(package_name, export_key) else {
        return;
    };
    insert_importable_exact_target(
        &mut import_surface.paths,
        target,
        export_specifier.as_str(),
        kind,
    );
}

fn export_key_to_specifier(package_name: &str, export_key: &str) -> Option<String> {
    if export_key.contains('*') {
        return None;
    }
    if export_key == "." {
        return Some(package_name.to_string());
    }
    export_key
        .strip_prefix("./")
        .filter(|subpath| !subpath.trim().is_empty())
        .map(|subpath| format!("{package_name}/{subpath}"))
}

fn export_pattern_to_specifier_parts(
    package_name: &str,
    export_key: &str,
) -> Option<(String, String)> {
    if export_key.matches('*').count() != 1 {
        return None;
    }
    let subpath = export_key
        .strip_prefix("./")
        .filter(|subpath| !subpath.trim().is_empty())?;
    let specifier_pattern = format!("{package_name}/{subpath}");
    let (prefix, suffix) = specifier_pattern.split_once('*')?;
    Some((prefix.to_string(), suffix.to_string()))
}

fn insert_importable_exact_target(
    importable_paths: &mut BTreeMap<String, LocalPackageImportTarget>,
    target: &str,
    export_specifier: &str,
    kind: LocalPackageImportKind,
) {
    let Some(clean_target) = clean_export_target(target) else {
        return;
    };
    for candidate in importable_target_candidates(clean_target.as_str()) {
        match importable_paths.get_mut(candidate.as_str()) {
            Some(existing) if existing.specifier == export_specifier => {
                existing.kind = existing.kind.merge(kind);
            }
            Some(_) => {}
            None => {
                importable_paths.insert(
                    candidate,
                    LocalPackageImportTarget {
                        specifier: export_specifier.to_string(),
                        kind,
                    },
                );
            }
        }
    }
}

fn insert_importable_pattern(
    patterns: &mut Vec<LocalPackageImportPattern>,
    target: &str,
    package_name: &str,
    export_key: &str,
    kind: LocalPackageImportKind,
) {
    let Some((specifier_prefix, specifier_suffix)) =
        export_pattern_to_specifier_parts(package_name, export_key)
    else {
        return;
    };
    let Some(clean_target) = clean_export_pattern_target(target) else {
        return;
    };
    let Some((target_prefix, target_suffix)) = clean_target.split_once('*') else {
        return;
    };
    patterns.push(LocalPackageImportPattern {
        target_prefix: target_prefix.to_string(),
        target_suffix: target_suffix.to_string(),
        specifier_prefix,
        specifier_suffix,
        kind,
    });
}

fn clean_export_target(target: &str) -> Option<String> {
    let clean = target
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/');
    if clean.is_empty()
        || clean == "."
        || clean.contains('*')
        || clean.starts_with("../")
        || clean.contains("/../")
    {
        return None;
    }
    Some(clean.to_string())
}

fn clean_export_pattern_target(target: &str) -> Option<String> {
    let clean = target
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/');
    if clean.is_empty()
        || clean == "."
        || clean.matches('*').count() != 1
        || clean.starts_with("../")
        || clean.contains("/../")
    {
        return None;
    }
    Some(clean.to_string())
}

fn importable_target_candidates(clean_target: &str) -> Vec<String> {
    let mut candidates = vec![clean_target.to_string()];
    if Path::new(clean_target).extension().is_none() {
        for extension in ["js", "mjs", "cjs", "ts", "tsx"] {
            candidates.push(format!("{clean_target}.{extension}"));
        }
        for extension in ["js", "mjs", "cjs", "ts", "tsx"] {
            candidates.push(format!("{clean_target}/index.{extension}"));
        }
    }
    candidates
}

fn dedup_import_patterns(patterns: &mut Vec<LocalPackageImportPattern>) {
    let mut seen = BTreeSet::new();
    patterns.retain(|pattern| seen.insert(pattern.clone()));
}

fn should_descend_package_source_dir(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return true;
    };
    !matches!(
        name.to_ascii_lowercase().as_str(),
        "node_modules" | "test" | "tests" | "__tests__" | "coverage" | "benchmark" | "benchmarks"
    )
}

const RUNTIME_BUILD_FAMILY_MAX_SCORE: u8 = 3;
const PACKAGE_SOURCE_PATH_HINT_FILTER_MIN_SOURCES: usize = 256;

fn runtime_build_family_score(rel_path: &str) -> Option<u8> {
    let lower = rel_path.to_ascii_lowercase();
    let first_segment = lower.split('/').next().unwrap_or("");
    if matches!(
        first_segment,
        "dist" | "lib" | "cjs" | "esm" | "module" | "build"
    ) {
        return Some(1);
    }
    if lower.contains("/dist/")
        || lower.contains("/lib/")
        || lower.contains("/cjs/")
        || lower.contains("/esm/")
    {
        return Some(2);
    }
    if !lower.starts_with("src/") {
        return Some(3);
    }
    Some(5)
}

fn is_typescript_source_family_path(rel_path: &str) -> bool {
    let lower = rel_path.to_ascii_lowercase();
    lower.starts_with("src/")
        && matches!(
            Path::new(lower.as_str())
                .extension()
                .and_then(|ext| ext.to_str()),
            Some("ts" | "tsx")
        )
}

fn is_javascript_source_path(rel_path: &str) -> bool {
    matches!(
        Path::new(rel_path).extension().and_then(|ext| ext.to_str()),
        Some("js" | "mjs" | "cjs")
    )
}

fn package_source_body_for_local_file(rel_path: &str, source: &str) -> Option<String> {
    if is_json_source_path(rel_path) {
        json_package_source_module(source)
    } else {
        None
    }
}

fn json_package_source_module(source: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(source).ok()?;
    let json = serde_json::to_string(&value).ok()?;
    Some(format!("export default {json};\n"))
}

fn filter_package_sources_to_best_build_variants(
    rows: &InputRows,
    package_sources: &mut Vec<PackageSource>,
) {
    let hints_by_version = package_path_hints_by_version(rows);
    if hints_by_version.is_empty() {
        return;
    }

    let mut source_families_by_version =
        BTreeMap::<(String, String), BTreeMap<String, Vec<usize>>>::new();
    for (index, source) in package_sources.iter().enumerate() {
        let key = (source.package_name.clone(), source.package_version.clone());
        let rel_path = package_source_cache_entry_path(source);
        source_families_by_version
            .entry(key)
            .or_default()
            .entry(build_variant_family_key(rel_path.as_str()))
            .or_default()
            .push(index);
    }

    let mut selected_families_by_version = BTreeMap::<(String, String), BTreeSet<String>>::new();
    for (key, family_paths) in source_families_by_version {
        let Some(hints) = hints_by_version.get(&key) else {
            continue;
        };
        let mut scored = family_paths
            .into_iter()
            .map(|(family, paths)| {
                let matched_hints = hints
                    .iter()
                    .map(|hint| {
                        paths
                            .iter()
                            .map(|index| {
                                package_source_semantic_filter_hint_score(
                                    &package_sources[*index],
                                    hint,
                                )
                            })
                            .max()
                            .unwrap_or(0)
                    })
                    .sum::<usize>();
                let has_external_importable = paths
                    .iter()
                    .any(|index| package_sources[*index].external_importable);
                (family, matched_hints, has_external_importable)
            })
            .filter(|(_family, matched_hints, _has_external_importable)| *matched_hints > 0)
            .collect::<Vec<_>>();
        if scored.is_empty() {
            continue;
        }
        scored.sort_by(|left, right| {
            right
                .1
                .cmp(&left.1)
                .then_with(|| {
                    build_variant_family_rank(left.0.as_str())
                        .cmp(&build_variant_family_rank(right.0.as_str()))
                })
                .then_with(|| left.0.cmp(&right.0))
        });
        let best_score = scored[0].1;
        let best_rank = build_variant_family_rank(scored[0].0.as_str());
        let best_external_rank = scored
            .iter()
            .filter(|(_family, score, has_external_importable)| {
                *score == best_score && *has_external_importable
            })
            .map(|(family, _score, _has_external_importable)| {
                build_variant_family_rank(family.as_str())
            })
            .min();
        let selected = scored
            .into_iter()
            .filter(|(family, score, has_external_importable)| {
                if *score != best_score {
                    return false;
                }
                let rank = build_variant_family_rank(family.as_str());
                if best_score < 2 || rank == best_rank {
                    return true;
                }
                best_external_rank
                    .is_some_and(|external_rank| *has_external_importable && rank == external_rank)
            })
            .map(|(family, _score, _has_external_importable)| family)
            .collect::<BTreeSet<_>>();
        selected_families_by_version.insert(key, selected);
    }

    if selected_families_by_version.is_empty() {
        return;
    }

    package_sources.retain(|source| {
        if source.external_importable && source.export_specifier == source.package_name {
            return true;
        }
        let key = (source.package_name.clone(), source.package_version.clone());
        let Some(selected_families) = selected_families_by_version.get(&key) else {
            return true;
        };
        let rel_path = package_source_cache_entry_path(source);
        selected_families.contains(build_variant_family_key(rel_path.as_str()).as_str())
    });
}

fn filter_package_sources_to_relevant_path_hints(
    rows: &InputRows,
    package_sources: &mut Vec<PackageSource>,
) {
    let hints_by_version = package_path_hints_by_version(rows);
    if hints_by_version.is_empty() {
        return;
    }
    let counts_by_version = package_sources.iter().fold(
        BTreeMap::<(String, String), usize>::new(),
        |mut counts, source| {
            *counts
                .entry((source.package_name.clone(), source.package_version.clone()))
                .or_default() += 1;
            counts
        },
    );
    package_sources.retain(|source| {
        let key = (source.package_name.clone(), source.package_version.clone());
        if counts_by_version.get(&key).copied().unwrap_or_default()
            <= PACKAGE_SOURCE_PATH_HINT_FILTER_MIN_SOURCES
        {
            return true;
        }
        let Some(hints) = hints_by_version.get(&key) else {
            return true;
        };
        if source.external_importable && source.export_specifier == source.package_name {
            return true;
        }
        hints
            .iter()
            .any(|hint| package_source_semantic_filter_hint_score(source, hint.as_str()) > 0)
    });
}

fn package_source_semantic_filter_hint_score(source: &PackageSource, hint: &str) -> usize {
    package_source_semantic_surface_hint_score(source, hint).max(
        package_source_body_semantic_hint_score(source.source.as_str(), hint),
    )
}

fn package_source_body_semantic_hint_score(source: &str, hint: &str) -> usize {
    let hint = hint.trim().trim_matches('/');
    if hint.is_empty() {
        return 0;
    }
    let hint_last_segment = hint.rsplit('/').next().unwrap_or(hint);
    let hint_last_normalized = normalize_hint_text(hint_last_segment);
    if hint_last_normalized.len() < 4 {
        return 0;
    }
    let source_normalized = normalize_hint_text(source);
    if source_normalized.contains(hint_last_normalized.as_str()) {
        2
    } else {
        0
    }
}

fn package_path_hints_by_version(rows: &InputRows) -> BTreeMap<(String, String), BTreeSet<String>> {
    let mut hints = BTreeMap::<(String, String), BTreeSet<String>>::new();
    for module in &rows.modules {
        if module.kind != ModuleKind::Package {
            continue;
        }
        let Some(package_name) = module.package_name.as_deref().map(str::trim) else {
            continue;
        };
        let Some(package_version) = module
            .package_version
            .as_deref()
            .map(str::trim)
            .filter(|version| is_exact_package_version_hint(version))
        else {
            continue;
        };
        let Some(hint) =
            clean_package_semantic_path_hint(package_name, module.semantic_path.as_str())
        else {
            continue;
        };
        hints
            .entry((package_name.to_string(), package_version.to_string()))
            .or_default()
            .insert(hint);
    }
    hints
}

fn build_variant_family_key(rel_path: &str) -> String {
    let lower = rel_path.to_ascii_lowercase();
    let parts = lower.split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        ["dist", second, ..]
            if matches!(
                *second,
                "cjs"
                    | "commonjs"
                    | "esm"
                    | "es"
                    | "es5"
                    | "es2015"
                    | "es2020"
                    | "module"
                    | "browser"
                    | "node"
                    | "umd"
                    | "bundles"
                    | "fesm5"
                    | "fesm2015"
                    | "fesm2020"
                    | "fesm2022"
            ) =>
        {
            format!("dist/{second}")
        }
        ["lib", second, ..]
            if matches!(
                *second,
                "cjs"
                    | "commonjs"
                    | "esm"
                    | "es"
                    | "es5"
                    | "es2015"
                    | "es2020"
                    | "module"
                    | "browser"
                    | "node"
                    | "umd"
            ) =>
        {
            format!("lib/{second}")
        }
        [first, ..]
            if matches!(
                *first,
                "dist"
                    | "lib"
                    | "cjs"
                    | "commonjs"
                    | "esm"
                    | "es"
                    | "es5"
                    | "es2015"
                    | "es2020"
                    | "module"
                    | "umd"
                    | "bundles"
                    | "fesm5"
                    | "fesm2015"
                    | "fesm2020"
                    | "fesm2022"
                    | "build"
                    | "src"
            ) =>
        {
            (*first).to_string()
        }
        _ => "root".to_string(),
    }
}

fn build_variant_family_rank(family: &str) -> u8 {
    match family {
        "dist/esm" | "dist/es" | "dist/es5" | "dist/es2015" | "dist/es2020" | "dist/module"
        | "lib/esm" | "lib/es" | "lib/es5" | "lib/es2015" | "lib/es2020" | "lib/module" | "esm"
        | "es" | "es5" | "es2015" | "es2020" | "module" => 0,
        "dist/cjs" | "dist/commonjs" | "lib/cjs" | "lib/commonjs" | "cjs" | "commonjs" => 1,
        "dist/node" | "lib/node" => 2,
        "dist/umd" | "dist/bundles" | "lib/umd" | "umd" | "bundles" => 3,
        "dist/fesm5" | "dist/fesm2015" | "dist/fesm2020" | "dist/fesm2022" | "fesm5"
        | "fesm2015" | "fesm2020" | "fesm2022" => 4,
        "dist" | "lib" | "build" => 5,
        "root" => 6,
        "src" => 7,
        _ => 8,
    }
}

fn is_local_package_source_candidate(rel_path: &str) -> bool {
    let lower = rel_path.to_ascii_lowercase();
    if lower.ends_with(".d.ts")
        || lower.ends_with("tsconfig.json")
        || lower.ends_with("/tsconfig.json")
        || lower.ends_with(".min.js")
        || lower.contains("/test/")
        || lower.contains("/tests/")
        || lower.contains("/__tests__/")
        || lower.starts_with("test/")
        || lower.starts_with("tests/")
        || lower.starts_with("__tests__/")
    {
        return false;
    }
    matches!(
        Path::new(rel_path).extension().and_then(|ext| ext.to_str()),
        Some("js" | "mjs" | "cjs" | "ts" | "tsx" | "json")
    )
}

fn slash_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(segment) => Some(segment.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn dedup_package_sources(package_sources: &mut Vec<PackageSource>) {
    package_sources.sort_by(|left, right| {
        (
            left.package_name.as_str(),
            left.package_version.as_str(),
            left.source_path.as_str(),
        )
            .cmp(&(
                right.package_name.as_str(),
                right.package_version.as_str(),
                right.source_path.as_str(),
            ))
            .then_with(|| right.external_importable.cmp(&left.external_importable))
    });
    let mut seen = BTreeSet::new();
    package_sources.retain(|source| {
        seen.insert((
            source.package_name.clone(),
            source.package_version.clone(),
            source.source_path.clone(),
        ))
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PackageVersionResolutionEvidence {
    requested_version: Option<String>,
    resolved_version: String,
    reason: &'static str,
}

fn package_versions_by_module(rows: &InputRows) -> BTreeMap<ModuleId, Option<String>> {
    rows.modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
        .map(|module| {
            (
                module.id,
                module
                    .package_version
                    .as_deref()
                    .map(str::trim)
                    .filter(|version| !version.is_empty())
                    .map(ToOwned::to_owned),
            )
        })
        .collect()
}

fn package_version_resolution_evidence(
    before: &BTreeMap<ModuleId, Option<String>>,
    rows: &InputRows,
) -> BTreeMap<ModuleId, PackageVersionResolutionEvidence> {
    rows.modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
        .filter_map(|module| {
            let requested_version = before.get(&module.id).cloned().flatten();
            let resolved_version = module
                .package_version
                .as_deref()
                .map(str::trim)
                .filter(|version| !version.is_empty())?;
            if requested_version.as_deref() == Some(resolved_version) {
                return None;
            }
            Some((
                module.id,
                PackageVersionResolutionEvidence {
                    reason: package_version_resolution_reason(
                        requested_version.as_deref(),
                        resolved_version,
                    ),
                    requested_version,
                    resolved_version: resolved_version.to_string(),
                },
            ))
        })
        .collect()
}

fn package_version_resolution_reason(
    requested_version: Option<&str>,
    resolved_version: &str,
) -> &'static str {
    let Some(requested_version) = requested_version else {
        return "missing_hint_resolved_to_available_source";
    };
    if Version::parse(resolved_version)
        .ok()
        .is_some_and(|version| version_hint_matches(requested_version, &version))
    {
        "range_resolved_to_available_source"
    } else {
        "non_matching_version_resolution"
    }
}

fn persist_package_attributions(
    connection: &mut Connection,
    rows: &InputRows,
    report: &VersionedPackageMatchReport,
    matched_package_names: &BTreeSet<String>,
    version_resolutions: &BTreeMap<ModuleId, PackageVersionResolutionEvidence>,
) -> Result<usize, MatchPackagesError> {
    let rejected_attributions =
        rejected_package_attributions_for_unaccepted_modules(rows, report, matched_package_names)?;
    if report.attributions.is_empty() && rejected_attributions.is_empty() {
        return Ok(0);
    }

    ensure_package_attributions_table(connection)?;

    let matches_by_module = report
        .matches
        .iter()
        .map(|module_match| (module_match.module_id, module_match))
        .collect::<BTreeMap<_, _>>();
    let chain_proofs = externalization_chain_proofs(rows, report);
    let diagnostics_context = PackageDiagnosticsContext::new(rows, report);
    let modules_by_id = rows
        .modules
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let transaction = connection
        .transaction()
        .map_err(MatchPackagesError::WriteAttribution)?;
    let mut written = 0;

    for attribution in &report.attributions {
        let module_match = matches_by_module.get(&attribution.module_id).ok_or(
            MatchPackagesError::MissingMatchEvidence {
                module_id: attribution.module_id,
            },
        )?;
        let module = modules_by_id.get(&attribution.module_id).copied().ok_or(
            MatchPackagesError::MissingModuleForAttribution {
                module_id: attribution.module_id,
            },
        )?;
        persist_package_attribution(
            &transaction,
            module.original_name.as_str(),
            attribution,
            module_match,
            version_resolutions.get(&attribution.module_id),
            chain_proofs.get(&attribution.module_id),
        )?;
        written += 1;
    }
    for attribution in &rejected_attributions {
        let module = modules_by_id.get(&attribution.module_id).copied().ok_or(
            MatchPackagesError::MissingModuleForAttribution {
                module_id: attribution.module_id,
            },
        )?;
        persist_rejected_package_attribution(
            &transaction,
            module.original_name.as_str(),
            attribution,
            matches_by_module.get(&attribution.module_id).copied(),
            unmatched_package_diagnostics(rows, &diagnostics_context, module),
        )?;
        written += 1;
    }

    transaction
        .commit()
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(written)
}

fn ensure_package_attributions_table(
    connection: &mut Connection,
) -> Result<(), MatchPackagesError> {
    connection
        .execute_batch(PACKAGE_ATTRIBUTIONS_CREATE_SQL)
        .map_err(MatchPackagesError::WriteAttribution)?;
    if package_attributions_requires_nullable_version_migration(connection)
        .map_err(MatchPackagesError::WriteAttribution)?
    {
        migrate_package_attributions_nullable_version(connection)?;
    }
    if !sqlite_table_has_column(
        connection,
        "package_attributions",
        "external_import_policy_version",
    )
    .map_err(MatchPackagesError::WriteAttribution)?
    {
        connection
            .execute_batch(
                r"
                ALTER TABLE package_attributions
                    ADD COLUMN external_import_policy_version INTEGER NOT NULL DEFAULT 0;
                ",
            )
            .map_err(MatchPackagesError::WriteAttribution)?;
    }
    connection
        .execute_batch(PACKAGE_ATTRIBUTIONS_INDEX_SQL)
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(())
}

const PACKAGE_ATTRIBUTIONS_CREATE_SQL: &str = r"
CREATE TABLE IF NOT EXISTS package_attributions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    module_id INTEGER NOT NULL,
    module_original_name TEXT NOT NULL,
    package_name TEXT NOT NULL,
    package_version TEXT,
    package_subpath TEXT,
    resolved_file TEXT,
    export_specifier TEXT,
    emission_mode TEXT NOT NULL,
    status TEXT NOT NULL,
    evidence_json TEXT,
    rejection_reason TEXT,
    external_import_policy_version INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (module_id),
    FOREIGN KEY (module_id) REFERENCES modules(id) ON DELETE CASCADE,
    CHECK (TRIM(module_original_name) != ''),
    CHECK (TRIM(package_name) != ''),
    CHECK (emission_mode IN (
        'external_import',
        'vendored_asset',
        'application_source',
        'runtime_glue'
    )),
    CHECK (status IN ('proposed', 'accepted', 'rejected')),
    CHECK (status != 'accepted' OR TRIM(COALESCE(package_version, '')) != ''),
    CHECK (
        status != 'accepted'
        OR emission_mode != 'external_import'
        OR TRIM(COALESCE(export_specifier, '')) != ''
    ),
    CHECK (status != 'rejected' OR TRIM(COALESCE(rejection_reason, '')) != '')
);
";

const PACKAGE_ATTRIBUTIONS_INDEX_SQL: &str = r"
CREATE INDEX IF NOT EXISTS idx_package_attributions_package
    ON package_attributions(package_name, package_version);
CREATE INDEX IF NOT EXISTS idx_package_attributions_status
    ON package_attributions(status);
CREATE INDEX IF NOT EXISTS idx_package_attributions_emission
    ON package_attributions(emission_mode);
";

fn package_attributions_requires_nullable_version_migration(
    connection: &Connection,
) -> rusqlite::Result<bool> {
    let mut statement = connection.prepare("PRAGMA table_info(package_attributions)")?;
    let columns = statement.query_map([], |row| {
        Ok((row.get::<_, String>(1)?, row.get::<_, i64>(3)?))
    })?;
    let mut package_version_not_null = false;
    for column in columns {
        let (name, not_null) = column?;
        if name == "package_version" && not_null != 0 {
            package_version_not_null = true;
            break;
        }
    }

    let create_sql = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'package_attributions'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let has_legacy_non_empty_version_check = create_sql
        .as_deref()
        .is_some_and(|sql| sql.contains("TRIM(package_version) != ''"));

    Ok(package_version_not_null || has_legacy_non_empty_version_check)
}

fn migrate_package_attributions_nullable_version(
    connection: &mut Connection,
) -> Result<(), MatchPackagesError> {
    let transaction = connection
        .transaction()
        .map_err(MatchPackagesError::WriteAttribution)?;
    transaction
        .execute_batch(
            r"
            ALTER TABLE package_attributions RENAME TO package_attributions__reverts_old;
            ",
        )
        .map_err(MatchPackagesError::WriteAttribution)?;
    transaction
        .execute_batch(PACKAGE_ATTRIBUTIONS_CREATE_SQL)
        .map_err(MatchPackagesError::WriteAttribution)?;
    transaction
        .execute_batch(
            r"
            INSERT INTO package_attributions (
                id,
                module_id,
                module_original_name,
                package_name,
                package_version,
                package_subpath,
                resolved_file,
                export_specifier,
                emission_mode,
                status,
                evidence_json,
                rejection_reason,
                external_import_policy_version,
                created_at,
                updated_at
            )
            SELECT
                id,
                module_id,
                module_original_name,
                package_name,
                package_version,
                package_subpath,
                resolved_file,
                export_specifier,
                emission_mode,
                status,
                evidence_json,
                rejection_reason,
                0,
                created_at,
                updated_at
              FROM package_attributions__reverts_old;
            DROP TABLE package_attributions__reverts_old;
            ",
        )
        .map_err(MatchPackagesError::WriteAttribution)?;
    transaction
        .commit()
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(())
}

fn rejected_package_attributions_for_unaccepted_modules(
    rows: &InputRows,
    report: &VersionedPackageMatchReport,
    matched_package_names: &BTreeSet<String>,
) -> Result<Vec<PackageAttributionInput>, MatchPackagesError> {
    let accepted_modules = report
        .attributions
        .iter()
        .filter(|attribution| {
            attribution.status == PackageAttributionStatus::Accepted
                && attribution.emission_mode == PackageEmissionMode::ExternalImport
        })
        .map(|attribution| attribution.module_id)
        .chain(
            rows.package_attributions
                .iter()
                .filter(|attribution| {
                    attribution.status == PackageAttributionStatus::Accepted
                        && attribution.emission_mode == PackageEmissionMode::ExternalImport
                })
                .map(|attribution| attribution.module_id),
        )
        .collect::<BTreeSet<_>>();
    let decision_reasons = report
        .version_matches
        .iter()
        .map(|decision| {
            (
                decision_package_name(decision).to_string(),
                rejection_reason_from_decision(decision),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let modules_by_id = rows
        .modules
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let (outgoing_dependencies, _incoming_dependencies) = dependency_indexes(rows);

    let mut rejected = Vec::new();
    for module in &rows.modules {
        if module.kind != ModuleKind::Package || accepted_modules.contains(&module.id) {
            continue;
        }
        let package_name =
            module
                .package_name
                .as_deref()
                .ok_or(MatchPackagesError::InvalidAttribution {
                    module_id: module.id,
                    message: "package module has no package_name".to_string(),
                })?;
        if !matched_package_names.contains(package_name) {
            continue;
        }

        let match_evidence = report
            .matches
            .iter()
            .find(|package_match| package_match.module_id == module.id);
        let external_import_match =
            match_evidence.filter(|package_match| package_match.external_importable);
        let source_only_match =
            match_evidence.filter(|package_match| !package_match.external_importable);
        let reason = external_import_match
            .map(|_| {
                "matched package external import, but at least one non-externalized consumer still depends on this module"
            })
            .or_else(|| {
                source_only_match
            .filter(|package_match| {
                matches!(
                    package_match.strategy,
                    ModuleMatchStrategy::AggregateFunctionSignatureAndStringAnchors
                        | ModuleMatchStrategy::CascadeFunctionCoverage
                        | ModuleMatchStrategy::CascadeFunctionOwnership
                        | ModuleMatchStrategy::CascadePartialFunctionCoverage
                        | ModuleMatchStrategy::AggregateStructuralBagSimilarity
                        | ModuleMatchStrategy::DependencyClosureOwnership
                )
            })
            .map(|_| {
                "matched package ownership, but the evidence does not prove a safe single external import"
            })
            })
            .or_else(|| {
                package_source_quality_rejection_reason(
                    rows,
                    module,
                    package_name,
                    &modules_by_id,
                    &outgoing_dependencies,
                )
            })
            .or_else(|| decision_reasons.get(package_name).map(String::as_str))
            .unwrap_or("package matcher did not produce an accepted attribution for this package");
        let mut attribution =
            PackageAttributionInput::rejected_source(module.id, package_name, reason);
        if let Some(package_match) = match_evidence {
            attribution.package_version = Some(package_match.package_version.clone());
            if package_match.external_importable {
                attribution.export_specifier = Some(package_match.export_specifier.clone());
                attribution.resolved_file = Some(package_match.source_path.clone());
            }
        }
        rejected.push(attribution);
    }
    Ok(rejected)
}

fn filter_unsafe_interpackage_external_attributions(
    rows: &InputRows,
    report: &mut VersionedPackageMatchReport,
) -> ExternalImportSafetyReport {
    let modules_by_id = rows
        .modules
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let (outgoing_dependencies, incoming_dependencies) = dependency_indexes(rows);
    let ownership_proven_modules = package_ownership_proven_modules(rows, report, &modules_by_id);
    let report_external_modules = report
        .attributions
        .iter()
        .filter(|attribution| is_accepted_external_attribution(attribution))
        .map(|attribution| attribution.module_id)
        .collect::<BTreeSet<_>>();
    if report_external_modules.is_empty() {
        return ExternalImportSafetyReport::default();
    }
    let mut accepted_external_modules = rows
        .package_attributions
        .iter()
        .filter(|attribution| is_accepted_external_attribution(attribution))
        .map(|attribution| attribution.module_id)
        .chain(report_external_modules.iter().copied())
        .collect::<BTreeSet<_>>();
    let mut rejected = BTreeSet::<ModuleId>::new();

    loop {
        let mut changed = false;
        let source_suppressed_closure = external_import_source_suppressed_package_closure(
            &accepted_external_modules,
            &ownership_proven_modules,
            &modules_by_id,
            &outgoing_dependencies,
            &incoming_dependencies,
        );
        let source_boundary_modules = external_import_source_boundary_modules(
            &accepted_external_modules,
            &source_suppressed_closure,
            &modules_by_id,
            &incoming_dependencies,
        );
        for module_id in &report_external_modules {
            if rejected.contains(module_id) {
                continue;
            }
            if external_attribution_has_unexternalized_consumer(
                *module_id,
                &accepted_external_modules,
                &source_suppressed_closure,
                &source_boundary_modules,
                &modules_by_id,
                &incoming_dependencies,
            ) {
                rejected.insert(*module_id);
                accepted_external_modules.remove(module_id);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    if rejected.is_empty() {
        return ExternalImportSafetyReport::default();
    }
    let source_suppressed_closure = external_import_source_suppressed_package_closure(
        &accepted_external_modules,
        &ownership_proven_modules,
        &modules_by_id,
        &outgoing_dependencies,
        &incoming_dependencies,
    );
    let blockers = external_import_blocker_summaries(
        &rejected,
        &accepted_external_modules,
        &source_suppressed_closure,
        &external_import_source_boundary_modules(
            &accepted_external_modules,
            &source_suppressed_closure,
            &modules_by_id,
            &incoming_dependencies,
        ),
        &ownership_proven_modules,
        &modules_by_id,
        &incoming_dependencies,
    );
    let before = report.attributions.len();
    report
        .attributions
        .retain(|attribution| !rejected.contains(&attribution.module_id));
    ExternalImportSafetyReport {
        removed_modules: before.saturating_sub(report.attributions.len()),
        blockers,
    }
}

#[cfg(test)]
fn source_eliminated_package_modules_for_report(
    rows: &InputRows,
    report: &VersionedPackageMatchReport,
) -> usize {
    let loaded_package_modules = rows
        .modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
        .count();
    package_source_elimination_stats_for_report(rows, report, loaded_package_modules)
        .source_eliminated_package_modules
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct PackageSourceEliminationStats {
    direct_external_import_modules: usize,
    private_source_suppressed_package_modules: usize,
    source_eliminated_package_modules: usize,
    remaining_package_source_modules: usize,
}

fn package_source_elimination_stats_for_report(
    rows: &InputRows,
    report: &VersionedPackageMatchReport,
    loaded_package_modules: usize,
) -> PackageSourceEliminationStats {
    let accepted_external_modules = rows
        .package_attributions
        .iter()
        .chain(report.attributions.iter())
        .filter(|attribution| is_accepted_external_attribution(attribution))
        .map(|attribution| attribution.module_id)
        .collect::<BTreeSet<_>>();
    if accepted_external_modules.is_empty() {
        return PackageSourceEliminationStats {
            remaining_package_source_modules: loaded_package_modules,
            ..PackageSourceEliminationStats::default()
        };
    }
    let modules_by_id = rows
        .modules
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let (outgoing_dependencies, incoming_dependencies) = dependency_indexes(rows);
    let ownership_proven_modules = package_ownership_proven_modules(rows, report, &modules_by_id);
    let source_eliminated_modules = external_import_source_suppressed_package_closure(
        &accepted_external_modules,
        &ownership_proven_modules,
        &modules_by_id,
        &outgoing_dependencies,
        &incoming_dependencies,
    );
    let direct_external_import_modules = accepted_external_modules
        .iter()
        .filter(|module_id| {
            modules_by_id
                .get(module_id)
                .is_some_and(|module| module.kind == ModuleKind::Package)
        })
        .count();
    let source_eliminated_package_modules = source_eliminated_modules.len();
    PackageSourceEliminationStats {
        direct_external_import_modules,
        private_source_suppressed_package_modules: source_eliminated_package_modules
            .saturating_sub(direct_external_import_modules),
        source_eliminated_package_modules,
        remaining_package_source_modules: loaded_package_modules
            .saturating_sub(source_eliminated_package_modules),
    }
}

fn package_ownership_proven_modules(
    rows: &InputRows,
    report: &VersionedPackageMatchReport,
    modules_by_id: &BTreeMap<ModuleId, &ModuleInput>,
) -> BTreeSet<ModuleId> {
    let mut proven = BTreeSet::new();
    for attribution in &rows.package_attributions {
        if let Some(module) = modules_by_id.get(&attribution.module_id).copied()
            && package_attribution_proves_module_ownership(attribution, module)
        {
            proven.insert(attribution.module_id);
        }
    }
    for package_match in &report.matches {
        if let Some(module) = modules_by_id.get(&package_match.module_id).copied()
            && package_match_proves_module_ownership(package_match, module)
        {
            proven.insert(package_match.module_id);
        }
    }
    proven
}

fn package_attribution_proves_module_ownership(
    attribution: &PackageAttributionInput,
    module: &ModuleInput,
) -> bool {
    if module.kind != ModuleKind::Package
        || module.package_name.as_deref() != Some(attribution.package_name.as_str())
    {
        return false;
    }
    if let Some(attribution_version) = attribution.package_version.as_deref()
        && module
            .package_version
            .as_deref()
            .is_some_and(|module_version| {
                !module_version.trim().is_empty() && module_version != attribution_version
            })
    {
        return false;
    }
    is_accepted_external_attribution(attribution)
        || (attribution.status == PackageAttributionStatus::Rejected
            && attribution.emission_mode == PackageEmissionMode::ApplicationSource
            && attribution.package_version.is_some())
}

fn package_match_proves_module_ownership(
    package_match: &PackageMatch,
    module: &ModuleInput,
) -> bool {
    module.kind == ModuleKind::Package
        && module.package_name.as_deref() == Some(package_match.package_name.as_str())
        && module
            .package_version
            .as_deref()
            .is_none_or(|module_version| {
                module_version.trim().is_empty() || module_version == package_match.package_version
            })
}

fn is_accepted_external_attribution(attribution: &PackageAttributionInput) -> bool {
    attribution.status == PackageAttributionStatus::Accepted
        && attribution.emission_mode == PackageEmissionMode::ExternalImport
}

fn external_import_blocker_summaries(
    rejected: &BTreeSet<ModuleId>,
    accepted_external_modules: &BTreeSet<ModuleId>,
    source_suppressed_closure: &BTreeSet<ModuleId>,
    source_boundary_modules: &BTreeSet<ModuleId>,
    ownership_proven_modules: &BTreeSet<ModuleId>,
    modules_by_id: &BTreeMap<ModuleId, &ModuleInput>,
    incoming_dependencies: &BTreeMap<ModuleId, Vec<ModuleId>>,
) -> Vec<ExternalImportBlockerSummary> {
    let mut counts = BTreeMap::<(String, String), usize>::new();
    for module_id in rejected {
        let Some(module) = modules_by_id.get(module_id).copied() else {
            continue;
        };
        for consumer_id in incoming_dependencies.get(module_id).into_iter().flatten() {
            if accepted_external_modules.contains(consumer_id)
                || source_suppressed_closure.contains(consumer_id)
                || source_boundary_modules.contains(consumer_id)
            {
                continue;
            }
            let Some(consumer) = modules_by_id.get(consumer_id).copied() else {
                continue;
            };
            if external_import_consumer_is_boundary(module, consumer) {
                continue;
            }
            let reason = match consumer.kind {
                ModuleKind::Application => continue,
                ModuleKind::Package if !ownership_proven_modules.contains(consumer_id) => {
                    "package consumer ownership not proven"
                }
                ModuleKind::Package => "package consumer not externalized",
                ModuleKind::Builtin => "builtin consumer not externalized",
            };
            let label = module_consumer_label(consumer);
            *counts.entry((reason.to_string(), label)).or_default() += 1;
        }
    }
    let mut blockers = counts
        .into_iter()
        .map(|((reason, consumer), count)| ExternalImportBlockerSummary {
            reason,
            consumer,
            count,
        })
        .collect::<Vec<_>>();
    blockers.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.reason.cmp(&right.reason))
            .then_with(|| left.consumer.cmp(&right.consumer))
    });
    blockers
}

fn module_consumer_label(module: &ModuleInput) -> String {
    match module.kind {
        ModuleKind::Package => {
            let package = module
                .package_name
                .as_deref()
                .unwrap_or("<unknown-package>");
            let version = module
                .package_version
                .as_deref()
                .unwrap_or("<unknown-version>");
            format!(
                "{package}@{version} module={} path={}",
                module.id.0, module.semantic_path
            )
        }
        ModuleKind::Application => {
            format!(
                "application module={} path={}",
                module.id.0, module.semantic_path
            )
        }
        ModuleKind::Builtin => {
            format!(
                "builtin module={} path={}",
                module.id.0, module.semantic_path
            )
        }
    }
}

fn external_attribution_has_unexternalized_consumer(
    module_id: ModuleId,
    accepted_external_modules: &BTreeSet<ModuleId>,
    source_suppressed_closure: &BTreeSet<ModuleId>,
    source_boundary_modules: &BTreeSet<ModuleId>,
    modules_by_id: &BTreeMap<ModuleId, &ModuleInput>,
    incoming_dependencies: &BTreeMap<ModuleId, Vec<ModuleId>>,
) -> bool {
    let Some(module) = modules_by_id.get(&module_id).copied() else {
        return false;
    };
    for consumer_id in incoming_dependencies.get(&module_id).into_iter().flatten() {
        if accepted_external_modules.contains(consumer_id)
            || source_suppressed_closure.contains(consumer_id)
            || source_boundary_modules.contains(consumer_id)
        {
            continue;
        }
        if modules_by_id
            .get(consumer_id)
            .is_some_and(|consumer| !external_import_consumer_is_boundary(module, consumer))
        {
            return true;
        }
    }
    false
}

fn external_import_source_boundary_modules(
    accepted_external_modules: &BTreeSet<ModuleId>,
    source_suppressed_closure: &BTreeSet<ModuleId>,
    modules_by_id: &BTreeMap<ModuleId, &ModuleInput>,
    incoming_dependencies: &BTreeMap<ModuleId, Vec<ModuleId>>,
) -> BTreeSet<ModuleId> {
    let mut boundary = modules_by_id
        .iter()
        .filter_map(|(module_id, module)| {
            (module.kind == ModuleKind::Package
                && !accepted_external_modules.contains(module_id)
                && !source_suppressed_closure.contains(module_id))
            .then_some(*module_id)
        })
        .collect::<BTreeSet<_>>();
    loop {
        let mut removed = Vec::new();
        for module_id in &boundary {
            let Some(module) = modules_by_id.get(module_id).copied() else {
                continue;
            };
            let has_unresolved_same_package_consumer = incoming_dependencies
                .get(module_id)
                .into_iter()
                .flatten()
                .any(|consumer_id| {
                    modules_by_id.get(consumer_id).is_some_and(|consumer| {
                        consumer.kind == ModuleKind::Package
                            && same_package_consumer(module, consumer)
                            && !accepted_external_modules.contains(consumer_id)
                            && !source_suppressed_closure.contains(consumer_id)
                            && !boundary.contains(consumer_id)
                    })
                });
            if has_unresolved_same_package_consumer {
                removed.push(*module_id);
            }
        }
        if removed.is_empty() {
            break;
        }
        for module_id in removed {
            boundary.remove(&module_id);
        }
    }
    boundary
}

fn external_import_consumer_is_boundary(module: &ModuleInput, consumer: &ModuleInput) -> bool {
    match consumer.kind {
        ModuleKind::Application => true,
        ModuleKind::Package => !same_package_consumer(module, consumer),
        ModuleKind::Builtin => true,
    }
}

fn source_suppressed_consumer_is_boundary(module: &ModuleInput, consumer: &ModuleInput) -> bool {
    match consumer.kind {
        ModuleKind::Application => true,
        ModuleKind::Package => !same_package_consumer(module, consumer),
        ModuleKind::Builtin => false,
    }
}

fn same_package_consumer(module: &ModuleInput, consumer: &ModuleInput) -> bool {
    let Some(module_package) = module.package_name.as_deref().map(str::trim) else {
        return false;
    };
    let Some(consumer_package) = consumer.package_name.as_deref().map(str::trim) else {
        return false;
    };
    !module_package.is_empty() && module_package == consumer_package
}

fn external_import_source_suppressed_package_closure(
    accepted_external_modules: &BTreeSet<ModuleId>,
    ownership_proven_modules: &BTreeSet<ModuleId>,
    modules_by_id: &BTreeMap<ModuleId, &ModuleInput>,
    outgoing_dependencies: &BTreeMap<ModuleId, Vec<ModuleId>>,
    incoming_dependencies: &BTreeMap<ModuleId, Vec<ModuleId>>,
) -> BTreeSet<ModuleId> {
    let mut reachable = accepted_external_modules
        .iter()
        .copied()
        .filter(|module_id| {
            modules_by_id
                .get(module_id)
                .is_some_and(|module| module.kind == ModuleKind::Package)
        })
        .collect::<BTreeSet<_>>();
    let mut stack = reachable.iter().copied().collect::<Vec<_>>();
    while let Some(module_id) = stack.pop() {
        for dependency_id in outgoing_dependencies
            .get(&module_id)
            .into_iter()
            .flatten()
            .copied()
        {
            let Some(dependency) = modules_by_id.get(&dependency_id) else {
                continue;
            };
            if dependency.kind != ModuleKind::Package
                || !ownership_proven_modules.contains(&dependency_id)
                || !reachable.insert(dependency_id)
            {
                continue;
            }
            stack.push(dependency_id);
        }
    }

    let seed_modules = accepted_external_modules.clone();
    loop {
        let mut removed = Vec::new();
        for module_id in &reachable {
            if seed_modules.contains(module_id) {
                continue;
            }
            let Some(module) = modules_by_id.get(module_id).copied() else {
                continue;
            };
            let has_external_consumer = incoming_dependencies
                .get(module_id)
                .into_iter()
                .flatten()
                .any(|consumer_id| {
                    modules_by_id.get(consumer_id).is_some_and(|consumer| {
                        !reachable.contains(consumer_id)
                            && !source_suppressed_consumer_is_boundary(module, consumer)
                    })
                });
            if has_external_consumer {
                removed.push(*module_id);
            }
        }
        if removed.is_empty() {
            break;
        }
        for module_id in removed {
            reachable.remove(&module_id);
        }
    }
    reachable
}

fn externalization_chain_proofs(
    rows: &InputRows,
    report: &VersionedPackageMatchReport,
) -> BTreeMap<ModuleId, serde_json::Value> {
    let modules_by_id = rows
        .modules
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let (outgoing_dependencies, incoming_dependencies) = dependency_indexes(rows);
    let accepted_external_modules = rows
        .package_attributions
        .iter()
        .chain(report.attributions.iter())
        .filter(|attribution| is_accepted_external_attribution(attribution))
        .map(|attribution| attribution.module_id)
        .collect::<BTreeSet<_>>();
    let ownership_proven_modules = package_ownership_proven_modules(rows, report, &modules_by_id);
    let source_suppressed_closure = external_import_source_suppressed_package_closure(
        &accepted_external_modules,
        &ownership_proven_modules,
        &modules_by_id,
        &outgoing_dependencies,
        &incoming_dependencies,
    );
    let source_boundary_modules = external_import_source_boundary_modules(
        &accepted_external_modules,
        &source_suppressed_closure,
        &modules_by_id,
        &incoming_dependencies,
    );
    report
        .attributions
        .iter()
        .filter(|attribution| is_accepted_external_attribution(attribution))
        .filter_map(|attribution| {
            let module = modules_by_id.get(&attribution.module_id).copied()?;
            let suppressed_dependencies = source_suppressed_dependencies_for_seed(
                attribution.module_id,
                &accepted_external_modules,
                &source_suppressed_closure,
                &outgoing_dependencies,
            );
            let incoming_consumers = incoming_dependencies
                .get(&attribution.module_id)
                .into_iter()
                .flatten()
                .filter_map(|consumer_id| {
                    let consumer = modules_by_id.get(consumer_id).copied()?;
                    let resolution = if accepted_external_modules.contains(consumer_id) {
                        "direct_externalized"
                    } else if source_suppressed_closure.contains(consumer_id) {
                        "source_suppressed"
                    } else if consumer.kind == ModuleKind::Application {
                        "application_boundary"
                    } else if consumer.kind == ModuleKind::Builtin {
                        "builtin_boundary"
                    } else if external_import_consumer_is_boundary(module, consumer) {
                        "package_boundary"
                    } else if source_boundary_modules.contains(consumer_id) {
                        "source_boundary"
                    } else {
                        "unresolved"
                    };
                    Some(serde_json::json!({
                        "module_id": consumer_id.0,
                        "kind": module_kind_label(consumer.kind),
                        "package_name": consumer.package_name.as_deref(),
                        "package_version": consumer.package_version.as_deref(),
                        "semantic_path": consumer.semantic_path.as_str(),
                        "resolution": resolution,
                    }))
                })
                .take(64)
                .collect::<Vec<_>>();
            Some((
                attribution.module_id,
                serde_json::json!({
                    "proof_model": "externalization_chain_v1",
                    "direct_seed_module_id": attribution.module_id.0,
                    "direct_seed_kind": module_kind_label(module.kind),
                    "ownership_proof": "direct_external_import",
                    "all_incoming_consumers_resolved": incoming_consumers
                        .iter()
                        .all(|consumer| consumer
                            .get("resolution")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|resolution| resolution != "unresolved")),
                    "incoming_consumers": incoming_consumers,
                    "source_suppressed_dependency_count": suppressed_dependencies.len(),
                    "source_suppressed_dependency_module_ids": suppressed_dependencies
                        .iter()
                        .take(64)
                        .map(|module_id| module_id.0)
                        .collect::<Vec<_>>(),
                }),
            ))
        })
        .collect()
}

fn source_suppressed_dependencies_for_seed(
    seed: ModuleId,
    accepted_external_modules: &BTreeSet<ModuleId>,
    source_suppressed_closure: &BTreeSet<ModuleId>,
    outgoing_dependencies: &BTreeMap<ModuleId, Vec<ModuleId>>,
) -> BTreeSet<ModuleId> {
    let mut dependencies = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut stack = outgoing_dependencies
        .get(&seed)
        .into_iter()
        .flatten()
        .copied()
        .collect::<Vec<_>>();
    while let Some(module_id) = stack.pop() {
        if !visited.insert(module_id) || !source_suppressed_closure.contains(&module_id) {
            continue;
        }
        if !accepted_external_modules.contains(&module_id) {
            dependencies.insert(module_id);
        }
        stack.extend(
            outgoing_dependencies
                .get(&module_id)
                .into_iter()
                .flatten()
                .copied(),
        );
    }
    dependencies
}

const fn module_kind_label(kind: ModuleKind) -> &'static str {
    match kind {
        ModuleKind::Application => "application",
        ModuleKind::Package => "package",
        ModuleKind::Builtin => "builtin",
    }
}

fn package_source_quality_rejection_reason(
    rows: &InputRows,
    module: &ModuleInput,
    package_name: &str,
    modules_by_id: &BTreeMap<ModuleId, &ModuleInput>,
    outgoing_dependencies: &BTreeMap<ModuleId, Vec<ModuleId>>,
) -> Option<&'static str> {
    let Some(slice) = rows.module_source_slice(module.id) else {
        return Some(
            "package module has no source slice, so package ownership could not be verified",
        );
    };
    let quality = package_module_source_quality(module, slice.source_file_path, slice.source);
    if quality == PackageModuleSourceQuality::Invalid {
        return Some(
            "package module source slice is not parseable, so package ownership could not be verified",
        );
    }
    if quality != PackageModuleSourceQuality::Weak {
        return None;
    }
    let mut same_package_dependencies = 0usize;
    let mut other_package_dependencies = 0usize;
    for dependency_id in outgoing_dependencies.get(&module.id).into_iter().flatten() {
        let Some(dependency) = modules_by_id.get(dependency_id) else {
            continue;
        };
        let Some(dependency_package_name) = dependency.package_name.as_deref() else {
            continue;
        };
        if dependency_package_name == package_name {
            same_package_dependencies += 1;
        } else {
            other_package_dependencies += 1;
        }
    }
    if other_package_dependencies > 0 && same_package_dependencies == 0 {
        Some(
            "package hint is weak and direct dependency graph points at other packages; no safe package ownership match was accepted",
        )
    } else {
        Some(
            "package hint is weak because the module source does not contain strong package path tokens; no package ownership evidence matched",
        )
    }
}

fn decision_package_name(decision: &BestVersionMatch) -> &str {
    match decision {
        BestVersionMatch::Selected { score, .. }
        | BestVersionMatch::InsufficientEvidence { score } => score.package_name.as_str(),
        BestVersionMatch::Ambiguous { package_name, .. }
        | BestVersionMatch::NoMatch { package_name, .. } => package_name.as_str(),
    }
}

fn rejection_reason_from_decision(decision: &BestVersionMatch) -> String {
    match decision {
        BestVersionMatch::Selected { module_matches, .. }
            if module_matches
                .iter()
                .all(|module_match| !module_match.external_importable) =>
        {
            "selected package source is source-only and has not been proven external-importable"
                .to_string()
        }
        BestVersionMatch::Selected { .. } => {
            "selected package version did not match this module source".to_string()
        }
        BestVersionMatch::Ambiguous { .. } => {
            "package version matching found more than one best version".to_string()
        }
        BestVersionMatch::NoMatch { scores, .. } if scores.is_empty() => {
            "no cached package source was available for this package".to_string()
        }
        BestVersionMatch::NoMatch { .. } => {
            "package version matching found no usable evidence".to_string()
        }
        BestVersionMatch::InsufficientEvidence { .. } => {
            "package version evidence did not satisfy the acceptance threshold".to_string()
        }
    }
}

struct PackageDiagnosticsContext<'a> {
    modules_by_id: BTreeMap<ModuleId, &'a ModuleInput>,
    ownership_by_module: BTreeMap<ModuleId, (String, String)>,
    outgoing_dependencies: BTreeMap<ModuleId, Vec<ModuleId>>,
    incoming_dependencies: BTreeMap<ModuleId, Vec<ModuleId>>,
    version_decisions_by_package: BTreeMap<String, &'a BestVersionMatch>,
}

impl<'a> PackageDiagnosticsContext<'a> {
    fn new(rows: &'a InputRows, report: &'a VersionedPackageMatchReport) -> Self {
        let modules_by_id = rows
            .modules
            .iter()
            .map(|module| (module.id, module))
            .collect::<BTreeMap<_, _>>();
        let (outgoing_dependencies, incoming_dependencies) = dependency_indexes(rows);
        let version_decisions_by_package = report
            .version_matches
            .iter()
            .map(|decision| (decision_package_name(decision).to_string(), decision))
            .collect::<BTreeMap<_, _>>();
        Self {
            modules_by_id,
            ownership_by_module: ownership_by_module(rows, report),
            outgoing_dependencies,
            incoming_dependencies,
            version_decisions_by_package,
        }
    }
}

fn dependency_indexes(
    rows: &InputRows,
) -> (
    BTreeMap<ModuleId, Vec<ModuleId>>,
    BTreeMap<ModuleId, Vec<ModuleId>>,
) {
    let mut outgoing = BTreeMap::<ModuleId, Vec<ModuleId>>::new();
    let mut incoming = BTreeMap::<ModuleId, Vec<ModuleId>>::new();
    for dependency in &rows.dependencies {
        let ModuleDependencyTarget::Module(target) = dependency.target else {
            continue;
        };
        outgoing
            .entry(dependency.from_module_id)
            .or_default()
            .push(target);
        incoming
            .entry(target)
            .or_default()
            .push(dependency.from_module_id);
    }
    (outgoing, incoming)
}

fn unmatched_package_diagnostics(
    rows: &InputRows,
    context: &PackageDiagnosticsContext<'_>,
    module: &ModuleInput,
) -> serde_json::Value {
    let package_name = module.package_name.as_deref().unwrap_or_default();
    serde_json::json!({
        "module_id": module.id.0,
        "module_original_name": module.original_name,
        "semantic_path": module.semantic_path,
        "package_hint": {
            "package_name": module.package_name,
            "package_version": module.package_version,
        },
        "source_slice": source_slice_diagnostics(rows, module),
        "dependency_neighborhood": dependency_neighborhood_diagnostics(context, module.id),
        "version_decision": version_decision_diagnostics(context, package_name),
    })
}

fn source_slice_diagnostics(rows: &InputRows, module: &ModuleInput) -> serde_json::Value {
    let Some(slice) = rows.module_source_slice(module.id) else {
        return serde_json::json!({
            "available": false,
            "source_file_id": module.source_file_id,
            "has_source_span": module.source_span.is_some(),
            "reason": "missing_or_ambiguous_source_slice",
        });
    };
    let quality = package_module_source_quality(module, slice.source_file_path, slice.source);
    serde_json::json!({
        "available": true,
        "source_file_id": slice.source_file_id,
        "source_file_path": slice.source_file_path,
        "has_source_span": slice.span.is_some(),
        "byte_start": slice.span.map(|span| span.byte_start),
        "byte_end": slice.span.map(|span| span.byte_end),
        "source_len": slice.source.len(),
        "quality": package_source_quality_label(quality),
        "function_count": FunctionExtractor::function_count(module.id, slice.source),
    })
}

fn package_source_quality_label(quality: PackageModuleSourceQuality) -> &'static str {
    match quality {
        PackageModuleSourceQuality::Trusted => "trusted",
        PackageModuleSourceQuality::Weak => "weak",
        PackageModuleSourceQuality::Invalid => "invalid",
    }
}

fn dependency_neighborhood_diagnostics(
    context: &PackageDiagnosticsContext<'_>,
    module_id: ModuleId,
) -> serde_json::Value {
    let outgoing_ids = context
        .outgoing_dependencies
        .get(&module_id)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let incoming_ids = context
        .incoming_dependencies
        .get(&module_id)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let outgoing = dependency_package_summary(
        outgoing_ids,
        &context.modules_by_id,
        &context.ownership_by_module,
    );
    let incoming = dependency_package_summary(
        incoming_ids,
        &context.modules_by_id,
        &context.ownership_by_module,
    );
    serde_json::json!({
        "outgoing_package_counts": outgoing.package_counts.clone(),
        "incoming_package_counts": incoming.package_counts.clone(),
        "outgoing_owned_package_counts": outgoing.owned_package_counts.clone(),
        "incoming_owned_package_counts": incoming.owned_package_counts.clone(),
        "outgoing": dependency_package_summary_json(&outgoing),
        "incoming": dependency_package_summary_json(&incoming),
    })
}

#[derive(Debug, Clone)]
struct DependencyPackageSummary {
    module_count: usize,
    package_counts: BTreeMap<String, usize>,
    owned_module_count: usize,
    owned_package_counts: BTreeMap<String, usize>,
    owned_version_counts: BTreeMap<String, usize>,
}

fn dependency_package_summary(
    module_ids: &[ModuleId],
    modules_by_id: &BTreeMap<ModuleId, &ModuleInput>,
    ownership_by_module: &BTreeMap<ModuleId, (String, String)>,
) -> DependencyPackageSummary {
    let mut seen = BTreeSet::new();
    let mut package_counts = BTreeMap::<String, usize>::new();
    let mut owned_package_counts = BTreeMap::<String, usize>::new();
    let mut owned_version_counts = BTreeMap::<String, usize>::new();
    let mut module_count = 0usize;
    let mut owned_module_count = 0usize;
    for module_id in module_ids.iter().copied() {
        if !seen.insert(module_id) {
            continue;
        }
        let Some(module) = modules_by_id.get(&module_id) else {
            continue;
        };
        module_count += 1;
        if let Some(package_name) = module.package_name.as_deref() {
            *package_counts.entry(package_name.to_string()).or_default() += 1;
        }
        let Some((owned_package_name, owned_package_version)) = ownership_by_module.get(&module_id)
        else {
            continue;
        };
        owned_module_count += 1;
        *owned_package_counts
            .entry(owned_package_name.clone())
            .or_default() += 1;
        *owned_version_counts
            .entry(format!("{owned_package_name}@{owned_package_version}"))
            .or_default() += 1;
    }
    DependencyPackageSummary {
        module_count,
        package_counts,
        owned_module_count,
        owned_package_counts,
        owned_version_counts,
    }
}

fn dependency_package_summary_json(summary: &DependencyPackageSummary) -> serde_json::Value {
    serde_json::json!({
        "module_count": summary.module_count,
        "package_counts": summary.package_counts,
        "owned_module_count": summary.owned_module_count,
        "owned_package_counts": summary.owned_package_counts,
        "owned_version_counts": summary.owned_version_counts,
    })
}

fn version_decision_diagnostics(
    context: &PackageDiagnosticsContext<'_>,
    package_name: &str,
) -> serde_json::Value {
    let Some(decision) = context.version_decisions_by_package.get(package_name) else {
        return serde_json::json!({
            "kind": "not_evaluated",
            "top_scores": [],
        });
    };
    let mut scores = decision_scores(decision);
    scores.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| right.package_version.cmp(&left.package_version))
    });
    let top_scores = scores
        .into_iter()
        .take(3)
        .map(version_score_json)
        .collect::<Vec<_>>();
    serde_json::json!({
        "kind": decision_kind(decision),
        "reason": rejection_reason_from_decision(decision),
        "top_scores": top_scores,
    })
}

fn decision_kind(decision: &BestVersionMatch) -> &'static str {
    match decision {
        BestVersionMatch::Selected { .. } => "selected",
        BestVersionMatch::Ambiguous { .. } => "ambiguous",
        BestVersionMatch::NoMatch { .. } => "no_match",
        BestVersionMatch::InsufficientEvidence { .. } => "insufficient_evidence",
    }
}

fn decision_scores(decision: &BestVersionMatch) -> Vec<&VersionMatchScore> {
    match decision {
        BestVersionMatch::Selected { score, .. }
        | BestVersionMatch::InsufficientEvidence { score } => vec![score],
        BestVersionMatch::Ambiguous { scores, .. } | BestVersionMatch::NoMatch { scores, .. } => {
            scores.iter().collect()
        }
    }
}

fn version_score_json(score: &VersionMatchScore) -> serde_json::Value {
    serde_json::json!({
        "package_name": score.package_name,
        "package_version": score.package_version,
        "score": score.score,
        "total_modules": score.total_modules,
        "matched_modules": score.matched_modules,
        "source_hash_matches": score.source_hash_matches,
        "function_signature_matches": score.function_signature_matches,
        "string_anchor_matches": score.string_anchor_matches,
        "binary_search_probes": score.binary_search_probes,
    })
}

fn persist_package_attribution(
    connection: &Connection,
    module_original_name: &str,
    attribution: &PackageAttributionInput,
    module_match: &PackageMatch,
    version_resolution: Option<&PackageVersionResolutionEvidence>,
    externalization_chain: Option<&serde_json::Value>,
) -> Result<(), MatchPackagesError> {
    let package_version =
        attribution
            .package_version
            .as_deref()
            .ok_or(MatchPackagesError::InvalidAttribution {
                module_id: attribution.module_id,
                message: "accepted package attribution has no package version".to_string(),
            })?;
    let export_specifier =
        attribution
            .export_specifier
            .as_deref()
            .ok_or(MatchPackagesError::InvalidAttribution {
                module_id: attribution.module_id,
                message: "accepted external package attribution has no export specifier"
                    .to_string(),
            })?;
    let version_resolution = version_resolution.map(|resolution| {
        serde_json::json!({
            "requested_version": resolution.requested_version,
            "resolved_version": resolution.resolved_version,
            "reason": resolution.reason,
        })
    });
    let evidence = serde_json::json!({
        "matcher": "exact_normalized_source_binary_search",
        "package_name": module_match.package_name,
        "package_version": module_match.package_version,
        "export_specifier": module_match.export_specifier,
        "source_path": module_match.source_path,
        "normalized_source_hash": module_match.normalized_source_hash,
        "match_strategy": module_match.strategy.as_str(),
        "external_import_proof": external_import_proof_kind(module_match.source_path.as_str()),
        "version_resolution": version_resolution,
        "function_signature_matches": module_match.function_signature_matches,
        "string_anchor_matches": module_match.string_anchor_matches,
        "writes_package_version": true,
        "external_import_policy_version": PACKAGE_ATTRIBUTION_EXTERNAL_IMPORT_POLICY_VERSION,
        "externalization_chain": externalization_chain,
    })
    .to_string();
    connection
        .execute(
            r"
            INSERT INTO package_attributions
                (module_id, module_original_name, package_name, package_version,
                 package_subpath, resolved_file, export_specifier, emission_mode,
                 status, evidence_json, rejection_reason, external_import_policy_version,
                 created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'external_import',
                    'accepted', ?8, NULL, ?9, datetime('now'), datetime('now'))
            ON CONFLICT(module_id) DO UPDATE SET
                module_original_name = excluded.module_original_name,
                package_name = excluded.package_name,
                package_version = excluded.package_version,
                package_subpath = excluded.package_subpath,
                resolved_file = excluded.resolved_file,
                export_specifier = excluded.export_specifier,
                emission_mode = excluded.emission_mode,
                status = excluded.status,
                evidence_json = excluded.evidence_json,
                rejection_reason = excluded.rejection_reason,
                external_import_policy_version = excluded.external_import_policy_version,
                updated_at = datetime('now')
            ",
            params![
                i64::from(attribution.module_id.0),
                module_original_name,
                attribution.package_name.as_str(),
                package_version,
                attribution.subpath.as_deref(),
                module_match.source_path.as_str(),
                export_specifier,
                evidence,
                PACKAGE_ATTRIBUTION_EXTERNAL_IMPORT_POLICY_VERSION,
            ],
        )
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(())
}

fn persist_rejected_package_attribution(
    connection: &Connection,
    module_original_name: &str,
    attribution: &PackageAttributionInput,
    module_match: Option<&PackageMatch>,
    unmatched_diagnostics: serde_json::Value,
) -> Result<(), MatchPackagesError> {
    let rejection_reason =
        attribution
            .rejection_reason
            .as_deref()
            .ok_or(MatchPackagesError::InvalidAttribution {
                module_id: attribution.module_id,
                message: "rejected package attribution has no rejection reason".to_string(),
            })?;
    if rejection_reason.trim().is_empty() {
        return Err(MatchPackagesError::InvalidAttribution {
            module_id: attribution.module_id,
            message: "rejected package attribution has empty rejection reason".to_string(),
        });
    }

    let match_evidence = module_match.map(|module_match| {
        serde_json::json!({
            "package_name": module_match.package_name,
            "package_version": module_match.package_version,
            "export_specifier": module_match.export_specifier,
            "source_path": module_match.source_path,
            "normalized_source_hash": module_match.normalized_source_hash,
            "match_strategy": module_match.strategy.as_str(),
            "function_signature_matches": module_match.function_signature_matches,
            "string_anchor_matches": module_match.string_anchor_matches,
            "external_importable": module_match.external_importable,
        })
    });
    let evidence = serde_json::json!({
        "matcher": "package_ownership_matcher",
        "package_name": attribution.package_name,
        "package_version": attribution.package_version,
        "status": "rejected",
        "rejection_reason": rejection_reason,
        "ownership_match": match_evidence,
        "unmatched_diagnostics": unmatched_diagnostics,
        "writes_external_import": false,
    })
    .to_string();
    let resolved_file = module_match.map(|module_match| module_match.source_path.as_str());
    connection
        .execute(
            r"
            INSERT INTO package_attributions
                (module_id, module_original_name, package_name, package_version,
                 package_subpath, resolved_file, export_specifier, emission_mode,
                 status, evidence_json, rejection_reason, external_import_policy_version,
                 created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'application_source',
                    'rejected', ?8, ?9, 0, datetime('now'), datetime('now'))
            ON CONFLICT(module_id) DO UPDATE SET
                module_original_name = excluded.module_original_name,
                package_name = excluded.package_name,
                package_version = excluded.package_version,
                package_subpath = excluded.package_subpath,
                resolved_file = excluded.resolved_file,
                export_specifier = excluded.export_specifier,
                emission_mode = excluded.emission_mode,
                status = excluded.status,
                evidence_json = excluded.evidence_json,
                rejection_reason = excluded.rejection_reason,
                external_import_policy_version = excluded.external_import_policy_version,
                updated_at = datetime('now')
            ",
            params![
                i64::from(attribution.module_id.0),
                module_original_name,
                attribution.package_name.as_str(),
                attribution.package_version.as_deref(),
                attribution.subpath.as_deref(),
                resolved_file,
                attribution.export_specifier.as_deref(),
                evidence,
                rejection_reason,
            ],
        )
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(())
}

fn external_import_proof_kind(source_path: &str) -> &'static str {
    external_import_proof_label(source_path)
}

/// Persist bundle-extraction synthetic modules into the SQLite `modules`
/// table. Required when `apply: true` so that function-level attribution rows
/// can satisfy their `module_id REFERENCES modules(id)` foreign key.
///
/// Uses `INSERT OR IGNORE` to allow idempotent re-runs: if a previous
/// run already wrote a row with the same `(file_id, original_name)`,
/// the duplicate is silently skipped. Synthetic-id collisions across
/// runs are avoided by the shared pipeline preparation allocator starting past
/// the max real module id at load time.
fn persist_synthetic_modules(
    connection: &mut Connection,
    synthetic_modules: &[reverts_input::ModuleInput],
) -> Result<usize, MatchPackagesError> {
    if synthetic_modules.is_empty() {
        return Ok(0);
    }
    let transaction = connection
        .transaction()
        .map_err(MatchPackagesError::WriteAttribution)?;
    let mut written = 0usize;
    for module in synthetic_modules {
        let Some(span) = module.source_span else {
            continue;
        };
        let kind_str = match module.kind {
            ModuleKind::Application => "application",
            ModuleKind::Package => "package",
            ModuleKind::Builtin => "builtin",
        };
        let n = transaction
            .execute(
                r"
                INSERT OR IGNORE INTO modules
                    (id, file_id, original_name, semantic_name, module_category,
                     package_name, package_version, byte_start, byte_end,
                     created_at, updated_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                        datetime('now'), datetime('now'))
                ",
                params![
                    module.id.0,
                    module.source_file_id,
                    module.original_name,
                    module.semantic_path,
                    kind_str,
                    module.package_name,
                    module.package_version,
                    span.byte_start,
                    span.byte_end,
                ],
            )
            .map_err(MatchPackagesError::WriteAttribution)?;
        written += n;
    }
    transaction
        .commit()
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(written)
}

fn persist_function_attributions(
    connection: &mut Connection,
    rows: &InputRows,
    attributions: &[PackageAttributionInput],
) -> Result<usize, MatchPackagesError> {
    if attributions.is_empty() {
        return Ok(0);
    }
    ensure_package_function_attributions_table(connection)?;

    let modules_by_id: BTreeMap<ModuleId, &str> = rows
        .modules
        .iter()
        .map(|m| (m.id, m.original_name.as_str()))
        .collect();

    let transaction = connection
        .transaction()
        .map_err(MatchPackagesError::WriteAttribution)?;
    let mut written = 0;

    for attribution in attributions {
        let Some(function_span) = attribution.function_span else {
            // Function-level attribution requires a span; matcher code only
            // emits rows with `with_function_span(...)`, so this is a programmer
            // error rather than user input — surface it instead of skipping.
            return Err(MatchPackagesError::InvalidAttribution {
                module_id: attribution.module_id,
                message: "function attribution missing function_span".to_string(),
            });
        };
        let Some(confidence) = attribution.confidence.as_ref() else {
            return Err(MatchPackagesError::InvalidAttribution {
                module_id: attribution.module_id,
                message: "function attribution missing confidence".to_string(),
            });
        };
        let module_original_name = modules_by_id.get(&attribution.module_id).copied().ok_or(
            MatchPackagesError::MissingModuleForAttribution {
                module_id: attribution.module_id,
            },
        )?;
        let package_version = attribution.package_version.as_deref().ok_or(
            MatchPackagesError::InvalidAttribution {
                module_id: attribution.module_id,
                message: "function attribution missing package version".to_string(),
            },
        )?;
        let export_specifier = attribution.export_specifier.as_deref().ok_or(
            MatchPackagesError::InvalidAttribution {
                module_id: attribution.module_id,
                message: "function attribution missing export specifier".to_string(),
            },
        )?;
        let matched_axes_json = serde_json::Value::Array(
            confidence
                .matched_axes
                .iter()
                .map(|a| serde_json::Value::String(a.as_str().to_string()))
                .collect(),
        )
        .to_string();
        let matched_alternate = confidence.matched_alternate.map(|p| p.as_str().to_string());

        transaction
            .execute(
                r"
                INSERT INTO package_function_attributions
                    (module_id, module_original_name, package_name, package_version,
                     export_specifier, function_span_start, function_span_end,
                     tier, matched_alternate, matched_axes_json,
                     top_score, runner_up_score, margin,
                     created_at, updated_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                        datetime('now'), datetime('now'))
                ON CONFLICT(module_id, function_span_start, function_span_end) DO UPDATE SET
                    module_original_name = excluded.module_original_name,
                    package_name = excluded.package_name,
                    package_version = excluded.package_version,
                    export_specifier = excluded.export_specifier,
                    tier = excluded.tier,
                    matched_alternate = excluded.matched_alternate,
                    matched_axes_json = excluded.matched_axes_json,
                    top_score = excluded.top_score,
                    runner_up_score = excluded.runner_up_score,
                    margin = excluded.margin,
                    updated_at = datetime('now')
                ",
                params![
                    i64::from(attribution.module_id.0),
                    module_original_name,
                    attribution.package_name.as_str(),
                    package_version,
                    export_specifier,
                    i64::from(function_span.start),
                    i64::from(function_span.end),
                    confidence.tier.as_str(),
                    matched_alternate,
                    matched_axes_json,
                    confidence.top_score,
                    confidence.runner_up_score,
                    confidence.margin,
                ],
            )
            .map_err(MatchPackagesError::WriteAttribution)?;
        written += 1;
    }

    transaction
        .commit()
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(written)
}

fn ensure_package_function_attributions_table(
    connection: &mut Connection,
) -> Result<(), MatchPackagesError> {
    connection
        .execute_batch(PACKAGE_FUNCTION_ATTRIBUTIONS_CREATE_SQL)
        .map_err(MatchPackagesError::WriteAttribution)?;
    if package_function_attributions_requires_alt_tier_migration(connection)
        .map_err(MatchPackagesError::WriteAttribution)?
    {
        migrate_package_function_attributions_alt_tier(connection)?;
    }
    connection
        .execute_batch(PACKAGE_FUNCTION_ATTRIBUTIONS_INDEX_SQL)
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(())
}

/// True when the existing `package_function_attributions` table was
/// created before any of the alt-tier names were added, i.e. its
/// CHECK constraint does not list one of the expected values.
/// Detected by peeking at the persisted `sql` text in `sqlite_master`.
fn package_function_attributions_requires_alt_tier_migration(
    connection: &Connection,
) -> rusqlite::Result<bool> {
    let sql: Option<String> = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='package_function_attributions'",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    Ok(sql
        .map(|s| {
            !s.contains("structural_anchored_alternate")
                || !s.contains("feature_similarity_alternate")
                || !s.contains("structural_only_alternate")
        })
        .unwrap_or(false))
}

fn migrate_package_function_attributions_alt_tier(
    connection: &mut Connection,
) -> Result<(), MatchPackagesError> {
    let transaction = connection
        .transaction()
        .map_err(MatchPackagesError::WriteAttribution)?;
    transaction
        .execute_batch(
            r"
            ALTER TABLE package_function_attributions
                RENAME TO package_function_attributions__reverts_old;
            ",
        )
        .map_err(MatchPackagesError::WriteAttribution)?;
    transaction
        .execute_batch(PACKAGE_FUNCTION_ATTRIBUTIONS_CREATE_SQL)
        .map_err(MatchPackagesError::WriteAttribution)?;
    transaction
        .execute_batch(
            r"
            INSERT INTO package_function_attributions
            SELECT * FROM package_function_attributions__reverts_old;
            DROP TABLE package_function_attributions__reverts_old;
            ",
        )
        .map_err(MatchPackagesError::WriteAttribution)?;
    transaction
        .commit()
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(())
}

const PACKAGE_FUNCTION_ATTRIBUTIONS_CREATE_SQL: &str = r"
CREATE TABLE IF NOT EXISTS package_function_attributions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    module_id INTEGER NOT NULL,
    module_original_name TEXT NOT NULL,
    package_name TEXT NOT NULL,
    package_version TEXT NOT NULL,
    export_specifier TEXT NOT NULL,
    function_span_start INTEGER NOT NULL,
    function_span_end INTEGER NOT NULL,
    tier TEXT NOT NULL,
    matched_alternate TEXT,
    matched_axes_json TEXT NOT NULL,
    top_score REAL NOT NULL,
    runner_up_score REAL NOT NULL,
    margin REAL NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (module_id, function_span_start, function_span_end),
    FOREIGN KEY (module_id) REFERENCES modules(id) ON DELETE CASCADE,
    CHECK (TRIM(module_original_name) != ''),
    CHECK (TRIM(package_name) != ''),
    CHECK (TRIM(package_version) != ''),
    CHECK (TRIM(export_specifier) != ''),
    CHECK (function_span_start <= function_span_end),
    CHECK (tier IN (
        'exact',
        'exact_alternate',
        'structural_anchored',
        'structural_anchored_alternate',
        'feature_similarity',
        'feature_similarity_alternate',
        'structural_only',
        'structural_only_alternate'
    )),
    CHECK (margin >= 0.0 AND margin <= 1.0)
);
";

const PACKAGE_FUNCTION_ATTRIBUTIONS_INDEX_SQL: &str = r"
CREATE INDEX IF NOT EXISTS idx_package_function_attributions_module
    ON package_function_attributions(module_id);
CREATE INDEX IF NOT EXISTS idx_package_function_attributions_package
    ON package_function_attributions(package_name, package_version);
CREATE INDEX IF NOT EXISTS idx_package_function_attributions_tier
    ON package_function_attributions(tier);
";

fn persist_package_surfaces(
    connection: &mut Connection,
    rows: &InputRows,
    report: &VersionedPackageMatchReport,
) -> Result<usize, MatchPackagesError> {
    if report.surfaces.is_empty() {
        return Ok(0);
    }

    ensure_package_surfaces_table(connection)?;
    let transaction = connection
        .transaction()
        .map_err(MatchPackagesError::WritePackageSurface)?;
    let mut written = 0;
    for surface in &report.surfaces {
        persist_package_surface(&transaction, rows.project.id, surface)?;
        written += 1;
    }
    transaction
        .commit()
        .map_err(MatchPackagesError::WritePackageSurface)?;
    Ok(written)
}

fn ensure_package_surfaces_table(connection: &Connection) -> Result<(), MatchPackagesError> {
    connection
        .execute_batch(
            r"
            CREATE TABLE IF NOT EXISTS package_surfaces (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_id INTEGER NOT NULL,
                package_name TEXT NOT NULL,
                package_version TEXT NOT NULL,
                export_specifier TEXT NOT NULL,
                status TEXT NOT NULL,
                evidence_json TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                UNIQUE (project_id, export_specifier)
            );
            ",
        )
        .map_err(MatchPackagesError::WritePackageSurface)
}

fn persist_package_surface(
    connection: &Connection,
    project_id: u32,
    surface: &reverts_input::PackageSurfaceInput,
) -> Result<(), MatchPackagesError> {
    let package_version =
        surface
            .package_version
            .as_deref()
            .ok_or(MatchPackagesError::InvalidPackageSurface {
                export_specifier: surface.export_specifier.clone(),
                message: "accepted package surface has no package version".to_string(),
            })?;
    let evidence = surface.evidence.clone().unwrap_or_else(|| {
        serde_json::json!({
            "matcher": "source_package_import_surface",
            "package_name": surface.package_name.as_str(),
            "package_version": package_version,
            "export_specifier": surface.export_specifier.as_str(),
        })
        .to_string()
    });
    connection
        .execute(
            r"
            INSERT INTO package_surfaces
                (project_id, package_name, package_version, export_specifier,
                 status, evidence_json, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, 'accepted', ?5, datetime('now'), datetime('now'))
            ON CONFLICT(project_id, export_specifier) DO UPDATE SET
                package_name = excluded.package_name,
                package_version = excluded.package_version,
                status = excluded.status,
                evidence_json = excluded.evidence_json,
                updated_at = datetime('now')
            ",
            params![
                i64::from(project_id),
                surface.package_name.as_str(),
                package_version,
                surface.export_specifier.as_str(),
                evidence,
            ],
        )
        .map_err(MatchPackagesError::WritePackageSurface)?;
    Ok(())
}

fn sqlite_table_exists(connection: &Connection, table: &str) -> rusqlite::Result<bool> {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_row| Ok(()),
        )
        .optional()
        .map(|value| value.is_some())
}

fn sqlite_table_has_column(
    connection: &Connection,
    table: &str,
    column: &str,
) -> rusqlite::Result<bool> {
    let mut statement = connection.prepare(format!("PRAGMA table_info({table})").as_str())?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    for name in columns {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn collect_sqlite_rows<T>(
    rows: impl Iterator<Item = rusqlite::Result<T>>,
) -> rusqlite::Result<Vec<T>> {
    rows.collect()
}

fn sqlite_placeholders(count: usize) -> String {
    (0..count).map(|_| "?").collect::<Vec<_>>().join(", ")
}

fn dedup_audit_report(audit: AuditReport) -> AuditReport {
    let mut deduped = AuditReport::default();
    let mut seen = BTreeSet::new();
    for finding in audit.findings() {
        let key = (
            format!("{:?}", finding.code),
            format!("{:?}", finding.severity),
            finding.module.clone(),
            finding.binding.clone(),
            finding.message.clone(),
        );
        if seen.insert(key) {
            deduped.push(finding.clone());
        }
    }
    deduped
}

pub(crate) fn format_audit_findings(audit: &AuditReport) -> String {
    audit
        .findings()
        .iter()
        .take(20)
        .map(|finding| {
            format!(
                "{:?}: {}{}{}",
                finding.code,
                finding.message,
                finding
                    .module
                    .as_deref()
                    .map(|module| format!(" module={module}"))
                    .unwrap_or_default(),
                finding
                    .binding
                    .as_deref()
                    .map(|binding| format!(" binding={binding}"))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::path::{Path, PathBuf};

    use reverts_input::{
        InputRows, ModuleDependencyInput, ModuleDependencyTarget, ModuleInput,
        PACKAGE_ATTRIBUTION_EXTERNAL_IMPORT_POLICY_VERSION, PackageAttributionInput, ProjectInput,
        SourceFileInput,
    };
    use reverts_ir::{BindingName, ModuleId, ModuleKind};
    use reverts_observe::{AuditFinding, AuditReport, FindingCode};
    use reverts_package_matcher::{
        ModuleMatchStrategy, PackageMatch, PackageSource, VersionedPackageMatchReport,
    };
    use reverts_pipeline::{
        EmittedAsset, EmittedFile, RuntimeDependency, RuntimeSetterMigrationBlockerReason,
        RuntimeSetterMigrationBlockerReport,
    };
    use rusqlite::{Connection, params};

    use super::commands::generate_project::{checked_output_path, write_emitted_project};
    use super::{
        CliCommand, CliError, ExtractAssetsArgs, GenerateProjectV2Args, HelpTopic,
        MatchPackagesArgs, MatchPackagesError, PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION,
        PackageExternalizationHintsArgs, PackageVersionResolutionPlan, RuntimeInventoryArgs,
        RuntimeSourceSpanOwner, best_matching_package_version_by_binary_search,
        collect_local_package_sources, dedup_audit_report, externalization_chain_proofs,
        extract_bun_embedded_asset_from_bytes, filter_package_sources_to_best_build_variants,
        filter_package_sources_to_relevant_path_hints,
        filter_unsafe_interpackage_external_attributions, help_text, json_package_source_module,
        load_package_sources, local_package_metadata, match_packages_from_connection,
        network_package_version_resolution_hints, package_export_specifier,
        package_externalization_hints_from_connection, package_graph_component_scope,
        package_source_elimination_stats_for_report, package_version_hints_for_materialization,
        package_version_resolution_evidence, package_versions_by_module, parse_npm_versions_json,
        persist_package_source_cache, promote_package_sources_with_externalization_hints,
        remove_package_attributions_for_revalidation,
        resolve_package_version_hints_to_available_sources, run,
        runtime_emitted_setter_blockers_from_files, runtime_inventory_counts_from_files,
        runtime_inventory_project_selections, runtime_line_attribution_from_files,
        runtime_module_owner_label, runtime_original_name_owners_by_binding,
        runtime_source_span_owner_label_for_range, source_eliminated_package_modules_for_report,
        stable_hash, stale_cache_version_hints_for_materialization,
        stale_package_source_cache_versions, version_text,
    };

    #[test]
    fn parses_generate_project_v2_paths_without_external_process() {
        let args = GenerateProjectV2Args::parse([
            "generate-project-v2".to_string(),
            "--input".to_string(),
            "input.db".to_string(),
            "--project-id".to_string(),
            "13495".to_string(),
            "--output".to_string(),
            "out".to_string(),
        ])
        .expect("args should parse");

        assert_eq!(args.input, PathBuf::from("input.db"));
        assert_eq!(args.project_id, 13495);
        assert_eq!(args.output, PathBuf::from("out"));
    }

    #[test]
    fn project_id_must_be_positive_integer() {
        let error = GenerateProjectV2Args::parse([
            "--input".to_string(),
            "input.db".to_string(),
            "--project-id".to_string(),
            "0".to_string(),
            "--output".to_string(),
            "out".to_string(),
        ]);

        assert!(matches!(error, Err(CliError::InvalidProjectId(value)) if value == "0"));
    }

    #[test]
    fn parses_match_packages_command_without_version_suffix() {
        let args = MatchPackagesArgs::parse([
            "match-packages".to_string(),
            "--input".to_string(),
            "input.db".to_string(),
            "--project-id".to_string(),
            "13495".to_string(),
            "--package-name".to_string(),
            "pkg".to_string(),
            "--package-source-root".to_string(),
            "node_modules".to_string(),
            "--materialize-package-sources".to_string(),
            "--apply".to_string(),
        ])
        .expect("args should parse");

        assert_eq!(args.input, PathBuf::from("input.db"));
        assert_eq!(args.project_id, 13495);
        assert_eq!(args.package_names, vec!["pkg"]);
        assert_eq!(
            args.package_source_roots,
            vec![PathBuf::from("node_modules")]
        );
        assert!(args.materialize_package_sources);
        assert!(args.apply);

        let old_command = CliCommand::parse(["match-packages-v2".to_string()]);
        assert!(
            matches!(old_command, Err(CliError::UnknownCommand(command)) if command == "match-packages-v2")
        );
    }

    #[test]
    fn parses_package_externalization_hints_command() {
        let args = PackageExternalizationHintsArgs::parse([
            "package-externalization-hints".to_string(),
            "--input".to_string(),
            "input.db".to_string(),
            "--package-name".to_string(),
            "pkg".to_string(),
            "--limit".to_string(),
            "50".to_string(),
            "--apply".to_string(),
        ])
        .expect("args should parse");

        assert_eq!(args.input, PathBuf::from("input.db"));
        assert_eq!(args.package_names, vec!["pkg"]);
        assert_eq!(args.limit, Some(50));
        assert!(args.apply);

        let command = CliCommand::parse([
            "package-externalization-hints".to_string(),
            "--input".to_string(),
            "input.db".to_string(),
        ])
        .expect("command should parse");
        assert!(matches!(
            command,
            CliCommand::PackageExternalizationHints(parsed) if parsed.input.as_path() == Path::new("input.db")
        ));
    }

    #[test]
    fn parses_extract_assets_command() {
        let args = ExtractAssetsArgs::parse([
            "extract-assets".to_string(),
            "--input".to_string(),
            "input.db".to_string(),
            "--project-id".to_string(),
            "13495".to_string(),
            "--asset-root".to_string(),
            "dist".to_string(),
            "--asset-root".to_string(),
            "vendor".to_string(),
            "--apply".to_string(),
        ])
        .expect("args should parse");

        assert_eq!(args.input, PathBuf::from("input.db"));
        assert_eq!(args.project_id, 13495);
        assert_eq!(
            args.asset_roots,
            vec![PathBuf::from("dist"), PathBuf::from("vendor")]
        );
        assert!(args.apply);
    }

    #[test]
    fn parses_runtime_inventory_command() {
        let args = RuntimeInventoryArgs::parse([
            "runtime-inventory".to_string(),
            "--input".to_string(),
            "input.db".to_string(),
            "--all-projects".to_string(),
            "--limit".to_string(),
            "25".to_string(),
            "--newest".to_string(),
            "--max-source-bytes".to_string(),
            "1000000".to_string(),
            "--setter-blockers".to_string(),
            "--runtime-attribution".to_string(),
        ])
        .expect("args should parse");

        assert_eq!(args.input, PathBuf::from("input.db"));
        assert_eq!(args.project_id, None);
        assert!(args.all_projects);
        assert_eq!(args.limit, Some(25));
        assert!(args.newest);
        assert_eq!(args.max_source_bytes, Some(1_000_000));
        assert!(args.setter_blockers);
        assert!(args.runtime_attribution);

        let command = CliCommand::parse([
            "runtime-inventory".to_string(),
            "--input".to_string(),
            "input.db".to_string(),
            "--project-id".to_string(),
            "13495".to_string(),
        ])
        .expect("command should parse");
        assert!(
            matches!(command, CliCommand::RuntimeInventory(parsed) if parsed.project_id == Some(13495))
        );
    }

    #[test]
    fn parses_top_level_help_and_version_without_required_command_args() {
        assert_eq!(
            CliCommand::parse(Vec::<String>::new()).expect("empty args should show help"),
            CliCommand::Help(HelpTopic::TopLevel)
        );
        assert_eq!(
            CliCommand::parse(["--help".to_string()]).expect("top-level help should parse"),
            CliCommand::Help(HelpTopic::TopLevel)
        );
        assert_eq!(
            CliCommand::parse(["-h".to_string()]).expect("short help should parse"),
            CliCommand::Help(HelpTopic::TopLevel)
        );
        assert_eq!(
            CliCommand::parse(["help".to_string()]).expect("help command should parse"),
            CliCommand::Help(HelpTopic::TopLevel)
        );
        assert_eq!(
            CliCommand::parse(["--version".to_string()]).expect("version should parse"),
            CliCommand::Version
        );
        assert_eq!(
            CliCommand::parse(["-V".to_string()]).expect("short version should parse"),
            CliCommand::Version
        );
        assert_eq!(
            CliCommand::parse(["version".to_string()]).expect("version command should parse"),
            CliCommand::Version
        );
    }

    #[test]
    fn parses_command_specific_help_without_running_command() {
        assert_eq!(
            CliCommand::parse(["generate-project-v2".to_string(), "--help".to_string()])
                .expect("generate help should parse"),
            CliCommand::Help(HelpTopic::GenerateProjectV2)
        );
        assert_eq!(
            CliCommand::parse(["match-packages".to_string(), "help".to_string()])
                .expect("match help should parse"),
            CliCommand::Help(HelpTopic::MatchPackages)
        );
        assert_eq!(
            CliCommand::parse(["help".to_string(), "extract-assets".to_string()])
                .expect("extract help should parse"),
            CliCommand::Help(HelpTopic::ExtractAssets)
        );
        assert_eq!(
            CliCommand::parse(["runtime-inventory".to_string(), "--help".to_string()])
                .expect("inventory help should parse"),
            CliCommand::Help(HelpTopic::RuntimeInventory)
        );
    }

    #[test]
    fn help_and_version_commands_return_ok() {
        run(["--help".to_string()]).expect("top-level help should not require a database");
        run(["help".to_string(), "extract-assets".to_string()])
            .expect("command help should not require a database");
        run(["--version".to_string()]).expect("version should not require a database");
    }

    #[test]
    fn help_text_documents_commands_and_options() {
        assert!(help_text(HelpTopic::TopLevel).contains("extract-assets"));
        assert!(help_text(HelpTopic::GenerateProjectV2).contains("--output <DIR>"));
        assert!(help_text(HelpTopic::MatchPackages).contains("--package-name <NAME>"));
        assert!(help_text(HelpTopic::MatchPackages).contains("--package-source-root <DIR>"));
        assert!(help_text(HelpTopic::MatchPackages).contains("--materialize-package-sources"));
        assert!(help_text(HelpTopic::MatchPackagesReport).contains("source_eliminated"));
        assert!(help_text(HelpTopic::ExtractAssets).contains("--asset-root <DIR-OR-BUN-EXE>"));
        assert!(help_text(HelpTopic::RuntimeInventory).contains("--all-projects"));
        assert!(version_text().starts_with("reverts-cli "));
    }

    #[test]
    fn runtime_inventory_counts_runtime_helpers_and_internal_names() {
        let files = vec![
            EmittedFile {
                path: "modules/runtime/source-1-helpers.ts".to_string(),
                source: "// @ts-nocheck\nexport { X } from '../real.js';\nfunction __reverts_set_X(value) { return X = value; }function __reverts_set_Y(value) { return Y = value; }\n".to_string(),
            },
            EmittedFile {
                path: "modules/consumer.ts".to_string(),
                source: "import { __reverts_set_X } from './runtime/source-1-helpers.js';\n__reverts_set_X(1);\n".to_string(),
            },
        ];

        let counts = runtime_inventory_counts_from_files(&files);

        assert_eq!(counts.files, 2);
        assert_eq!(counts.runtime_files, 1);
        assert_eq!(counts.runtime_lines, 3);
        assert_eq!(counts.runtime_import_statements, 1);
        assert_eq!(counts.runtime_reexport_statements, 1);
        assert_eq!(counts.setter_function_definitions, 2);
        assert_eq!(counts.setter_import_statements, 1);
        assert_eq!(counts.setter_occurrences, 4);
        assert_eq!(counts.reverts_internal_occurrences, 4);
        assert_eq!(counts.named_import_statements, 1);
        assert_eq!(counts.named_export_statements, 1);
    }

    #[test]
    fn runtime_inventory_counts_only_real_setter_declarations() {
        let files = vec![EmittedFile {
            path: "modules/runtime/source-1-helpers.ts".to_string(),
            source: "// function __reverts_set_comment(value) {}\n\
                     const text = 'function __reverts_set_string(value) {}';\n\
                     function __reverts_set_X(value) { return X = value; }function __reverts_set_Y(value) { return Y = value; }\n"
                .to_string(),
        }];

        let counts = runtime_inventory_counts_from_files(&files);

        assert_eq!(counts.setter_function_definitions, 2);
    }

    #[test]
    fn runtime_emitted_setter_blockers_count_batched_setter_declarations() {
        let files = vec![EmittedFile {
            path: "modules/runtime/source-1-helpers.ts".to_string(),
            source: "function __reverts_set_X(value) { return X = value; }function __reverts_set_Y(value) { return Y = value; }\n".to_string(),
        }];
        let mut report = RuntimeSetterMigrationBlockerReport {
            total_bindings: 2,
            ..Default::default()
        };
        report.add_reason(
            1,
            BindingName::new("X"),
            RuntimeSetterMigrationBlockerReason::ReaderNonSnippetUse,
        );
        report.add_reason(
            1,
            BindingName::new("Y"),
            RuntimeSetterMigrationBlockerReason::RuntimeNonSnippetRead,
        );

        let emitted = runtime_emitted_setter_blockers_from_files(&files, &report);

        assert_eq!(emitted.total_bindings, 2);
        assert_eq!(emitted.blocked_bindings, 2);
        assert_eq!(
            emitted
                .reasons
                .get(&RuntimeSetterMigrationBlockerReason::ReaderNonSnippetUse),
            Some(&1)
        );
        assert_eq!(
            emitted
                .reasons
                .get(&RuntimeSetterMigrationBlockerReason::RuntimeNonSnippetRead),
            Some(&1)
        );
    }

    #[test]
    fn runtime_line_attribution_reports_runtime_lines_by_kind_and_binding() {
        let files = vec![
            EmittedFile {
                path: "modules/runtime/source-1-helpers.ts".to_string(),
                source: "import { dep } from '../dep.js';\n\
                         var cached = lazyValue(() => ({ dep }));\n\
                         function __reverts_set_cached(value) { cached = value; return value; }\n\
                         function run() {\n  return cached();\n}\n\
                         class Box {}\n\
                         export { cached, run };\n"
                    .to_string(),
            },
            EmittedFile {
                path: "modules/consumer.ts".to_string(),
                source: "import { run } from './runtime/source-1-helpers.js';\nrun();\n"
                    .to_string(),
            },
        ];

        let package_ownership = BTreeMap::from([
            ((1, "cached".to_string()), "fixture@1.0.0".to_string()),
            ((1, "run".to_string()), "<application>".to_string()),
            ((1, "Box".to_string()), "ui@2.0.0".to_string()),
        ]);
        let report = runtime_line_attribution_from_files(&files, &package_ownership);

        assert_eq!(report.total_runtime_lines, 8);
        assert_eq!(report.unattributed_lines, 0);
        assert_eq!(report.by_kind["import"].lines, 1);
        assert_eq!(report.by_kind["lazy_value"].lines, 1);
        assert_eq!(report.by_kind["setter"].lines, 1);
        assert_eq!(report.by_kind["function"].lines, 3);
        assert_eq!(report.by_kind["class"].lines, 1);
        assert_eq!(report.by_kind["export"].lines, 1);
        assert_eq!(report.by_package["fixture@1.0.0"].lines, 2);
        assert_eq!(report.by_package["<application>"].lines, 3);
        assert_eq!(report.by_package["ui@2.0.0"].lines, 1);
        assert_eq!(report.by_package["<runtime-glue>"].lines, 2);
        assert!(
            report
                .items
                .iter()
                .any(|item| item.kind == "function" && item.binding == "run" && item.lines == 3),
            "top-level function span should be attributed to run: {:?}",
            report.items
        );
        assert!(
            report
                .items
                .iter()
                .all(|item| item.path.starts_with("modules/runtime/")),
            "only runtime files should be attributed"
        );
    }

    #[test]
    fn runtime_source_span_owner_matches_runtime_wrapper_that_overlaps_module_body() {
        let owners = vec![RuntimeSourceSpanOwner {
            byte_start: 25,
            byte_end: 125,
            label: "zod@3.24.2".to_string(),
        }];

        assert_eq!(
            runtime_source_span_owner_label_for_range(&owners, 10, 150).as_deref(),
            Some("zod@3.24.2")
        );
        assert_eq!(
            runtime_source_span_owner_label_for_range(&owners, 0, 10).as_deref(),
            None
        );
    }

    #[test]
    fn runtime_source_span_owner_reports_shared_cross_package_overlap() {
        let owners = vec![
            RuntimeSourceSpanOwner {
                byte_start: 25,
                byte_end: 75,
                label: "alpha@1.0.0".to_string(),
            },
            RuntimeSourceSpanOwner {
                byte_start: 80,
                byte_end: 125,
                label: "beta@2.0.0".to_string(),
            },
        ];

        assert_eq!(
            runtime_source_span_owner_label_for_range(&owners, 10, 150).as_deref(),
            Some("<shared>")
        );
    }

    #[test]
    fn runtime_module_owner_label_prefers_package_hint_on_application_modules() {
        let mut module = ModuleInput::application(ModuleId(7), "lazy", "lazy").with_source_file(1);
        module.package_name = Some("zod".to_string());
        module.package_version = Some("3.24.2".to_string());

        assert_eq!(runtime_module_owner_label(&module), "zod@3.24.2");
    }

    #[test]
    fn runtime_original_name_owner_labels_runtime_wrapper_by_module_name() {
        let mut module =
            ModuleInput::application(ModuleId(7), "kP7", "modules/7-kp7.ts").with_source_file(1);
        module.package_name = Some("zod".to_string());
        module.package_version = Some("3.24.2".to_string());

        let owners = runtime_original_name_owners_by_binding(&[module]);

        assert_eq!(
            owners
                .get(&(1, "kP7".to_string()))
                .and_then(|labels| labels.iter().next())
                .map(String::as_str),
            Some("zod@3.24.2")
        );
    }

    #[test]
    fn runtime_inventory_selects_project_source_sizes_with_limit_ordering() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let database_path = tempdir.path().join("input.db");
        let connection = Connection::open(database_path.as_path()).expect("open sqlite");
        connection
            .execute_batch(
                r"
                CREATE TABLE projects (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                CREATE TABLE source_files (
                    id INTEGER PRIMARY KEY,
                    file_path TEXT NOT NULL,
                    file_size INTEGER NOT NULL
                );
                CREATE TABLE project_files (project_id INTEGER NOT NULL, file_id INTEGER NOT NULL);

                INSERT INTO projects (id, name) VALUES (1, 'old'), (2, 'middle'), (3, 'new');
                INSERT INTO source_files (id, file_path, file_size)
                    VALUES (10, 'one.js', 100), (11, 'two.js', 25), (12, 'three.js', 7);
                INSERT INTO project_files (project_id, file_id)
                    VALUES (1, 10), (1, 11), (2, 12);
                ",
            )
            .expect("schema");

        let newest_args = RuntimeInventoryArgs {
            input: database_path.clone(),
            project_id: None,
            all_projects: true,
            limit: Some(2),
            newest: true,
            max_source_bytes: Some(10),
            setter_blockers: false,
            runtime_attribution: false,
        };
        let selections =
            runtime_inventory_project_selections(&newest_args).expect("select newest projects");

        assert_eq!(selections.len(), 2);
        assert_eq!(selections[0].project_id, 3);
        assert_eq!(selections[0].source_bytes, 0);
        assert_eq!(selections[1].project_id, 2);
        assert_eq!(selections[1].source_bytes, 7);

        let single_project_args = RuntimeInventoryArgs {
            input: database_path,
            project_id: Some(1),
            all_projects: false,
            limit: None,
            newest: false,
            max_source_bytes: None,
            setter_blockers: false,
            runtime_attribution: false,
        };
        let selections = runtime_inventory_project_selections(&single_project_args)
            .expect("select single project");

        assert_eq!(selections.len(), 1);
        assert_eq!(selections[0].project_id, 1);
        assert_eq!(selections[0].source_bytes, 125);
    }

    #[test]
    fn materialization_hints_resolve_non_exact_versions_from_project_and_cache() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "lodash",
            "node_modules/lodash/index.js",
            "lodash",
            Some("4.17.21".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(11),
            "rxjs",
            "node_modules/rxjs/index.js",
            "rxjs",
            Some("7.x".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(12),
            "zod",
            "node_modules/zod/index.js",
            "zod",
            Some("4.0.0".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(13),
            "react",
            "node_modules/react/index.js",
            "react",
            Some("latest".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(14),
            "protobufjs",
            "node_modules/protobufjs/index.js",
            "protobufjs",
            Some("7.x".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(15),
            "protobufjsExact",
            "node_modules/protobufjs/light.js",
            "protobufjs",
            Some("7.5.4".to_string()),
        ));
        let available_sources = [
            PackageSource::source_only(
                "rxjs",
                "7.8.2",
                "rxjs",
                "rxjs@7.8.2/index.js",
                "export {};",
            ),
            PackageSource::source_only(
                "protobufjs",
                "7.4.0",
                "protobufjs",
                "protobufjs@7.4.0/index.js",
                "export {};",
            ),
        ];

        let hints = package_version_hints_for_materialization(
            &rows,
            &BTreeSet::from([
                "lodash".to_string(),
                "rxjs".to_string(),
                "react".to_string(),
                "protobufjs".to_string(),
            ]),
            &available_sources,
        );

        assert_eq!(
            hints,
            BTreeSet::from([
                ("lodash".to_string(), "4.17.21".to_string()),
                ("protobufjs".to_string(), "7.5.4".to_string()),
            ])
        );
    }

    #[test]
    fn non_exact_package_versions_resolve_to_best_cached_version_before_matching() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "forgeRange",
            "node_modules/node-forge/index.js",
            "node-forge",
            Some("1.x".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(11),
            "forgeExact",
            "node_modules/node-forge/lib/aes.js",
            "node-forge",
            Some("1.3.1".to_string()),
        ));
        let package_sources = [
            PackageSource::source_only(
                "node-forge",
                "1.0.0",
                "node-forge",
                "node-forge@1.0.0/index.js",
                "export {};",
            ),
            PackageSource::source_only(
                "node-forge",
                "1.3.1",
                "node-forge",
                "node-forge@1.3.1/index.js",
                "export {};",
            ),
            PackageSource::source_only(
                "node-forge",
                "1.3.3",
                "node-forge",
                "node-forge@1.3.3/index.js",
                "export {};",
            ),
        ];

        let resolved = resolve_package_version_hints_to_available_sources(
            &mut rows,
            &package_sources,
            &BTreeSet::new(),
        )
        .expect("resolve package version hints");

        assert_eq!(resolved, 1);
        assert_eq!(rows.modules[0].package_version.as_deref(), Some("1.3.1"));
    }

    #[test]
    fn package_version_resolution_rejects_invalid_package_source_version() {
        let rows = InputRows::new(ProjectInput::new(1, "fixture"));
        let package_sources = [PackageSource::source_only(
            "pkg",
            "not-semver",
            "pkg",
            "pkg@not-semver/index.js",
            "export {};",
        )];

        let error = PackageVersionResolutionPlan::build(&rows, &BTreeSet::new(), &package_sources)
            .expect_err("invalid package source version should fail");

        assert!(matches!(
            error,
            MatchPackagesError::InvalidPackageSourceVersion { .. }
        ));
    }

    #[test]
    fn package_version_resolution_rejects_unparseable_package_source() {
        let rows = InputRows::new(ProjectInput::new(1, "fixture"));
        let package_sources = [PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg",
            "pkg@1.0.0/index.js",
            "const =",
        )];

        let error = PackageVersionResolutionPlan::build(&rows, &BTreeSet::new(), &package_sources)
            .expect_err("unparseable package source should fail");

        assert!(matches!(
            error,
            MatchPackagesError::NormalizePackageSource { .. }
        ));
    }

    #[test]
    fn impossible_non_exact_versions_do_not_resolve_to_project_exact_cached_version() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "otelImpossibleRange",
            "node_modules/@opentelemetry/otlp-exporter-base/index.js",
            "@opentelemetry/otlp-exporter-base",
            Some("1.x".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(11),
            "otelExactOlder",
            "node_modules/@opentelemetry/otlp-exporter-base/old.js",
            "@opentelemetry/otlp-exporter-base",
            Some("0.208.0".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(12),
            "otelExactCurrent",
            "node_modules/@opentelemetry/otlp-exporter-base/current.js",
            "@opentelemetry/otlp-exporter-base",
            Some("0.211.0".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(13),
            "otelExactCurrentAgain",
            "node_modules/@opentelemetry/otlp-exporter-base/current-again.js",
            "@opentelemetry/otlp-exporter-base",
            Some("0.211.0".to_string()),
        ));
        let package_sources = [
            PackageSource::source_only(
                "@opentelemetry/otlp-exporter-base",
                "0.208.0",
                "@opentelemetry/otlp-exporter-base",
                "@opentelemetry/otlp-exporter-base@0.208.0/index.js",
                "export {};",
            ),
            PackageSource::source_only(
                "@opentelemetry/otlp-exporter-base",
                "0.211.0",
                "@opentelemetry/otlp-exporter-base",
                "@opentelemetry/otlp-exporter-base@0.211.0/index.js",
                "export {};",
            ),
        ];
        let resolved = resolve_package_version_hints_to_available_sources(
            &mut rows,
            &package_sources,
            &BTreeSet::new(),
        )
        .expect("resolve package version hints");

        assert_eq!(resolved, 0);
        assert_eq!(rows.modules[0].package_version.as_deref(), Some("1.x"));
    }

    #[test]
    fn package_version_resolution_evidence_records_matching_range_resolution() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "pkgRange",
            "node_modules/pkg/index.js",
            "pkg",
            Some("1.x".to_string()),
        ));
        let before = package_versions_by_module(&rows);
        rows.modules[0].package_version = Some("1.2.3".to_string());

        let evidence = package_version_resolution_evidence(&before, &rows);
        let evidence = evidence
            .get(&ModuleId(10))
            .expect("resolution should be recorded");

        assert_eq!(evidence.requested_version.as_deref(), Some("1.x"));
        assert_eq!(evidence.resolved_version.as_str(), "1.2.3");
        assert_eq!(evidence.reason, "range_resolved_to_available_source");
    }

    #[test]
    fn impossible_non_exact_versions_do_not_resolve_when_project_exact_versions_exist() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "otelImpossibleRange",
            "node_modules/@opentelemetry/otlp-exporter-base/index.js",
            "@opentelemetry/otlp-exporter-base",
            Some("1.x".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(11),
            "otelExactOne",
            "node_modules/@opentelemetry/otlp-exporter-base/one.js",
            "@opentelemetry/otlp-exporter-base",
            Some("0.208.0".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(12),
            "otelExactTwo",
            "node_modules/@opentelemetry/otlp-exporter-base/two.js",
            "@opentelemetry/otlp-exporter-base",
            Some("0.211.0".to_string()),
        ));
        let package_sources = [
            PackageSource::source_only(
                "@opentelemetry/otlp-exporter-base",
                "0.208.0",
                "@opentelemetry/otlp-exporter-base",
                "@opentelemetry/otlp-exporter-base@0.208.0/index.js",
                "export {};",
            ),
            PackageSource::source_only(
                "@opentelemetry/otlp-exporter-base",
                "0.211.0",
                "@opentelemetry/otlp-exporter-base",
                "@opentelemetry/otlp-exporter-base@0.211.0/index.js",
                "export {};",
            ),
        ];
        assert_eq!(
            best_matching_package_version_by_binary_search(
                "1.x",
                &BTreeSet::from([
                    semver::Version::parse("0.208.0").expect("fixture version should parse"),
                    semver::Version::parse("0.211.0").expect("fixture version should parse"),
                ]),
            ),
            None
        );
        let resolved = resolve_package_version_hints_to_available_sources(
            &mut rows,
            &package_sources,
            &BTreeSet::new(),
        )
        .expect("resolve package version hints");

        assert_eq!(resolved, 0);
        assert_eq!(rows.modules[0].package_version.as_deref(), Some("1.x"));
    }

    #[test]
    fn unavailable_exact_package_versions_remain_unchanged_when_exact_source_missing() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "awsMissingExact",
            "node_modules/@aws-sdk/middleware-host-header/index.js",
            "@aws-sdk/middleware-host-header",
            Some("3.712.0".to_string()),
        ));
        let package_sources = [
            PackageSource::source_only(
                "@aws-sdk/middleware-host-header",
                "3.700.0",
                "@aws-sdk/middleware-host-header",
                "@aws-sdk/middleware-host-header@3.700.0/index.js",
                "export {};",
            ),
            PackageSource::source_only(
                "@aws-sdk/middleware-host-header",
                "3.711.0",
                "@aws-sdk/middleware-host-header",
                "@aws-sdk/middleware-host-header@3.711.0/index.js",
                "export {};",
            ),
            PackageSource::source_only(
                "@aws-sdk/middleware-host-header",
                "3.720.0",
                "@aws-sdk/middleware-host-header",
                "@aws-sdk/middleware-host-header@3.720.0/index.js",
                "export {};",
            ),
        ];

        let resolved = resolve_package_version_hints_to_available_sources(
            &mut rows,
            &package_sources,
            &BTreeSet::new(),
        )
        .expect("resolve package version hints");

        assert_eq!(resolved, 0);
        assert_eq!(rows.modules[0].package_version.as_deref(), Some("3.712.0"));
    }

    #[test]
    fn unavailable_exact_package_versions_do_not_rewrite_from_source_identity() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        let bundled_source = "export function add(a,b){return a+b}";
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(bundled_source.to_string()),
        ));
        rows.modules.push(
            ModuleInput::package(
                ModuleId(10),
                "pkgMissingExact",
                "node_modules/pkg/add.js",
                "pkg",
                Some("3.712.0".to_string()),
            )
            .with_source_file(1),
        );
        let package_sources = [
            PackageSource::source_only(
                "pkg",
                "3.711.0",
                "pkg/add.js",
                "pkg@3.711.0/add.js",
                "export function sub(a,b){return a-b}",
            ),
            PackageSource::source_only(
                "pkg",
                "3.720.0",
                "pkg/add.js",
                "pkg@3.720.0/add.js",
                "export function add(a, b) {\n  return a + b;\n}",
            ),
        ];

        let resolved = resolve_package_version_hints_to_available_sources(
            &mut rows,
            &package_sources,
            &BTreeSet::new(),
        )
        .expect("resolve package version hints");

        assert_eq!(resolved, 0);
        assert_eq!(rows.modules[0].package_version.as_deref(), Some("3.712.0"));
    }

    #[test]
    fn missing_package_versions_prefer_source_identity_over_latest_cached_version() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        let bundled_source = "export function add(a,b){return a+b}";
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(bundled_source.to_string()),
        ));
        rows.modules.push(
            ModuleInput::package(
                ModuleId(10),
                "pkgNoVersion",
                "node_modules/pkg/add.js",
                "pkg",
                None,
            )
            .with_source_file(1),
        );
        let package_sources = [
            PackageSource::source_only(
                "pkg",
                "1.0.0",
                "pkg/add.js",
                "pkg@1.0.0/add.js",
                "export function add(a, b) {\n  return a + b;\n}",
            ),
            PackageSource::source_only(
                "pkg",
                "2.0.0",
                "pkg/add.js",
                "pkg@2.0.0/add.js",
                "export function sub(a,b){return a-b}",
            ),
        ];

        let resolved = resolve_package_version_hints_to_available_sources(
            &mut rows,
            &package_sources,
            &BTreeSet::new(),
        )
        .expect("resolve package version hints");

        assert_eq!(resolved, 1);
        assert_eq!(rows.modules[0].package_version.as_deref(), Some("1.0.0"));
    }

    #[test]
    fn package_sources_filter_to_versions_referenced_after_resolution() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "pkgOne",
            "node_modules/pkg/index.js",
            "pkg",
            Some("1.0.0".to_string()),
        ));
        let mut package_sources = vec![
            PackageSource::source_only("pkg", "1.0.0", "pkg", "pkg@1.0.0/index.js", "export {};"),
            PackageSource::source_only("pkg", "2.0.0", "pkg", "pkg@2.0.0/index.js", "export {};"),
            PackageSource::source_only(
                "import-only",
                "3.0.0",
                "import-only",
                "import-only@3.0.0/index.js",
                "export {};",
            ),
        ];

        let removed = super::filter_package_sources_to_referenced_package_versions(
            &rows,
            &mut package_sources,
        );

        assert_eq!(removed, 1);
        assert_eq!(
            package_sources
                .iter()
                .map(|source| {
                    (
                        source.package_name.as_str(),
                        source.package_version.as_str(),
                    )
                })
                .collect::<Vec<_>>(),
            vec![("pkg", "1.0.0"), ("import-only", "3.0.0")]
        );
    }

    #[test]
    fn missing_package_versions_resolve_to_latest_cached_version() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "inkNoVersion",
            "node_modules/ink/index.js",
            "ink",
            None,
        ));
        let package_sources = [
            PackageSource::source_only("ink", "4.4.1", "ink", "ink@4.4.1/index.js", "export {};"),
            PackageSource::source_only("ink", "5.2.1", "ink", "ink@5.2.1/index.js", "export {};"),
        ];

        let resolved = resolve_package_version_hints_to_available_sources(
            &mut rows,
            &package_sources,
            &BTreeSet::new(),
        )
        .expect("resolve package version hints");

        assert_eq!(resolved, 1);
        assert_eq!(rows.modules[0].package_version.as_deref(), Some("5.2.1"));
    }

    #[test]
    fn best_matching_package_version_uses_binary_search_for_wildcards() {
        let versions = BTreeSet::from([
            semver::Version::parse("0.9.9").expect("fixture version should parse"),
            semver::Version::parse("1.0.0").expect("fixture version should parse"),
            semver::Version::parse("1.2.3").expect("fixture version should parse"),
            semver::Version::parse("1.9.9").expect("fixture version should parse"),
            semver::Version::parse("2.0.0").expect("fixture version should parse"),
        ]);

        let selected = best_matching_package_version_by_binary_search("1.x", &versions);

        assert_eq!(
            selected.as_ref().map(ToString::to_string).as_deref(),
            Some("1.9.9")
        );
    }

    #[test]
    fn network_version_resolution_requires_exact_match_for_exact_requests() {
        let versions = BTreeSet::from([
            semver::Version::parse("3.354.0").expect("fixture version should parse"),
            semver::Version::parse("3.370.0").expect("fixture version should parse"),
            semver::Version::parse("3.374.0").expect("fixture version should parse"),
        ]);

        let selected = super::resolve_package_version_hint_from_versions("3.712.0", &versions);

        assert_eq!(selected, None);
    }

    #[test]
    fn network_resolution_hints_include_missing_versions() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "inkNoVersion",
            "node_modules/ink/index.js",
            "ink",
            None,
        ));

        let hints = network_package_version_resolution_hints(&rows, &BTreeSet::new(), &[]);

        assert_eq!(
            hints,
            BTreeSet::from([("ink".to_string(), "latest".to_string())])
        );
    }

    #[test]
    fn requested_package_scope_expands_to_dependency_graph_component() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        for (module_id, package_name) in [(10, "alpha"), (11, "beta"), (12, "gamma"), (13, "delta")]
        {
            rows.modules.push(ModuleInput::package(
                ModuleId(module_id),
                package_name,
                format!("node_modules/{package_name}/index.js"),
                package_name,
                Some("1.0.0".to_string()),
            ));
        }
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(10),
            target: ModuleDependencyTarget::Module(ModuleId(11)),
        });
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(12),
            target: ModuleDependencyTarget::Module(ModuleId(11)),
        });

        let scope = package_graph_component_scope(&rows, &["alpha".to_string()]);

        assert_eq!(
            scope,
            BTreeSet::from(["alpha".to_string(), "beta".to_string(), "gamma".to_string(),]),
            "requested package matching must see the whole package dependency component, including reverse consumers"
        );
    }

    #[test]
    fn revalidation_removes_external_attributions_for_expanded_component() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.package_attributions
            .push(PackageAttributionInput::accepted_external(
                ModuleId(10),
                "alpha",
                "1.0.0",
                "alpha",
            ));
        rows.package_attributions
            .push(PackageAttributionInput::accepted_external(
                ModuleId(11),
                "beta",
                "1.0.0",
                "beta",
            ));
        rows.package_attributions
            .push(PackageAttributionInput::accepted_external(
                ModuleId(12),
                "delta",
                "1.0.0",
                "delta",
            ));

        let removed = remove_package_attributions_for_revalidation(
            &mut rows,
            &BTreeSet::from(["alpha".to_string(), "beta".to_string()]),
        );

        assert_eq!(removed, 2);
        assert_eq!(rows.package_attributions.len(), 1);
        assert_eq!(
            rows.package_attributions[0].package_name.as_str(),
            "delta",
            "external imports outside the expanded package component stay as existing proof"
        );
    }

    #[test]
    fn npm_versions_json_parser_accepts_arrays_and_single_versions() {
        let array_versions = parse_npm_versions_json("pkg", "1.x", br#"["1.0.0","1.2.3"]"#)
            .expect("array versions should parse");
        let single_version = parse_npm_versions_json("pkg", "latest", br#""2.0.0""#)
            .expect("single version should parse");

        assert!(
            array_versions
                .contains(&semver::Version::parse("1.2.3").expect("fixture version should parse"))
        );
        assert_eq!(
            single_version
                .iter()
                .next()
                .map(ToString::to_string)
                .as_deref(),
            Some("2.0.0")
        );
    }

    #[test]
    fn npm_versions_json_parser_rejects_invalid_version_entries() {
        let error = parse_npm_versions_json("pkg", "1.x", br#"["1.0.0","bad"]"#)
            .expect_err("invalid semver entry should fail");

        assert!(matches!(
            error,
            MatchPackagesError::MaterializePackageSource { .. }
        ));
    }

    #[test]
    fn package_source_cache_persists_external_importability() {
        let mut connection = Connection::open_in_memory().expect("open sqlite");
        connection
            .execute_batch(
                r"
                CREATE TABLE package_source_cache (
                    package_name TEXT NOT NULL,
                    package_version TEXT NOT NULL,
                    entry_path TEXT NOT NULL,
                    source_content TEXT NOT NULL,
                    content_hash TEXT NOT NULL,
                    fetched_at TEXT NOT NULL,
                    expires_at TEXT NOT NULL,
                    PRIMARY KEY (package_name, package_version)
                );
                ",
            )
            .expect("create legacy package source cache");
        let sources = vec![
            PackageSource::external(
                "pkg",
                "1.2.3",
                "pkg/public",
                "pkg@1.2.3/public.js",
                "export const publicValue = 1;",
            ),
            PackageSource::source_only(
                "pkg",
                "1.2.3",
                "pkg/internal",
                "pkg@1.2.3/internal.js",
                "export const internalValue = 2;",
            ),
        ];

        let written =
            persist_package_source_cache(&mut connection, &sources).expect("persist cache");

        assert_eq!(written, 2);
        let rows = InputRows::new(ProjectInput::new(1, "fixture"));
        let loaded = load_package_sources(
            &mut connection,
            &rows,
            &BTreeSet::from(["pkg".to_string()]),
            &[],
            false,
            false,
        )
        .expect("load cache");
        assert_eq!(loaded.len(), 2);
        assert!(
            loaded
                .iter()
                .any(|source| source.source_path.ends_with("public.js")
                    && source.export_specifier == "pkg/public"
                    && source.external_importable)
        );
        assert!(loaded.iter().any(
            |source| source.source_path.ends_with("internal.js") && !source.external_importable
        ));
    }

    #[test]
    fn load_package_sources_skips_cache_when_no_package_scope_exists() {
        let mut connection = Connection::open_in_memory().expect("open sqlite");
        connection
            .execute_batch(
                r"
                CREATE TABLE package_source_cache (
                    package_name TEXT NOT NULL,
                    package_version TEXT NOT NULL,
                    entry_path TEXT NOT NULL,
                    source_content TEXT NOT NULL,
                    content_hash TEXT NOT NULL,
                    fetched_at TEXT NOT NULL,
                    expires_at TEXT NOT NULL,
                    PRIMARY KEY (package_name, package_version, entry_path)
                );
                INSERT INTO package_source_cache
                    (package_name, package_version, entry_path, source_content, content_hash, fetched_at, expires_at)
                VALUES
                    ('huge', '1.0.0', 'index.js', 'export const value = 1;', 'h', 'now', 'later');
                ",
            )
            .expect("create package source cache");
        let rows = InputRows::new(ProjectInput::new(1, "fixture"));

        let loaded =
            load_package_sources(&mut connection, &rows, &BTreeSet::new(), &[], false, false)
                .expect("empty package scope should be accepted");

        assert!(loaded.is_empty());
    }

    #[test]
    fn package_source_cache_without_import_policy_version_is_source_only() {
        let mut connection = Connection::open_in_memory().expect("open sqlite");
        connection
            .execute_batch(
                r"
                CREATE TABLE package_source_cache (
                    package_name TEXT NOT NULL,
                    package_version TEXT NOT NULL,
                    entry_path TEXT NOT NULL,
                    source_content TEXT NOT NULL,
                    content_hash TEXT NOT NULL,
                    external_importable INTEGER NOT NULL DEFAULT 1,
                    fetched_at TEXT NOT NULL,
                    expires_at TEXT NOT NULL,
                    PRIMARY KEY (package_name, package_version, entry_path)
                );
                INSERT INTO package_source_cache
                    (package_name, package_version, entry_path, source_content,
                     content_hash, external_importable, fetched_at, expires_at)
                VALUES
                    ('pkg', '1.2.3', 'public.js', 'export const publicValue = 1;',
                     'hash', 1, 'now', 'later');
                ",
            )
            .expect("create stale package source cache");
        let rows = InputRows::new(ProjectInput::new(1, "fixture"));

        let loaded = load_package_sources(
            &mut connection,
            &rows,
            &BTreeSet::from(["pkg".to_string()]),
            &[],
            false,
            false,
        )
        .expect("load cache");

        assert_eq!(loaded.len(), 1);
        assert!(
            !loaded[0].external_importable,
            "stale cache rows must be revalidated/materialized before external import emission"
        );
    }

    #[test]
    fn externalization_hints_promote_source_only_cache_rows_when_proof_matches() {
        let connection = Connection::open_in_memory().expect("open sqlite");
        connection
            .execute_batch(
                r"
                CREATE TABLE package_externalization_hints (
                    package_name TEXT NOT NULL,
                    package_version TEXT NOT NULL,
                    entry_path TEXT NOT NULL,
                    export_specifier TEXT NOT NULL,
                    content_hash TEXT,
                    normalized_source_hash TEXT,
                    public_members_json TEXT,
                    proof_policy_version INTEGER NOT NULL
                );
                ",
            )
            .expect("create hints table");
        let source = "exports.Widget = function Widget(){ return 1; };";
        let content_hash = stable_hash(source.as_bytes());
        connection
            .execute(
                r#"
                INSERT INTO package_externalization_hints
                    (package_name, package_version, entry_path, export_specifier,
                     content_hash, public_members_json, proof_policy_version)
                VALUES ('pkg', '1.2.3', 'dist/index.cjs', 'pkg',
                        ?1, '["Widget"]', 1)
                "#,
                [content_hash],
            )
            .expect("insert hint");
        let mut sources = vec![PackageSource::source_only(
            "pkg",
            "1.2.3",
            "pkg",
            "pkg@1.2.3/dist/index.cjs",
            source,
        )];

        let promoted = promote_package_sources_with_externalization_hints(
            &connection,
            &BTreeSet::from(["pkg".to_string()]),
            &mut sources,
        )
        .expect("promote hints");

        assert_eq!(promoted, 1);
        assert!(sources.iter().any(|source| source.external_importable));
    }

    #[test]
    fn package_externalization_hints_command_persists_verified_cache_rows() {
        let mut connection = Connection::open_in_memory().expect("open sqlite");
        let source = "export const Widget = 1;";
        let content_hash = stable_hash(source.as_bytes());
        connection
            .execute_batch(
                r"
                CREATE TABLE package_source_cache (
                    package_name TEXT NOT NULL,
                    package_version TEXT NOT NULL,
                    entry_path TEXT NOT NULL,
                    source_content TEXT NOT NULL,
                    content_hash TEXT NOT NULL,
                    external_importable INTEGER NOT NULL DEFAULT 1,
                    external_import_policy_version INTEGER NOT NULL DEFAULT 0,
                    export_specifier TEXT NOT NULL DEFAULT '',
                    fetched_at TEXT NOT NULL,
                    expires_at TEXT NOT NULL,
                    PRIMARY KEY (package_name, package_version, entry_path)
                );
                ",
            )
            .expect("create cache table");
        connection
            .execute(
                r"
                INSERT INTO package_source_cache
                    (package_name, package_version, entry_path, source_content,
                     content_hash, external_importable, external_import_policy_version,
                     export_specifier, fetched_at, expires_at)
                VALUES ('pkg', '1.2.3', 'dist/index.js', ?1, ?2, 1, ?3,
                        'pkg/dist/index.js', 'now', 'later')
                ",
                params![
                    source,
                    content_hash.as_str(),
                    PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION,
                ],
            )
            .expect("insert cache row");

        let outcome = package_externalization_hints_from_connection(
            &mut connection,
            &PackageExternalizationHintsArgs {
                input: PathBuf::from("input.db"),
                package_names: vec!["pkg".to_string()],
                limit: None,
                apply: true,
            },
        )
        .expect("write hints");

        assert_eq!(outcome.scanned_rows, 1);
        assert_eq!(outcome.verified_rows, 1);
        assert_eq!(outcome.written_rows, 1);
        let (stored_hash, normalized_hash, members, policy): (String, String, String, i64) =
            connection
                .query_row(
                    r"
                SELECT content_hash, normalized_source_hash, public_members_json,
                       proof_policy_version
                  FROM package_externalization_hints
                 WHERE package_name = 'pkg'
                   AND package_version = '1.2.3'
                   AND entry_path = 'dist/index.js'
                ",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .expect("hint row");
        assert_eq!(stored_hash, content_hash);
        assert!(!normalized_hash.is_empty());
        assert!(members.contains("Widget"));
        assert_eq!(policy, 1);
    }

    #[test]
    fn package_externalization_hints_command_persists_public_export_proofs() {
        let mut connection = Connection::open_in_memory().expect("open sqlite");
        connection
            .execute_batch(
                r"
                CREATE TABLE package_source_cache (
                    package_name TEXT NOT NULL,
                    package_version TEXT NOT NULL,
                    entry_path TEXT NOT NULL,
                    source_content TEXT NOT NULL,
                    content_hash TEXT NOT NULL,
                    external_importable INTEGER NOT NULL DEFAULT 1,
                    external_import_policy_version INTEGER NOT NULL DEFAULT 0,
                    export_specifier TEXT NOT NULL DEFAULT '',
                    fetched_at TEXT NOT NULL,
                    expires_at TEXT NOT NULL,
                    PRIMARY KEY (package_name, package_version, entry_path)
                );
                ",
            )
            .expect("create cache table");
        let public_source = "export { Widget } from './internal/widget.js';";
        let private_source = "export class Widget { method(){ return 1; } }";
        for (entry_path, source, external_importable, export_specifier) in [
            ("dist/index.js", public_source, 1_i64, "pkg"),
            (
                "dist/internal/widget.js",
                private_source,
                0_i64,
                "pkg/dist/internal/widget.js",
            ),
        ] {
            connection
                .execute(
                    r"
                    INSERT INTO package_source_cache
                        (package_name, package_version, entry_path, source_content,
                         content_hash, external_importable, external_import_policy_version,
                         export_specifier, fetched_at, expires_at)
                    VALUES ('pkg', '1.2.3', ?1, ?2, ?3, ?4, ?5, ?6, 'now', 'later')
                    ",
                    params![
                        entry_path,
                        source,
                        stable_hash(source.as_bytes()),
                        external_importable,
                        PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION,
                        export_specifier,
                    ],
                )
                .expect("insert cache row");
        }

        let outcome = package_externalization_hints_from_connection(
            &mut connection,
            &PackageExternalizationHintsArgs {
                input: PathBuf::from("input.db"),
                package_names: vec!["pkg".to_string()],
                limit: None,
                apply: true,
            },
        )
        .expect("write hints");

        assert_eq!(outcome.scanned_rows, 2);
        assert_eq!(outcome.verified_rows, 2);
        let (export_specifier, members): (String, String) = connection
            .query_row(
                r"
                SELECT export_specifier, public_members_json
                  FROM package_externalization_hints
                 WHERE package_name = 'pkg'
                   AND package_version = '1.2.3'
                   AND entry_path = 'dist/internal/widget.js'
                ",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("public proof hint row");
        assert_eq!(export_specifier, "pkg");
        assert!(members.contains("Widget"));

        let mut sources = vec![PackageSource::source_only(
            "pkg",
            "1.2.3",
            "pkg/dist/internal/widget.js",
            "pkg@1.2.3/dist/internal/widget.js",
            private_source,
        )];
        let promoted = promote_package_sources_with_externalization_hints(
            &connection,
            &BTreeSet::from(["pkg".to_string()]),
            &mut sources,
        )
        .expect("promote public proof hint");
        assert_eq!(promoted, 1);
        assert!(
            sources
                .iter()
                .any(|source| source.external_importable && source.export_specifier == "pkg")
        );
    }

    #[test]
    fn source_eliminated_metric_counts_externalized_private_closure() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "root",
            "pkg/root.ts",
            "pkg",
            Some("1.0.0".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(11),
            "private",
            "pkg/private.ts",
            "pkg",
            Some("1.0.0".to_string()),
        ));
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(10),
            target: ModuleDependencyTarget::Module(ModuleId(11)),
        });
        let mut private_ownership = PackageAttributionInput::rejected_source(
            ModuleId(11),
            "pkg",
            "matched package ownership, but the evidence does not prove a safe single external import",
        );
        private_ownership.package_version = Some("1.0.0".to_string());
        rows.package_attributions.push(private_ownership);
        let report = VersionedPackageMatchReport {
            attributions: vec![PackageAttributionInput::accepted_external(
                ModuleId(10),
                "pkg",
                "1.0.0",
                "pkg",
            )],
            surfaces: Vec::new(),
            matches: Vec::new(),
            version_matches: Vec::new(),
            audit: AuditReport::default(),
        };

        assert_eq!(
            source_eliminated_package_modules_for_report(&rows, &report),
            2
        );
        let stats = package_source_elimination_stats_for_report(&rows, &report, 2);
        assert_eq!(stats.direct_external_import_modules, 1);
        assert_eq!(stats.private_source_suppressed_package_modules, 1);
        assert_eq!(stats.source_eliminated_package_modules, 2);
        assert_eq!(stats.remaining_package_source_modules, 0);
    }

    #[test]
    fn materialize_mode_uses_only_current_policy_cache_rows_as_match_sources() {
        let mut connection = Connection::open_in_memory().expect("open sqlite");
        connection
            .execute_batch(
                r"
                CREATE TABLE package_source_cache (
                    package_name TEXT NOT NULL,
                    package_version TEXT NOT NULL,
                    entry_path TEXT NOT NULL,
                    source_content TEXT NOT NULL,
                    content_hash TEXT NOT NULL,
                    external_importable INTEGER NOT NULL DEFAULT 1,
                    external_import_policy_version INTEGER NOT NULL DEFAULT 0,
                    export_specifier TEXT NOT NULL DEFAULT '',
                    fetched_at TEXT NOT NULL,
                    expires_at TEXT NOT NULL,
                    PRIMARY KEY (package_name, package_version, entry_path)
                );
                INSERT INTO package_source_cache
                    (package_name, package_version, entry_path, source_content,
                     content_hash, external_importable, external_import_policy_version,
                     export_specifier, fetched_at, expires_at)
                VALUES
                    ('pkg', '1.x', 'internal.js', 'export const stale = 1;',
                     'stale', 1, 0, '', 'now', 'later'),
                    ('pkg', '1.2.3', 'public.js', 'export const current = 1;',
                     'current', 1, 4, 'pkg/public', 'now', 'later');
                ",
            )
            .expect("create package source cache");
        let rows = InputRows::new(ProjectInput::new(1, "fixture"));

        let loaded = load_package_sources(
            &mut connection,
            &rows,
            &BTreeSet::from(["pkg".to_string()]),
            &[],
            true,
            false,
        )
        .expect("load cache");

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].package_version, "1.2.3");
        assert_eq!(loaded[0].export_specifier, "pkg/public");
        assert!(loaded[0].external_importable);
    }

    #[test]
    fn package_source_cache_stale_policy_versions_are_materialization_hints() {
        let connection = Connection::open_in_memory().expect("open sqlite");
        connection
            .execute_batch(
                r"
                CREATE TABLE package_source_cache (
                    package_name TEXT NOT NULL,
                    package_version TEXT NOT NULL,
                    entry_path TEXT NOT NULL,
                    source_content TEXT NOT NULL,
                    content_hash TEXT NOT NULL,
                    external_importable INTEGER NOT NULL DEFAULT 1,
                    external_import_policy_version INTEGER NOT NULL DEFAULT 0,
                    export_specifier TEXT NOT NULL DEFAULT '',
                    fetched_at TEXT NOT NULL,
                    expires_at TEXT NOT NULL,
                    PRIMARY KEY (package_name, package_version, entry_path)
                );
                INSERT INTO package_source_cache
                    (package_name, package_version, entry_path, source_content,
                     content_hash, external_importable, external_import_policy_version, export_specifier,
                     fetched_at, expires_at)
                VALUES
                    ('pkg', '1.2.3', 'index.js', 'export const oldValue = 1;',
                     'hash-a', 1, 0, '', 'now', 'later'),
                    ('pkg', '1.2.4', 'index.js', 'export const newValue = 1;',
                     'hash-b', 1, 4, 'pkg', 'now', 'later'),
                    ('other', '9.9.9', 'index.js', 'export const other = 1;',
                     'hash-c', 1, 0, '', 'now', 'later');
                ",
            )
            .expect("create mixed policy package source cache");

        let stale =
            stale_package_source_cache_versions(&connection, &BTreeSet::from(["pkg".to_string()]))
                .expect("query stale cache versions");

        assert_eq!(
            stale,
            BTreeSet::from([("pkg".to_string(), "1.2.3".to_string())])
        );
    }

    #[test]
    fn stale_cache_materialization_hints_resolve_ranges_to_project_versions() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(1),
            "m1",
            "lodash/add.js",
            "lodash",
            Some("4.2.0".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(2),
            "m2",
            "lodash/map.js",
            "lodash",
            Some("4.x".to_string()),
        ));
        let existing_sources = [
            PackageSource::source_only(
                "lodash",
                "4.2.0",
                "lodash/add",
                "lodash@4.2.0/add.js",
                "export {};",
            ),
            PackageSource::source_only(
                "lodash",
                "4.17.21",
                "lodash/add",
                "lodash@4.17.21/add.js",
                "export {};",
            ),
        ];
        let stale = BTreeSet::from([
            ("lodash".to_string(), "4.x".to_string()),
            ("lodash".to_string(), "4.17.21".to_string()),
        ]);

        let hints = stale_cache_version_hints_for_materialization(
            &rows,
            &BTreeSet::from(["lodash".to_string()]),
            &existing_sources,
            &stale,
        );

        assert_eq!(
            hints,
            BTreeSet::from([("lodash".to_string(), "4.2.0".to_string())]),
            "stale range cache rows must materialize the resolved project version, not raw 4.x or unrelated cached versions"
        );
    }

    #[test]
    fn local_package_source_collection_prefers_compiled_runtime_family_over_src_ts() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let package_dir = tempdir.path().join("node_modules/pkg");
        fs::create_dir_all(package_dir.join("src")).expect("create src dir");
        fs::create_dir_all(package_dir.join("dist")).expect("create dist dir");
        fs::write(
            package_dir.join("package.json"),
            r#"{"name":"pkg","version":"1.2.3","main":"dist/index.js"}"#,
        )
        .expect("write package json");
        fs::write(
            package_dir.join("src/index.ts"),
            "export const tsSource: number = 1;",
        )
        .expect("write src ts");
        fs::write(
            package_dir.join("dist/index.js"),
            "export const jsSource = 1;",
        )
        .expect("write dist js");
        let metadata = local_package_metadata(package_dir.as_path())
            .expect("read metadata")
            .expect("metadata");
        let mut sources = Vec::new();

        collect_local_package_sources(package_dir.as_path(), &metadata, &mut sources)
            .expect("collect sources");

        assert_eq!(sources.len(), 1);
        assert!(sources[0].source_path.ends_with("dist/index.js"));
        assert!(sources[0].external_importable);
    }

    #[test]
    fn local_package_metadata_rejects_unparseable_package_json() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let package_dir = tempdir.path().join("node_modules/pkg");
        fs::create_dir_all(package_dir.as_path()).expect("create package dir");
        fs::write(package_dir.join("package.json"), r#"{"name":"pkg","#)
            .expect("write invalid package json");

        let error = local_package_metadata(package_dir.as_path())
            .expect_err("invalid package metadata should fail");

        assert!(matches!(
            error,
            MatchPackagesError::InvalidPackageMetadata { .. }
        ));
    }

    #[test]
    fn local_package_source_collection_wraps_importable_json_data() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let package_dir = tempdir.path().join("node_modules/css-color-names");
        fs::create_dir_all(package_dir.as_path()).expect("create package dir");
        fs::write(
            package_dir.join("package.json"),
            r#"{"name":"css-color-names","version":"1.0.1","main":"./css-color-names.json"}"#,
        )
        .expect("write package json");
        fs::write(
            package_dir.join("css-color-names.json"),
            r##"{"aliceblue":"#f0f8ff","rebeccapurple":"#663399"}"##,
        )
        .expect("write json data");
        fs::write(package_dir.join("ignored.json"), r#"{"private":true}"#)
            .expect("write private json");
        let metadata = local_package_metadata(package_dir.as_path())
            .expect("read metadata")
            .expect("metadata");
        let mut sources = Vec::new();

        collect_local_package_sources(package_dir.as_path(), &metadata, &mut sources)
            .expect("collect sources");

        assert_eq!(sources.len(), 1);
        assert!(sources[0].source_path.ends_with("css-color-names.json"));
        assert_eq!(sources[0].package_name, "css-color-names");
        assert_eq!(sources[0].package_version, "1.0.1");
        assert!(sources[0].external_importable);
        assert!(sources[0].source.starts_with("export default "));
        assert!(sources[0].source.contains("aliceblue"));
    }

    #[test]
    fn local_package_source_collection_keeps_exported_package_json() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let package_dir = tempdir.path().join("node_modules/pkg");
        fs::create_dir_all(package_dir.as_path()).expect("create package dir");
        fs::write(
            package_dir.join("package.json"),
            r#"{"name":"pkg","version":"1.2.3","exports":{"./package.json":"./package.json"}}"#,
        )
        .expect("write package json");
        let metadata = local_package_metadata(package_dir.as_path())
            .expect("read metadata")
            .expect("metadata");
        let mut sources = Vec::new();

        collect_local_package_sources(package_dir.as_path(), &metadata, &mut sources)
            .expect("collect sources");

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].export_specifier, "pkg/package.json");
        assert!(sources[0].source_path.ends_with("package.json"));
        assert!(sources[0].external_importable);
        assert!(sources[0].source.starts_with("export default "));
    }

    #[test]
    fn package_source_build_variant_selection_uses_semantic_path_hints() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "m10",
            "modules/10-rxjs/operators/sample.ts",
            "rxjs",
            Some("7.8.2".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(11),
            "m11",
            "modules/11-rxjs/_internal/is-array-like.ts",
            "rxjs",
            Some("7.8.2".to_string()),
        ));
        let mut sources = vec![
            PackageSource::source_only(
                "rxjs",
                "7.8.2",
                "rxjs/operators/sample",
                "rxjs@7.8.2/dist/cjs/operators/sample.js",
                "exports.sample = sample;",
            ),
            PackageSource::source_only(
                "rxjs",
                "7.8.2",
                "rxjs/internal/isArrayLike",
                "rxjs@7.8.2/dist/cjs/internal/util/isArrayLike.js",
                "exports.isArrayLike = isArrayLike;",
            ),
            PackageSource::source_only(
                "rxjs",
                "7.8.2",
                "rxjs/operators/sample",
                "rxjs@7.8.2/dist/esm/operators/sample.js",
                "export function sample() {}",
            ),
            PackageSource::source_only(
                "rxjs",
                "7.8.2",
                "rxjs/internal/isArrayLike",
                "rxjs@7.8.2/dist/esm/internal/util/isArrayLike.js",
                "export function isArrayLike() {}",
            ),
            PackageSource::source_only(
                "rxjs",
                "7.8.2",
                "rxjs/internal/unrelated",
                "rxjs@7.8.2/src/internal/unrelated.ts",
                "export const unrelated = 1;",
            ),
        ];

        super::filter_package_sources_to_best_build_variants(&rows, &mut sources);

        assert_eq!(sources.len(), 2);
        assert!(
            sources
                .iter()
                .all(|source| source.source_path.contains("/dist/esm/")),
            "{sources:?}"
        );
    }

    #[test]
    fn package_source_build_variant_selection_prefers_full_source_unit_variant_hint() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "m10",
            "rxjs/dist/esm/operators/sample",
            "rxjs",
            Some("7.8.2".to_string()),
        ));
        let mut sources = vec![
            PackageSource::source_only(
                "rxjs",
                "7.8.2",
                "rxjs/operators/sample",
                "rxjs@7.8.2/dist/cjs/operators/sample.js",
                "exports.sample = sample;",
            ),
            PackageSource::source_only(
                "rxjs",
                "7.8.2",
                "rxjs/operators/sample",
                "rxjs@7.8.2/dist/esm/operators/sample.js",
                "export function sample() {}",
            ),
        ];

        super::filter_package_sources_to_best_build_variants(&rows, &mut sources);

        assert_eq!(sources.len(), 1);
        assert!(sources[0].source_path.contains("/dist/esm/"));
    }

    #[test]
    fn package_source_build_variant_selection_keeps_equal_score_importable_family() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "m10",
            "modules/10-rxjs/operators/sample.ts",
            "rxjs",
            Some("7.8.2".to_string()),
        ));
        let mut sources = vec![
            PackageSource::external(
                "rxjs",
                "7.8.2",
                "rxjs/internal/operators/sample",
                "rxjs@7.8.2/dist/cjs/internal/operators/sample.js",
                "exports.sample = sample;",
            ),
            PackageSource::source_only(
                "rxjs",
                "7.8.2",
                "rxjs/dist/esm/internal/operators/sample.js",
                "rxjs@7.8.2/dist/esm/internal/operators/sample.js",
                "export function sample() {}",
            ),
        ];

        super::filter_package_sources_to_best_build_variants(&rows, &mut sources);

        assert_eq!(sources.len(), 2);
        assert!(
            sources
                .iter()
                .any(|source| source.source_path.contains("/dist/esm/")
                    && !source.external_importable),
            "{sources:?}"
        );
        assert!(
            sources
                .iter()
                .any(|source| source.source_path.contains("/dist/cjs/")
                    && source.external_importable),
            "{sources:?}"
        );
    }

    #[test]
    fn package_source_build_variant_selection_scores_export_surface_hints() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "m10",
            "pkg/public/client.js",
            "pkg",
            Some("1.0.0".to_string()),
        ));
        let mut sources = vec![
            PackageSource::external(
                "pkg",
                "1.0.0",
                "pkg/public/client",
                "pkg@1.0.0/dist/index.js",
                "export const client = 1;",
            ),
            PackageSource::source_only(
                "pkg",
                "1.0.0",
                "pkg/public/client",
                "pkg@1.0.0/src/public/client.ts",
                "export const client = 1;",
            ),
        ];

        filter_package_sources_to_best_build_variants(&rows, &mut sources);

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].export_specifier, "pkg/public/client");
        assert!(sources[0].source_path.contains("/dist/index.js"));
        assert!(sources[0].external_importable);
    }

    #[test]
    fn package_source_build_variant_selection_keeps_root_export_surface() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "m10",
            "pkg/public/client.js",
            "pkg",
            Some("1.0.0".to_string()),
        ));
        let mut sources = vec![
            PackageSource::external(
                "pkg",
                "1.0.0",
                "pkg",
                "pkg@1.0.0/index.js",
                "export const root = 1;",
            ),
            PackageSource::external(
                "pkg",
                "1.0.0",
                "pkg/public/client",
                "pkg@1.0.0/dist/index.js",
                "export const client = 1;",
            ),
            PackageSource::source_only(
                "pkg",
                "1.0.0",
                "pkg/public/client",
                "pkg@1.0.0/src/public/client.ts",
                "export const client = 1;",
            ),
        ];

        filter_package_sources_to_best_build_variants(&rows, &mut sources);

        assert!(
            sources.iter().any(|source| source.export_specifier == "pkg"
                && source.source_path.ends_with("index.js")
                && source.external_importable),
            "{sources:?}"
        );
        assert!(
            sources
                .iter()
                .any(|source| source.export_specifier == "pkg/public/client"
                    && source.source_path.contains("/dist/index.js")),
            "{sources:?}"
        );
    }

    #[test]
    fn package_source_path_hint_filter_keeps_export_surface_match() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "m10",
            "pkg/public/client.js",
            "pkg",
            Some("1.0.0".to_string()),
        ));
        let mut sources = (0..300)
            .map(|index| {
                PackageSource::source_only(
                    "pkg",
                    "1.0.0",
                    format!("pkg/private-{index}"),
                    format!("pkg@1.0.0/dist/chunk-{index}.js"),
                    "export const privateValue = 1;",
                )
            })
            .collect::<Vec<_>>();
        sources.push(PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/public/client",
            "pkg@1.0.0/dist/index.js",
            "export const client = 1;",
        ));

        filter_package_sources_to_relevant_path_hints(&rows, &mut sources);

        assert!(
            sources
                .iter()
                .any(|source| source.export_specifier == "pkg/public/client"
                    && source.source_path.ends_with("dist/index.js")
                    && source.external_importable),
            "{sources:?}"
        );
    }

    #[test]
    fn package_source_path_hint_filter_keeps_body_semantic_member_match() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "m10",
            "modules/10-opentelemetry/api/diag-log-level.ts",
            "@opentelemetry/api",
            Some("1.9.1".to_string()),
        ));
        let mut sources = (0..300)
            .map(|index| {
                PackageSource::source_only(
                    "@opentelemetry/api",
                    "1.9.1",
                    format!("@opentelemetry/api/private-{index}"),
                    format!("@opentelemetry/api@1.9.1/build/src/private-{index}.js"),
                    "exports.privateValue = 1;",
                )
            })
            .collect::<Vec<_>>();
        sources.push(PackageSource::source_only(
            "@opentelemetry/api",
            "1.9.1",
            "@opentelemetry/api/build/src/diag/types",
            "@opentelemetry/api@1.9.1/build/src/diag/types.js",
            "exports.DiagLogLevel = void 0;",
        ));

        filter_package_sources_to_relevant_path_hints(&rows, &mut sources);

        assert!(
            sources
                .iter()
                .any(|source| source.source_path.ends_with("build/src/diag/types.js")),
            "{sources:?}"
        );
    }

    #[test]
    fn source_unit_path_hints_enrich_package_module_semantic_path() {
        let connection = Connection::open_in_memory().expect("open in-memory database");
        connection
            .execute_batch(
                r"
                CREATE TABLE source_units (
                    project_id INTEGER NOT NULL,
                    file_id INTEGER,
                    logical_path TEXT NOT NULL,
                    package_name TEXT,
                    package_version TEXT
                );
                INSERT INTO source_units
                    (project_id, file_id, logical_path, package_name, package_version)
                VALUES
                    (1, 7, 'webpack://app/./node_modules/rxjs/dist/esm/operators/sample.js',
                     'rxjs', '7.8.2');
                ",
            )
            .expect("seed source_units");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(
            ModuleInput::package(ModuleId(10), "m10", "modules/10.ts", "rxjs", None)
                .with_source_file(7),
        );

        super::enrich_package_modules_from_source_units(&connection, &mut rows, 1)
            .expect("enrich from source_units");

        assert_eq!(
            rows.modules[0].semantic_path,
            "rxjs/dist/esm/operators/sample"
        );
        assert_eq!(rows.modules[0].package_version.as_deref(), Some("7.8.2"));
    }

    #[test]
    fn match_package_audit_findings_are_deduplicated() {
        let finding = AuditFinding::error(FindingCode::UnparseablePackageSource, "parse failed")
            .with_module("pkg@1.0.0/src/index.ts")
            .with_binding("pkg@1.0.0");
        let mut audit = AuditReport::default();
        audit.push(finding.clone());
        audit.push(finding);

        let deduped = dedup_audit_report(audit);

        assert_eq!(deduped.findings().len(), 1);
    }

    #[test]
    fn output_paths_cannot_escape_output_directory() {
        let error = checked_output_path(PathBuf::from("out").as_path(), "../escape.ts");

        assert!(error.is_err());
    }

    #[test]
    fn project_writer_emits_typescript_scaffold() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let files = vec![EmittedFile {
            path: "modules/1-entry.ts".to_string(),
            source: "// @ts-nocheck\nconsole.log('ok');".to_string(),
        }];

        let written = write_emitted_project(
            &files,
            &[],
            tempdir.path(),
            &[RuntimeDependency {
                package_name: "undici".to_string(),
                package_version: "2.2.1".to_string(),
            }],
        )
        .expect("project should be written");

        assert_eq!(written, 1);
        assert!(tempdir.path().join("modules/1-entry.ts").exists());
        assert!(
            fs::read_to_string(tempdir.path().join("package.json"))
                .expect("package json")
                .contains("\"check\": \"tsc --noEmit -p tsconfig.json\"")
        );
        assert!(
            fs::read_to_string(tempdir.path().join("package.json"))
                .expect("package json")
                .contains("\"undici\": \"2.2.1\"")
        );
        assert!(
            fs::read_to_string(tempdir.path().join("package.json"))
                .expect("package json")
                .contains("\"@types/node\": \"*\"")
        );
        assert!(
            !tempdir.path().join(".npmrc").exists(),
            "npmrc should only be written for known peer conflicts"
        );
        assert!(
            fs::read_to_string(tempdir.path().join("tsconfig.json"))
                .expect("tsconfig")
                .contains("\"modules/**/*.ts\"")
        );
        assert!(tempdir.path().join("tsconfig.runtime.json").exists());
    }

    #[test]
    fn project_writer_emits_npmrc_for_source_preserved_peer_conflict() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let files = vec![EmittedFile {
            path: "modules/1-entry.ts".to_string(),
            source: "// @ts-nocheck\nconsole.log('ok');".to_string(),
        }];

        write_emitted_project(
            &files,
            &[],
            tempdir.path(),
            &[
                RuntimeDependency {
                    package_name: "ink".to_string(),
                    package_version: "7.0.3".to_string(),
                },
                RuntimeDependency {
                    package_name: "react".to_string(),
                    package_version: "19.1.5".to_string(),
                },
                RuntimeDependency {
                    package_name: "react-devtools-core".to_string(),
                    package_version: "4.28.5".to_string(),
                },
            ],
        )
        .expect("project should be written");

        assert_eq!(
            fs::read_to_string(tempdir.path().join(".npmrc")).expect("npmrc"),
            "legacy-peer-deps=true\n"
        );
    }

    #[test]
    fn project_writer_emits_npmrc_for_externalized_zod_anthropic_peer_conflict() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let files = vec![EmittedFile {
            path: "modules/1-entry.ts".to_string(),
            source: "// @ts-nocheck\nconsole.log('ok');".to_string(),
        }];

        write_emitted_project(
            &files,
            &[],
            tempdir.path(),
            &[
                RuntimeDependency {
                    package_name: "@anthropic-ai/sdk".to_string(),
                    package_version: "0.91.1".to_string(),
                },
                RuntimeDependency {
                    package_name: "zod".to_string(),
                    package_version: "3.22.5".to_string(),
                },
            ],
        )
        .expect("project should be written");

        assert_eq!(
            fs::read_to_string(tempdir.path().join(".npmrc")).expect("npmrc"),
            "legacy-peer-deps=true\n"
        );
    }

    #[test]
    fn project_writer_materializes_react_esm_compat_shims() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let files = vec![EmittedFile {
            path: "modules/1-entry.ts".to_string(),
            source: "// @ts-nocheck\nconsole.log('ok');".to_string(),
        }];

        write_emitted_project(
            &files,
            &[],
            tempdir.path(),
            &[
                RuntimeDependency {
                    package_name: "react".to_string(),
                    package_version: "19.1.5".to_string(),
                },
                RuntimeDependency {
                    package_name: "react-dom".to_string(),
                    package_version: "18.3.1".to_string(),
                },
            ],
        )
        .expect("project should be written");

        let package_json = fs::read_to_string(tempdir.path().join("package.json"))
            .expect("package json should be written");
        let react_shim = fs::read_to_string(tempdir.path().join("vendor-shims/react/index.js"))
            .expect("react shim should be written");
        let react_dom_shim =
            fs::read_to_string(tempdir.path().join("vendor-shims/react-dom/index.js"))
                .expect("react-dom shim should be written");

        assert!(package_json.contains("\"react\": \"file:./vendor-shims/react\""));
        assert!(package_json.contains("\"react-cjs\": \"npm:react@19.1.5\""));
        assert!(package_json.contains("\"react-dom\": \"file:./vendor-shims/react-dom\""));
        assert!(package_json.contains("\"react-dom-cjs\": \"npm:react-dom@18.3.1\""));
        assert!(react_shim.contains("export const useEffectEvent"));
        assert!(react_dom_shim.contains("const load = () =>"));
        assert!(
            fs::read_to_string(tempdir.path().join("vendor-shims/react/package.json"))
                .expect("react package")
                .contains("\"version\": \"19.2.0\"")
        );
    }

    #[test]
    fn project_writer_adds_sentry_opentelemetry_peer_dependencies() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let files = vec![EmittedFile {
            path: "modules/1-entry.ts".to_string(),
            source: "// @ts-nocheck\nconsole.log('ok');".to_string(),
        }];

        write_emitted_project(
            &files,
            &[],
            tempdir.path(),
            &[RuntimeDependency {
                package_name: "@sentry/node".to_string(),
                package_version: "8.55.0".to_string(),
            }],
        )
        .expect("project should be written");

        let package_json = fs::read_to_string(tempdir.path().join("package.json"))
            .expect("package json should be written");
        assert!(package_json.contains("\"@opentelemetry/context-async-hooks\": \"^1.30.1\""));
        assert!(package_json.contains("\"@opentelemetry/instrumentation\": \"^0.57.1\""));
    }

    #[test]
    fn project_writer_exposes_cli_entrypoint_when_planned() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let files = vec![EmittedFile {
            path: "cli.ts".to_string(),
            source: "#!/usr/bin/env node\n// @ts-nocheck\nconsole.log('ok');".to_string(),
        }];

        let written = write_emitted_project(&files, &[], tempdir.path(), &[])
            .expect("project should be written");
        let package_json = fs::read_to_string(tempdir.path().join("package.json"))
            .expect("package json should be written");
        let tsconfig = fs::read_to_string(tempdir.path().join("tsconfig.json")).expect("tsconfig");

        assert_eq!(written, 1);
        assert!(tempdir.path().join("cli.ts").exists());
        assert!(package_json.contains("\"start\": \"node ./dist/cli.js\""));
        assert!(package_json.contains("\"reverts-output\": \"./dist/cli.js\""));
        assert!(tsconfig.contains("\"cli.ts\""));
    }

    #[test]
    fn project_writer_materializes_assets_and_build_copy_script() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let files = vec![EmittedFile {
            path: "modules/1-entry.ts".to_string(),
            source: "// @ts-nocheck\nexport const ok = true;".to_string(),
        }];
        let assets = vec![EmittedAsset {
            path: "modules/1-entry/vendor/rg".to_string(),
            bytes: b"rg-binary".to_vec(),
            executable: true,
        }];

        let written = write_emitted_project(&files, &assets, tempdir.path(), &[])
            .expect("project should be written");
        let asset_path = tempdir.path().join("modules/1-entry/vendor/rg");
        let package_json = fs::read_to_string(tempdir.path().join("package.json"))
            .expect("package json should be written");
        let copy_assets = fs::read_to_string(tempdir.path().join("scripts/copy-assets.mjs"))
            .expect("copy-assets script should be written");

        assert_eq!(written, 2);
        assert_eq!(
            fs::read(asset_path.as_path()).expect("asset bytes should be written"),
            b"rg-binary"
        );
        assert!(package_json.contains("node ./scripts/copy-assets.mjs"));
        assert!(copy_assets.contains("modules/1-entry/vendor/rg"));
        assert!(copy_assets.contains("dist/modules/1-entry/vendor/rg"));
        assert!(copy_assets.contains("\"executable\": true"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = fs::metadata(asset_path.as_path())
                .expect("asset metadata")
                .permissions()
                .mode();
            assert_ne!(mode & 0o111, 0);
        }
    }

    #[test]
    fn match_packages_runs_bundle_extraction_before_matcher() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let bundle_path = tempdir.path().join("bundle.js");
        let bundle_src = r#"
            var __commonJS=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports);
            var lib = __commonJS({
                "node_modules/example/index.js": (exports, module) => {
                    function add(a, b) { return a + b; }
                    module.exports = { add };
                }
            });
        "#;
        let mut connection = package_match_connection(bundle_path.clone(), bundle_src, &[]);
        // Replace the default seeded module (id=10, package kind) with an
        // application-kind module that carries no package_name. Bundle
        // extraction will discover the `node_modules/example/index.js` inner
        // module and inject a new package-kind row, bumping
        // loaded_package_modules to >= 1.
        connection
            .execute_batch(
                "DELETE FROM modules WHERE id = 10;
                 INSERT INTO modules (id, file_id, original_name, semantic_name, module_category,
                                      package_name, package_version, byte_start, byte_end)
                 VALUES (10, 1, 'lib', 'bundle/lib', 'application', NULL, NULL, 0, 0);",
            )
            .expect("seed module");

        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: false,
            package_names: Vec::new(),
            package_source_roots: Vec::new(),
            materialize_package_sources: false,
        };
        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");
        assert!(
            outcome.loaded_package_modules >= 1,
            "extraction should have produced at least one package module: {outcome:?}"
        );
    }

    #[test]
    fn match_packages_skips_cache_and_pipeline_when_no_package_scope_exists() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut connection = package_match_connection(
            tempdir.path().join("app.js"),
            "function local(){return 1;}",
            &[(
                "unused",
                "1.0.0",
                "index.js",
                "export function unused(){return 1;}",
            )],
        );
        connection
            .execute_batch(
                "DELETE FROM modules WHERE id = 10;
                 INSERT INTO modules (id, file_id, original_name, semantic_name, module_category,
                                      package_name, package_version, byte_start, byte_end)
                 VALUES (10, 1, 'app', 'src/app', 'application', NULL, NULL, 0, 0);",
            )
            .expect("seed application module");
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: false,
            package_names: Vec::new(),
            package_source_roots: Vec::new(),
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
        assert_eq!(outcome.loaded_package_modules, 0);
        assert_eq!(outcome.loaded_package_sources, 0);
        assert_eq!(outcome.matched_modules, 0);
        assert_eq!(outcome.function_attributions, 0);
    }

    #[test]
    fn match_packages_dry_run_does_not_write_attribution() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            "export function add(a,b){return a+b}",
            &[(
                "pkg",
                "1.2.3",
                "add.js",
                "export function add(a, b) {\n  return a + b;\n}",
            )],
        );
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: false,
            package_names: Vec::new(),
            package_source_roots: Vec::new(),
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean());
        assert_eq!(outcome.loaded_package_modules, 1);
        assert_eq!(outcome.package_source_quality_trusted, 1);
        assert_eq!(outcome.package_source_quality_invalid, 0);
        assert_eq!(outcome.loaded_package_sources, 1);
        assert_eq!(outcome.matched_modules, 1);
        assert_eq!(outcome.matched_package_surfaces, 0);
        assert_eq!(outcome.written_attributions, 0);
        assert_eq!(outcome.written_surfaces, 0);
        assert_eq!(package_attribution_count(&connection), 0);
    }

    #[test]
    fn match_packages_revalidates_requested_existing_accepted_attribution() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            "function localOnly(){return 1;}",
            &[(
                "pkg",
                "1.2.3",
                "add.js",
                "export function add(a, b) {\n  return a + b;\n}",
            )],
        );
        connection
            .execute(
                r"
                INSERT INTO package_attributions
                    (module_id, module_original_name, package_name, package_version,
                     package_subpath, resolved_file, export_specifier, emission_mode,
                     status, evidence_json, rejection_reason, created_at, updated_at)
                VALUES (10, 'm10', 'pkg', '1.2.3',
                        'add.js', 'pkg@1.2.3/add.js', 'pkg/add.js',
                        'external_import', 'accepted', '{}', NULL, 'old', 'old')
                ",
                [],
            )
            .expect("seed stale accepted attribution");
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: true,
            package_names: vec!["pkg".to_string()],
            package_source_roots: Vec::new(),
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
        assert_eq!(
            package_attribution_count(&connection),
            1,
            "revalidation should overwrite the stale row instead of adding a duplicate"
        );
        let (
            status,
            emission_mode,
            export_specifier,
            external_import_policy_version,
            rejection_reason,
        ): (String, String, Option<String>, i64, Option<String>) = connection
            .query_row(
                r"
                SELECT status, emission_mode, export_specifier,
                       external_import_policy_version, rejection_reason
                  FROM package_attributions
                 WHERE module_id = 10
                ",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("stale accepted attribution should be rewritten");
        assert_eq!(status, "accepted");
        assert_eq!(emission_mode, "external_import");
        assert_eq!(export_specifier.as_deref(), Some("pkg/add.js"));
        assert_eq!(
            external_import_policy_version,
            PACKAGE_ATTRIBUTION_EXTERNAL_IMPORT_POLICY_VERSION
        );
        assert_eq!(rejection_reason, None);
    }

    #[test]
    fn match_packages_without_filter_revalidates_existing_accepted_attribution() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            "function localOnly(){return 1;}",
            &[(
                "pkg",
                "1.2.3",
                "add.js",
                "export function add(a, b) {\n  return a + b;\n}",
            )],
        );
        connection
            .execute(
                r"
                INSERT INTO package_attributions
                    (module_id, module_original_name, package_name, package_version,
                     package_subpath, resolved_file, export_specifier, emission_mode,
                     status, evidence_json, rejection_reason, created_at, updated_at)
                VALUES (10, 'm10', 'pkg', '1.2.3',
                        'add.js', 'pkg@1.2.3/add.js', 'pkg/add.js',
                        'external_import', 'accepted', '{}', NULL, 'old', 'old')
                ",
                [],
            )
            .expect("seed accepted attribution");
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: true,
            package_names: Vec::new(),
            package_source_roots: Vec::new(),
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
        assert_eq!(
            package_attribution_count(&connection),
            1,
            "full revalidation should overwrite the stale row instead of adding a duplicate"
        );
        let (status, emission_mode, export_specifier, external_import_policy_version): (
            String,
            String,
            Option<String>,
            i64,
        ) = connection
            .query_row(
                r"
                SELECT status, emission_mode, export_specifier,
                       external_import_policy_version
                  FROM package_attributions
                 WHERE module_id = 10
                ",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("accepted attribution should be rewritten");
        assert_eq!(status, "accepted");
        assert_eq!(emission_mode, "external_import");
        assert_eq!(export_specifier.as_deref(), Some("pkg/add.js"));
        assert_eq!(
            external_import_policy_version,
            PACKAGE_ATTRIBUTION_EXTERNAL_IMPORT_POLICY_VERSION
        );
    }

    #[test]
    fn match_packages_reports_and_skips_invalid_package_module_slice() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            "lts.allowAbsoluteUrls !== void 0) K.allowAbsoluteU",
            &[(
                "pkg",
                "1.2.3",
                "add.js",
                "export function add(a, b) {\n  return a + b;\n}",
            )],
        );
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: false,
            package_names: Vec::new(),
            package_source_roots: Vec::new(),
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(
            outcome.audit.has(FindingCode::AstFactExtractionFailed),
            "{:?}",
            outcome.audit.findings()
        );
        assert_eq!(outcome.package_source_quality_trusted, 0);
        assert_eq!(outcome.package_source_quality_invalid, 1);
        assert_eq!(
            outcome.matched_modules, 0,
            "invalid source must not be forced to an external package import"
        );
        assert_eq!(outcome.function_ownership_matches, 0);
    }

    #[test]
    fn match_packages_rejects_trailing_garbage_package_module_slice() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            "export function add(a,b){return a+b} %%% trailing-runtime-garbage",
            &[(
                "pkg",
                "1.2.3",
                "add.js",
                "export function add(a, b) {\n  return a + b;\n}",
            )],
        );
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: false,
            package_names: Vec::new(),
            package_source_roots: Vec::new(),
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(
            outcome.audit.has(FindingCode::AstFactExtractionFailed),
            "{:?}",
            outcome.audit.findings()
        );
        assert_eq!(
            outcome.package_source_quality_trusted, 0,
            "invalid package slices must not be rewritten before quality counting"
        );
        assert_eq!(outcome.package_source_quality_invalid, 1);
        assert_eq!(outcome.matched_modules, 0);
    }

    #[test]
    fn match_packages_externalizes_unrestricted_subpath_from_package_source_root() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let package_dir = tempdir.path().join("project/node_modules/pkg");
        fs::create_dir_all(package_dir.join("lib").as_path()).expect("create package lib dir");
        fs::create_dir_all(package_dir.join("tests").as_path()).expect("create package test dir");
        fs::write(
            package_dir.join("package.json"),
            r#"{"name":"pkg","version":"1.2.3"}"#,
        )
        .expect("write package json");
        fs::write(
            package_dir.join("lib/add.js"),
            "export function add(a, b) {\n  return a + b;\n}",
        )
        .expect("write package source");
        fs::write(
            package_dir.join("tests/add.test.js"),
            "export function testOnly() { return 'skip'; }",
        )
        .expect("write skipped package test source");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            "export function add(a,b){return a+b}",
            &[],
        );
        connection
            .execute(
                "UPDATE modules SET package_version = '1.2.3' WHERE id = 10",
                [],
            )
            .expect("set exact package version");
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: true,
            package_names: vec!["pkg".to_string()],
            package_source_roots: vec![tempdir.path().join("project")],
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
        assert_eq!(
            outcome.loaded_package_sources, 1,
            "only lib/add.js should be loaded; tests must be skipped"
        );
        assert_eq!(outcome.matched_modules, 1);
        assert!(
            outcome.function_ownership_matches >= 1,
            "unrestricted subpath roots should still produce ownership evidence"
        );
        assert_eq!(
            outcome.written_attributions, 1,
            "unrestricted package subpaths should be persisted as external imports"
        );
        assert!(
            outcome.function_attributions >= 1,
            "importable unrestricted package subpaths should feed external function attribution"
        );
        assert!(outcome.written_function_attributions >= 1);
        let (status, emission_mode, export_specifier, package_version): (
            String,
            String,
            String,
            Option<String>,
        ) = connection
            .query_row(
                r"
                SELECT status, emission_mode, export_specifier, package_version
                  FROM package_attributions
                 WHERE module_id = 10
                ",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("unrestricted subpath match should write an external attribution");
        assert_eq!(status, "accepted");
        assert_eq!(emission_mode, "external_import");
        assert_eq!(export_specifier, "pkg/lib/add.js");
        assert_eq!(package_version.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn match_packages_externalizes_public_export_from_package_source_root() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let package_dir = tempdir.path().join("project/node_modules/pkg");
        fs::create_dir_all(package_dir.join("lib").as_path()).expect("create package lib dir");
        fs::write(
            package_dir.join("package.json"),
            r#"{"name":"pkg","version":"1.2.3","exports":{"./add":"./lib/add.js"}}"#,
        )
        .expect("write package json");
        fs::write(
            package_dir.join("lib/add.js"),
            "export function add(a, b) {\n  return a + b;\n}",
        )
        .expect("write package source");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            "export function add(a,b){return a+b}",
            &[],
        );
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: true,
            package_names: vec!["pkg".to_string()],
            package_source_roots: vec![tempdir.path().join("project")],
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
        assert_eq!(outcome.loaded_package_sources, 1);
        assert_eq!(outcome.matched_modules, 1);
        assert!(outcome.function_ownership_matches >= 1);
        assert_eq!(outcome.written_attributions, 1);
        let (status, emission_mode, package_version, export_specifier): (
            String,
            String,
            String,
            String,
        ) = connection
            .query_row(
                r"
                SELECT status, emission_mode, package_version, export_specifier
                  FROM package_attributions
                 WHERE module_id = 10
                ",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("public export should be externalized");
        assert_eq!(status, "accepted");
        assert_eq!(emission_mode, "external_import");
        assert_eq!(package_version, "1.2.3");
        assert_eq!(export_specifier, "pkg/add");
    }

    #[test]
    fn match_packages_externalizes_exported_package_json_from_package_source_root() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let package_dir = tempdir.path().join("project/node_modules/pkg");
        fs::create_dir_all(package_dir.as_path()).expect("create package dir");
        let package_json =
            r#"{"name":"pkg","version":"1.2.3","exports":{"./package.json":"./package.json"}}"#;
        fs::write(package_dir.join("package.json"), package_json).expect("write package json");
        let bundled_source =
            json_package_source_module(package_json).expect("json package source module");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            bundled_source.as_str(),
            &[],
        );
        connection
            .execute(
                "UPDATE modules SET semantic_name = 'pkg/package-json.ts', package_version = '1.2.3' WHERE id = 10",
                [],
            )
            .expect("set package json semantic path");
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: true,
            package_names: vec!["pkg".to_string()],
            package_source_roots: vec![tempdir.path().join("project")],
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
        assert_eq!(outcome.loaded_package_sources, 1);
        assert_eq!(outcome.matched_modules, 1);
        assert_eq!(outcome.written_attributions, 1);
        let (status, emission_mode, export_specifier, resolved_file): (
            String,
            String,
            String,
            String,
        ) = connection
            .query_row(
                r"
                SELECT status, emission_mode, export_specifier, resolved_file
                  FROM package_attributions
                 WHERE module_id = 10
                ",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("package.json export should be externalized");
        assert_eq!(status, "accepted");
        assert_eq!(emission_mode, "external_import");
        assert_eq!(export_specifier, "pkg/package.json");
        assert!(resolved_file.ends_with("package.json"));
    }

    #[test]
    fn match_packages_externalizes_package_needed_by_different_package_consumer() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let first_module = "export function init(){return 1;}";
        let second_module = "\nexport const consumer = init();";
        let bundled_source = format!("{first_module}{second_module}");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            bundled_source.as_str(),
            &[("pkg", "1.2.3", "index.js", first_module)],
        );
        connection
            .execute(
                r"
                UPDATE modules
                   SET original_name = 'init',
                       semantic_name = 'pkg/index.js',
                       package_version = '1.2.3',
                       byte_start = 0,
                       byte_end = ?1
                 WHERE id = 10
                ",
                [first_module.len() as i64],
            )
            .expect("narrow package module");
        connection
            .execute(
                r"
                INSERT INTO modules
                    (id, file_id, original_name, semantic_name, module_category,
                     package_name, package_version, byte_start, byte_end)
                VALUES (11, 1, 'consumer', 'other/consumer.js', 'package',
                        'other', '1.0.0', ?1, ?2)
                ",
                [first_module.len() as i64, bundled_source.len() as i64],
            )
            .expect("insert package consumer");
        connection
            .execute(
                "INSERT INTO module_dependencies (module_id, dependency_id) VALUES (11, 10)",
                [],
            )
            .expect("insert package dependency");
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: true,
            package_names: vec!["pkg".to_string()],
            package_source_roots: Vec::new(),
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert_eq!(outcome.matched_modules, 1);
        assert_eq!(
            connection
                .query_row(
                    r"
                    SELECT COUNT(*)
                      FROM package_attributions
                     WHERE module_id = 10
                       AND status = 'accepted'
                       AND emission_mode = 'external_import'
                    ",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("count accepted external"),
            1,
            "a different-package source consumer is a package boundary; the consumer remains source while the producer can be imported externally"
        );
    }

    #[test]
    fn external_import_safety_preserves_unproven_same_package_source_boundary() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "root",
            "pkg/root.ts",
            "pkg",
            Some("1.0.0".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(11),
            "unprovenConsumer",
            "pkg/private-consumer.ts",
            "pkg",
            Some("1.0.0".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(12),
            "leaf",
            "pkg/leaf.ts",
            "pkg",
            Some("1.0.0".to_string()),
        ));
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(10),
            target: ModuleDependencyTarget::Module(ModuleId(11)),
        });
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(11),
            target: ModuleDependencyTarget::Module(ModuleId(12)),
        });

        let mut report = VersionedPackageMatchReport {
            attributions: vec![
                PackageAttributionInput::accepted_external(ModuleId(10), "pkg", "1.0.0", "pkg"),
                PackageAttributionInput::accepted_external(
                    ModuleId(12),
                    "pkg",
                    "1.0.0",
                    "pkg/leaf",
                ),
            ],
            surfaces: Vec::new(),
            matches: vec![
                package_match(ModuleId(10), "pkg"),
                package_match(ModuleId(12), "pkg/leaf"),
            ],
            version_matches: Vec::new(),
            audit: AuditReport::default(),
        };

        let safety = filter_unsafe_interpackage_external_attributions(&rows, &mut report);

        assert_eq!(safety.removed_modules, 0);
        assert!(safety.blockers.is_empty());
        assert_eq!(
            report
                .attributions
                .iter()
                .map(|attribution| attribution.module_id)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([ModuleId(10), ModuleId(12)]),
            "unproven same-package consumers are preserved as source boundaries rather than source-suppressed"
        );
        assert_eq!(
            source_eliminated_package_modules_for_report(&rows, &report),
            2,
            "only the two direct external imports are eliminated; the unproven consumer is not source-suppressed"
        );
        let proofs = externalization_chain_proofs(&rows, &report);
        let leaf_proof = proofs.get(&ModuleId(12)).expect("leaf chain proof");
        assert!(
            leaf_proof
                .get("incoming_consumers")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .any(|consumer| {
                    consumer
                        .get("resolution")
                        .and_then(serde_json::Value::as_str)
                        == Some("source_boundary")
                }),
            "chain proof should record preserved same-package consumers as source boundaries"
        );
    }

    #[test]
    fn external_import_safety_preserves_cyclic_same_package_source_boundary() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "public",
            "pkg/public.ts",
            "pkg",
            Some("1.0.0".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(11),
            "runtimeA",
            "pkg/runtime-a.ts",
            "pkg",
            Some("1.0.0".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(12),
            "runtimeB",
            "pkg/runtime-b.ts",
            "pkg",
            Some("1.0.0".to_string()),
        ));
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(11),
            target: ModuleDependencyTarget::Module(ModuleId(10)),
        });
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(11),
            target: ModuleDependencyTarget::Module(ModuleId(12)),
        });
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(12),
            target: ModuleDependencyTarget::Module(ModuleId(11)),
        });

        let mut report = VersionedPackageMatchReport {
            attributions: vec![PackageAttributionInput::accepted_external(
                ModuleId(10),
                "pkg",
                "1.0.0",
                "pkg/public",
            )],
            surfaces: Vec::new(),
            matches: vec![package_match(ModuleId(10), "pkg/public")],
            version_matches: Vec::new(),
            audit: AuditReport::default(),
        };

        let safety = filter_unsafe_interpackage_external_attributions(&rows, &mut report);

        assert_eq!(safety.removed_modules, 0);
        let proofs = externalization_chain_proofs(&rows, &report);
        assert!(
            proofs
                .get(&ModuleId(10))
                .and_then(|proof| proof.get("incoming_consumers"))
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .any(|consumer| {
                    consumer
                        .get("resolution")
                        .and_then(serde_json::Value::as_str)
                        == Some("source_boundary")
                }),
            "closed same-package source cycles are preserved as source boundaries, not blockers"
        );
    }

    #[test]
    fn external_import_safety_allows_source_suppressed_package_closure_consumers() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "root",
            "pkg/root.ts",
            "pkg",
            Some("1.0.0".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(11),
            "privateConsumer",
            "pkg/private-consumer.ts",
            "pkg",
            Some("1.0.0".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(12),
            "leaf",
            "pkg/leaf.ts",
            "pkg",
            Some("1.0.0".to_string()),
        ));
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(10),
            target: ModuleDependencyTarget::Module(ModuleId(11)),
        });
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(11),
            target: ModuleDependencyTarget::Module(ModuleId(12)),
        });
        rows.package_attributions
            .push(rejected_package_ownership(ModuleId(11), "pkg", "1.0.0"));

        let mut report = VersionedPackageMatchReport {
            attributions: vec![
                PackageAttributionInput::accepted_external(ModuleId(10), "pkg", "1.0.0", "pkg"),
                PackageAttributionInput::accepted_external(
                    ModuleId(12),
                    "pkg",
                    "1.0.0",
                    "pkg/leaf",
                ),
            ],
            surfaces: Vec::new(),
            matches: vec![
                package_match(ModuleId(10), "pkg"),
                package_match(ModuleId(12), "pkg/leaf"),
            ],
            version_matches: Vec::new(),
            audit: AuditReport::default(),
        };

        let safety = filter_unsafe_interpackage_external_attributions(&rows, &mut report);

        assert_eq!(safety.removed_modules, 0);
        assert_eq!(
            report
                .attributions
                .iter()
                .map(|attribution| attribution.module_id)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([ModuleId(10), ModuleId(12)]),
            "a private package consumer that is only reachable from an externalized root is suppressed with that closure"
        );
        let proofs = externalization_chain_proofs(&rows, &report);
        let root_proof = proofs.get(&ModuleId(10)).expect("root chain proof");
        assert_eq!(
            root_proof
                .get("source_suppressed_dependency_count")
                .and_then(serde_json::Value::as_u64),
            Some(1)
        );
        let leaf_proof = proofs.get(&ModuleId(12)).expect("leaf chain proof");
        assert!(
            leaf_proof
                .get("incoming_consumers")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .any(|consumer| {
                    consumer
                        .get("resolution")
                        .and_then(serde_json::Value::as_str)
                        == Some("source_suppressed")
                }),
            "leaf proof should record that its private consumer is source-suppressed"
        );
    }

    #[test]
    fn external_import_safety_allows_application_boundary_consumers() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "root",
            "pkg/root.ts",
            "pkg",
            Some("1.0.0".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(11),
            "privateConsumer",
            "pkg/private-consumer.ts",
            "pkg",
            Some("1.0.0".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(12),
            "leaf",
            "pkg/leaf.ts",
            "pkg",
            Some("1.0.0".to_string()),
        ));
        rows.modules
            .push(ModuleInput::application(ModuleId(20), "app", "app.ts"));
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(10),
            target: ModuleDependencyTarget::Module(ModuleId(11)),
        });
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(11),
            target: ModuleDependencyTarget::Module(ModuleId(12)),
        });
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(20),
            target: ModuleDependencyTarget::Module(ModuleId(12)),
        });
        rows.package_attributions
            .push(rejected_package_ownership(ModuleId(11), "pkg", "1.0.0"));

        let mut report = VersionedPackageMatchReport {
            attributions: vec![
                PackageAttributionInput::accepted_external(ModuleId(10), "pkg", "1.0.0", "pkg"),
                PackageAttributionInput::accepted_external(
                    ModuleId(12),
                    "pkg",
                    "1.0.0",
                    "pkg/leaf",
                ),
            ],
            surfaces: Vec::new(),
            matches: vec![
                package_match(ModuleId(10), "pkg"),
                package_match(ModuleId(12), "pkg/leaf"),
            ],
            version_matches: Vec::new(),
            audit: AuditReport::default(),
        };

        let safety = filter_unsafe_interpackage_external_attributions(&rows, &mut report);

        assert_eq!(safety.removed_modules, 0);
        assert_eq!(
            report
                .attributions
                .iter()
                .map(|attribution| attribution.module_id)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([ModuleId(10), ModuleId(12)]),
            "application modules are a boundary consumer: package consumers still need chain proof, but app code may consume the external package adapter"
        );
        let proofs = externalization_chain_proofs(&rows, &report);
        let leaf_proof = proofs.get(&ModuleId(12)).expect("leaf chain proof");
        assert!(
            leaf_proof
                .get("incoming_consumers")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .any(|consumer| {
                    consumer
                        .get("resolution")
                        .and_then(serde_json::Value::as_str)
                        == Some("application_boundary")
                }),
            "chain proof should record application consumers as boundary consumers"
        );
    }

    #[test]
    fn external_import_safety_allows_builtin_source_boundary_consumers() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "public",
            "pkg/public.ts",
            "pkg",
            Some("1.0.0".to_string()),
        ));
        let mut builtin = ModuleInput::application(ModuleId(20), "node", "node/module.ts");
        builtin.kind = ModuleKind::Builtin;
        rows.modules.push(builtin);
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(20),
            target: ModuleDependencyTarget::Module(ModuleId(10)),
        });

        let mut report = VersionedPackageMatchReport {
            attributions: vec![PackageAttributionInput::accepted_external(
                ModuleId(10),
                "pkg",
                "1.0.0",
                "pkg/public",
            )],
            surfaces: Vec::new(),
            matches: vec![package_match(ModuleId(10), "pkg/public")],
            version_matches: Vec::new(),
            audit: AuditReport::default(),
        };

        let safety = filter_unsafe_interpackage_external_attributions(&rows, &mut report);

        assert_eq!(safety.removed_modules, 0);
        let proofs = externalization_chain_proofs(&rows, &report);
        assert!(
            proofs
                .get(&ModuleId(10))
                .and_then(|proof| proof.get("incoming_consumers"))
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .any(|consumer| {
                    consumer
                        .get("resolution")
                        .and_then(serde_json::Value::as_str)
                        == Some("builtin_boundary")
                }),
            "builtin shim modules are preserved source boundaries for direct external imports"
        );
    }

    #[test]
    fn source_suppression_does_not_eliminate_private_package_needed_by_builtin_consumer() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "root",
            "pkg/root.ts",
            "pkg",
            Some("1.0.0".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(11),
            "private",
            "pkg/private.ts",
            "pkg",
            Some("1.0.0".to_string()),
        ));
        let mut builtin = ModuleInput::application(ModuleId(20), "node", "node/module.ts");
        builtin.kind = ModuleKind::Builtin;
        rows.modules.push(builtin);
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(10),
            target: ModuleDependencyTarget::Module(ModuleId(11)),
        });
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(20),
            target: ModuleDependencyTarget::Module(ModuleId(11)),
        });
        rows.package_attributions
            .push(rejected_package_ownership(ModuleId(11), "pkg", "1.0.0"));

        let report = VersionedPackageMatchReport {
            attributions: vec![PackageAttributionInput::accepted_external(
                ModuleId(10),
                "pkg",
                "1.0.0",
                "pkg",
            )],
            surfaces: Vec::new(),
            matches: vec![package_match(ModuleId(10), "pkg")],
            version_matches: Vec::new(),
            audit: AuditReport::default(),
        };

        assert_eq!(
            source_eliminated_package_modules_for_report(&rows, &report),
            1,
            "private transitive package sources are only suppressed when every consumer can be removed or is an application/package boundary"
        );
    }

    #[test]
    fn external_import_safety_allows_different_package_boundary_consumers() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "public",
            "pkg-a/public.ts",
            "pkg-a",
            Some("1.0.0".to_string()),
        ));
        rows.modules.push(ModuleInput::package(
            ModuleId(20),
            "consumer",
            "pkg-b/consumer.ts",
            "pkg-b",
            Some("1.0.0".to_string()),
        ));
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(20),
            target: ModuleDependencyTarget::Module(ModuleId(10)),
        });

        let mut report = VersionedPackageMatchReport {
            attributions: vec![PackageAttributionInput::accepted_external(
                ModuleId(10),
                "pkg-a",
                "1.0.0",
                "pkg-a",
            )],
            surfaces: Vec::new(),
            matches: vec![package_match(ModuleId(10), "pkg-a")],
            version_matches: Vec::new(),
            audit: AuditReport::default(),
        };

        let safety = filter_unsafe_interpackage_external_attributions(&rows, &mut report);

        assert_eq!(safety.removed_modules, 0);
        let proofs = externalization_chain_proofs(&rows, &report);
        assert!(
            proofs
                .get(&ModuleId(10))
                .and_then(|proof| proof.get("incoming_consumers"))
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .any(|consumer| {
                    consumer
                        .get("resolution")
                        .and_then(serde_json::Value::as_str)
                        == Some("package_boundary")
                }),
            "different package source consumers are boundary consumers; only same-package consumers must be externalized or source-suppressed"
        );
    }

    #[test]
    fn match_packages_externalizes_wildcard_export_from_package_source_root() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let package_dir = tempdir.path().join("project/node_modules/pkg");
        fs::create_dir_all(package_dir.join("lib/features").as_path())
            .expect("create package feature dir");
        fs::write(
            package_dir.join("package.json"),
            r#"{"name":"pkg","version":"1.2.3","exports":{"./features/*":"./lib/features/*.js"}}"#,
        )
        .expect("write package json");
        fs::write(
            package_dir.join("lib/features/add.js"),
            "export function add(a, b) {\n  return a + b;\n}",
        )
        .expect("write package source");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            "export function add(a,b){return a+b}",
            &[],
        );
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: true,
            package_names: vec!["pkg".to_string()],
            package_source_roots: vec![tempdir.path().join("project")],
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
        assert_eq!(outcome.loaded_package_sources, 1);
        assert_eq!(outcome.matched_modules, 1);
        assert!(outcome.function_ownership_matches >= 1);
        let (status, emission_mode, package_version, export_specifier): (
            String,
            String,
            String,
            String,
        ) = connection
            .query_row(
                r"
                SELECT status, emission_mode, package_version, export_specifier
                  FROM package_attributions
                 WHERE module_id = 10
                ",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("wildcard export should be externalized");
        assert_eq!(status, "accepted");
        assert_eq!(emission_mode, "external_import");
        assert_eq!(package_version, "1.2.3");
        assert_eq!(export_specifier, "pkg/features/add");
    }

    #[test]
    fn match_packages_externalizes_conditional_wildcard_export_from_package_source_root() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let package_dir = tempdir.path().join("project/node_modules/pkg");
        fs::create_dir_all(package_dir.join("cjs/features").as_path())
            .expect("create package cjs feature dir");
        fs::create_dir_all(package_dir.join("esm/features").as_path())
            .expect("create package esm feature dir");
        fs::write(
            package_dir.join("package.json"),
            r#"{"name":"pkg","version":"1.2.3","exports":{"./features/*":{"require":"./cjs/features/*.cjs","import":"./esm/features/*.js"}}}"#,
        )
        .expect("write package json");
        fs::write(
            package_dir.join("cjs/features/add.cjs"),
            "exports.add = function add(a, b) {\n  return a + b;\n};",
        )
        .expect("write package cjs source");
        fs::write(
            package_dir.join("esm/features/add.js"),
            "export function add(a, b) {\n  return a + b;\n}",
        )
        .expect("write package esm source");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            "export function add(a,b){return a+b}",
            &[],
        );
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: true,
            package_names: vec!["pkg".to_string()],
            package_source_roots: vec![tempdir.path().join("project")],
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
        assert_eq!(outcome.loaded_package_sources, 2);
        assert_eq!(outcome.matched_modules, 1);
        let export_specifier: String = connection
            .query_row(
                r"
                SELECT export_specifier
                  FROM package_attributions
                 WHERE module_id = 10
                   AND status = 'accepted'
                   AND emission_mode = 'external_import'
                ",
                [],
                |row| row.get(0),
            )
            .expect("conditional wildcard export should be externalized");
        assert_eq!(export_specifier, "pkg/features/add");
    }

    #[test]
    fn match_packages_forces_require_only_conditional_export_external() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let package_dir = tempdir.path().join("project/node_modules/pkg");
        fs::create_dir_all(package_dir.join("cjs/features").as_path())
            .expect("create package cjs feature dir");
        fs::write(
            package_dir.join("package.json"),
            r#"{"name":"pkg","version":"1.2.3","exports":{"./features/*":{"require":"./cjs/features/*.cjs"}}}"#,
        )
        .expect("write package json");
        fs::write(
            package_dir.join("cjs/features/add.cjs"),
            "exports.add = function add(a, b) {\n  return a + b;\n};",
        )
        .expect("write package source");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            "exports.add=function add(a,b){return a+b};",
            &[],
        );
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: true,
            package_names: vec!["pkg".to_string()],
            package_source_roots: vec![tempdir.path().join("project")],
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
        assert_eq!(outcome.loaded_package_sources, 1);
        assert_eq!(outcome.matched_modules, 1);
        assert_eq!(
            outcome.function_attributions, 0,
            "forced module externalization does not imply function-level external evidence"
        );
        let (status, emission_mode, package_version, export_specifier, rejection_reason): (
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = connection
            .query_row(
                r"
                SELECT status, emission_mode, package_version, export_specifier, rejection_reason
                  FROM package_attributions
                 WHERE module_id = 10
                ",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("require-only export should write a rejected ownership attribution");
        assert_eq!(status, "rejected");
        assert_eq!(emission_mode, "application_source");
        assert_eq!(package_version.as_deref(), Some("1.2.3"));
        assert_eq!(export_specifier, None);
        assert!(rejection_reason.is_some());
    }

    #[test]
    fn match_packages_forces_ambiguous_wildcard_export_external() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let package_dir = tempdir.path().join("project/node_modules/pkg");
        fs::create_dir_all(package_dir.join("lib").as_path()).expect("create package lib dir");
        fs::write(
            package_dir.join("package.json"),
            r#"{"name":"pkg","version":"1.2.3","exports":{"./a/*":"./lib/*.js","./b/*":"./lib/*.js"}}"#,
        )
        .expect("write package json");
        fs::write(
            package_dir.join("lib/add.js"),
            "export function add(a, b) {\n  return a + b;\n}",
        )
        .expect("write package source");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            "export function add(a,b){return a+b}",
            &[],
        );
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: true,
            package_names: vec!["pkg".to_string()],
            package_source_roots: vec![tempdir.path().join("project")],
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
        assert_eq!(outcome.loaded_package_sources, 1);
        assert_eq!(outcome.matched_modules, 1);
        assert_eq!(
            outcome.function_attributions, 0,
            "forced module externalization does not imply function-level external evidence"
        );
        let (status, emission_mode, package_version, export_specifier): (
            String,
            String,
            Option<String>,
            Option<String>,
        ) = connection
            .query_row(
                r"
                SELECT status, emission_mode, package_version, export_specifier
                  FROM package_attributions
                 WHERE module_id = 10
                ",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("ambiguous wildcard export should write a rejected ownership attribution");
        assert_eq!(status, "rejected");
        assert_eq!(emission_mode, "application_source");
        assert_eq!(package_version.as_deref(), Some("1.2.3"));
        assert_eq!(export_specifier, None);
    }

    #[test]
    fn match_packages_uses_package_source_root_without_cache_table() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let package_dir = tempdir.path().join("node_modules/pkg");
        fs::create_dir_all(package_dir.join("lib").as_path()).expect("create package lib dir");
        fs::write(
            package_dir.join("package.json"),
            r#"{"name":"pkg","version":"1.2.3"}"#,
        )
        .expect("write package json");
        fs::write(
            package_dir.join("lib/add.js"),
            "export function add(a, b) {\n  return a + b;\n}",
        )
        .expect("write package source");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            "export function add(a,b){return a+b}",
            &[],
        );
        connection
            .execute("DROP TABLE package_source_cache", [])
            .expect("drop cache table");
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: false,
            package_names: vec!["pkg".to_string()],
            package_source_roots: vec![tempdir.path().to_path_buf()],
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
        assert_eq!(outcome.loaded_package_sources, 1);
        assert_eq!(outcome.matched_modules, 1);
        assert!(
            outcome.function_attributions >= 1,
            "unrestricted subpath package source should be importable even in dry-run"
        );
        assert_eq!(package_attribution_count(&connection), 0);
    }

    #[test]
    fn match_packages_promotes_full_cascade_function_coverage_to_module_attribution() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            "module.exports = function add(a,b){return a+b};",
            &[(
                "pkg",
                "1.2.3",
                "add.js",
                "const add = function add(a, b) {\n  return a + b;\n};",
            )],
        );
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: true,
            package_names: Vec::new(),
            package_source_roots: Vec::new(),
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
        assert_eq!(
            outcome.matched_modules, 1,
            "module-level attribution should be backed by function evidence"
        );
        assert_eq!(outcome.written_attributions, 1);
        assert!(outcome.written_function_attributions >= 1);
        let (status, emission_mode, package_version, evidence): (String, String, String, String) =
            connection
                .query_row(
                    r"
                SELECT status, emission_mode, package_version, evidence_json
                  FROM package_attributions
                 WHERE module_id = 10
                ",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .expect("cascade module attribution should be written");
        assert_eq!(status, "accepted");
        assert_eq!(emission_mode, "external_import");
        assert_eq!(package_version, "1.2.3");
        assert!(evidence.contains("cascade_function_coverage"));
    }

    #[test]
    fn match_packages_forces_source_only_cascade_ownership_external() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let package_dir = tempdir.path().join("project/node_modules/pkg");
        fs::create_dir_all(package_dir.join("lib").as_path()).expect("create package lib dir");
        fs::write(
            package_dir.join("package.json"),
            r#"{"name":"pkg","version":"1.2.3","exports":{"./add":{"require":"./lib/add.js"}}}"#,
        )
        .expect("write package json");
        fs::write(
            package_dir.join("lib/add.js"),
            "const add = function add(a, b) {\n  return a + b;\n};",
        )
        .expect("write package source");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            "module.exports = function add(a,b){return a+b};",
            &[],
        );
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: true,
            package_names: vec!["pkg".to_string()],
            package_source_roots: vec![tempdir.path().join("project")],
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
        assert_eq!(outcome.loaded_package_sources, 1);
        assert_eq!(
            outcome.matched_modules, 1,
            "source-only cascade coverage should still count as package ownership"
        );
        assert_eq!(
            outcome.function_ownership_matches, 1,
            "the source-only function should still produce one ownership match"
        );
        assert_eq!(
            outcome.function_attributions, 0,
            "source-only ownership must not become function-level external attributions"
        );
        assert_eq!(outcome.written_function_attributions, 0);
        assert_eq!(
            outcome.written_attributions, 1,
            "the module should receive a rejected ownership decision when the import target is unsafe"
        );

        let (
            status,
            emission_mode,
            package_version,
            export_specifier,
            rejection_reason,
            evidence,
        ): (
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
        ) = connection
            .query_row(
                r"
                SELECT status, emission_mode, package_version, export_specifier,
                       rejection_reason, evidence_json
                  FROM package_attributions
                 WHERE module_id = 10
                ",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .expect("source-only function ownership should write a rejected attribution");
        assert_eq!(status, "rejected");
        assert_eq!(emission_mode, "application_source");
        assert_eq!(package_version.as_deref(), Some("1.2.3"));
        assert_eq!(export_specifier, None);
        assert!(rejection_reason.is_some());
        assert!(evidence.contains("cascade_function_coverage"));
    }

    #[test]
    fn match_packages_forces_structural_bag_ownership_external() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            r#"
            function a(x){if(x){return true;}return false;}
            function b(y){if(y){return true;}return false;}
            "#,
            &[(
                "pkg",
                "1.2.3",
                "combined.js",
                r#"
                function first(value){if(value){return true;}return false;}
                function second(input){if(input){return true;}return false;}
                "#,
            )],
        );
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: true,
            package_names: Vec::new(),
            package_source_roots: Vec::new(),
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
        assert_eq!(
            outcome.matched_modules, 1,
            "structural bag evidence should be promoted as source-only ownership"
        );
        assert_eq!(
            outcome.function_ownership_matches, 0,
            "this fixture should not be matched by cascade"
        );
        assert_eq!(
            outcome.written_attributions, 1,
            "unmatched package modules still receive an explicit rejected source decision"
        );
        let (attribution_count, evidence): (i64, String) = connection
            .query_row(
                r"
                SELECT COUNT(*), COALESCE(MAX(evidence_json), '')
                  FROM package_attributions
                 WHERE module_id = 10
                ",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("count attribution rows");
        assert_eq!(attribution_count, 1);
        assert!(evidence.contains("aggregate_structural_bag_similarity"));
        assert!(evidence.contains("structural-bag:pkg@1.2.3"));
    }

    #[test]
    fn match_packages_promotes_dependency_closure_ownership_for_wrapper() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let wrapper_path = tempdir.path().join("wrapper.js");
        let one_path = tempdir.path().join("one.js");
        let two_path = tempdir.path().join("two.js");
        let mut connection = package_match_connection(
            wrapper_path.clone(),
            "var wrap = E(() => { one(); two(); });",
            &[
                (
                    "pkg",
                    "1.2.3",
                    "one.js",
                    "export function one(){return 'one-anchor';}",
                ),
                (
                    "pkg",
                    "1.2.3",
                    "two.js",
                    "export function two(){return 'two-anchor';}",
                ),
            ],
        );
        fs::write(
            one_path.as_path(),
            "export function one(){return 'one-anchor';}",
        )
        .expect("write one source");
        fs::write(
            two_path.as_path(),
            "export function two(){return 'two-anchor';}",
        )
        .expect("write two source");
        connection
            .execute(
                "INSERT INTO source_files (id, file_path) VALUES (2, ?1), (3, ?2)",
                params![
                    one_path.to_string_lossy().as_ref(),
                    two_path.to_string_lossy().as_ref()
                ],
            )
            .expect("insert source files");
        connection
            .execute(
                "INSERT INTO project_files (project_id, file_id) VALUES (1, 2), (1, 3)",
                [],
            )
            .expect("insert project files");
        connection
            .execute_batch(
                r"
                UPDATE modules
                   SET semantic_name = 'pkg/wrapper.js',
                       byte_start = NULL,
                       byte_end = NULL
                 WHERE id = 10;
                INSERT INTO modules
                    (id, file_id, original_name, semantic_name, module_category,
                     package_name, package_version, byte_start, byte_end)
                VALUES
                    (11, 2, 'one', 'pkg/one.js', 'package', 'pkg', '1.2.3', NULL, NULL),
                    (12, 3, 'two', 'pkg/two.js', 'package', 'pkg', '1.2.3', NULL, NULL);
                INSERT INTO module_dependencies (module_id, dependency_id)
                VALUES (10, 11), (10, 12);
                ",
            )
            .expect("seed dependency closure fixture");
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: true,
            package_names: Vec::new(),
            package_source_roots: Vec::new(),
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
        assert_eq!(outcome.matched_modules, 3);
        assert_eq!(outcome.written_attributions, 3);
        let (status, emission_mode, package_version, evidence): (
            String,
            String,
            Option<String>,
            String,
        ) = connection
            .query_row(
                r"
                SELECT status, emission_mode, package_version, evidence_json
                  FROM package_attributions
                 WHERE module_id = 10
                ",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("wrapper should have rejected ownership attribution");
        assert_eq!(status, "rejected");
        assert_eq!(emission_mode, "application_source");
        assert_eq!(package_version.as_deref(), Some("1.2.3"));
        assert!(evidence.contains("dependency_closure_ownership"));
        assert!(evidence.contains("exact-hint:pkg@1.2.3"));
    }

    #[test]
    fn match_packages_resolves_weak_unversioned_hint_to_forced_external() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let dependency_path = tempdir.path().join("axios.js");
        let mut connection = package_match_connection(
            tempdir.path().join("rxjs-wrapper.js"),
            "var r = E(() => { axiosDep(); });",
            &[(
                "rxjs",
                "7.8.2",
                "sample.js",
                "export function sample(notifier){return notifier;}",
            )],
        );
        connection
            .execute(
                "UPDATE modules SET semantic_name = 'rxjs/operators/sample', package_name = 'rxjs', package_version = NULL WHERE id = 10",
                [],
            )
            .expect("make weak rxjs hint");
        fs::write(dependency_path.as_path(), "export const axiosDep = 1;")
            .expect("write dependency source");
        connection
            .execute(
                "INSERT INTO source_files (id, file_path) VALUES (2, ?1)",
                [dependency_path.to_string_lossy().as_ref()],
            )
            .expect("insert dependency source file");
        connection
            .execute(
                "INSERT INTO project_files (project_id, file_id) VALUES (1, 2)",
                [],
            )
            .expect("insert dependency project file");
        connection
            .execute_batch(
                r"
                INSERT INTO modules
                    (id, file_id, original_name, semantic_name, module_category,
                     package_name, package_version, byte_start, byte_end)
                VALUES (11, 2, 'axiosDep', 'axios/index.js', 'package', 'axios', '1.7.3', NULL, NULL);
                INSERT INTO module_dependencies (module_id, dependency_id) VALUES (10, 11);
                ",
            )
            .expect("seed contradicted dependency");
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: true,
            package_names: vec!["rxjs".to_string()],
            package_source_roots: Vec::new(),
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
        assert_eq!(outcome.matched_modules, 1);
        assert_eq!(
            outcome.written_attributions, 2,
            "component-scoped package matching also writes the dependency package rejection"
        );
        let (status, emission_mode, package_version, rejection_reason, evidence_json): (
            String,
            String,
            Option<String>,
            Option<String>,
            String,
        ) = connection
            .query_row(
                r"
                SELECT status, emission_mode, package_version, rejection_reason, evidence_json
                  FROM package_attributions
                 WHERE module_id = 10
                ",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("weak resolved hint should write rejected ownership evidence");
        assert_eq!(status, "rejected");
        assert_eq!(emission_mode, "application_source");
        assert_eq!(package_version.as_deref(), Some("7.8.2"));
        assert!(rejection_reason.is_some());
        assert!(evidence_json.contains("rxjs"));
    }

    #[test]
    fn match_packages_forces_partial_cascade_coverage_external() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            r#"
            function first(value) {
              if (value > 0) {
                return value + 1;
              }
              return 0;
            }
            function second(limit) {
              let total = 0;
              for (let i = 0; i < limit; i++) {
                total += i;
              }
              return total;
            }
            function localOnly() { return 3; }
            exports.value = first(1) + second(2) + localOnly();
            "#,
            &[(
                "pkg",
                "1.2.3",
                "partial.js",
                r#"
                function first(value) {
                  if (value > 0) {
                    return value + 1;
                  }
                  return 0;
                }
                function second(limit) {
                  let total = 0;
                  for (let i = 0; i < limit; i++) {
                    total += i;
                  }
                  return total;
                }
                exports.value = first(1) + second(2);
                "#,
            )],
        );
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: true,
            package_names: Vec::new(),
            package_source_roots: Vec::new(),
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
        assert_eq!(
            outcome.matched_modules, 1,
            "2/3 function ownership should pass the partial cascade threshold"
        );
        assert_eq!(
            outcome.written_attributions, 1,
            "partial ownership should be persisted as rejected source-retention evidence"
        );
        assert!(
            outcome.written_function_attributions >= 2,
            "function-level cascade evidence should still be recorded"
        );
        let (status, emission_mode, package_version, export_specifier, rejection_reason): (
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = connection
            .query_row(
                r"
                SELECT status, emission_mode, package_version, export_specifier,
                       rejection_reason
                  FROM package_attributions
                 WHERE module_id = 10
                ",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("partial function ownership should write a rejected ownership attribution");
        assert_eq!(status, "rejected");
        assert_eq!(emission_mode, "application_source");
        assert_eq!(package_version.as_deref(), Some("1.2.3"));
        assert_eq!(export_specifier, None);
        assert!(rejection_reason.is_some());
    }

    #[test]
    fn match_packages_scopes_cascade_by_module_package_version_hint() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            "module.exports = function add(a,b){return a+b};",
            &[
                (
                    "pkg",
                    "1.0.0",
                    "add.js",
                    "const add = function add(a, b) {\n  return a + b;\n};",
                ),
                (
                    "pkg",
                    "2.0.0",
                    "add.js",
                    "const add = function add(a, b) {\n  return a + b;\n};",
                ),
            ],
        );
        connection
            .execute(
                "UPDATE modules SET package_version = '2.0.0' WHERE id = 10",
                [],
            )
            .expect("set package version hint");
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: true,
            package_names: Vec::new(),
            package_source_roots: Vec::new(),
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
        assert_eq!(outcome.matched_modules, 1);
        let package_version: String = connection
            .query_row(
                r"
                SELECT package_version
                  FROM package_attributions
                 WHERE module_id = 10
                   AND status = 'accepted'
                ",
                [],
                |row| row.get(0),
            )
            .expect("cascade module attribution should be written");
        assert_eq!(package_version, "2.0.0");
    }

    #[test]
    fn match_packages_package_name_filter_skips_unrequested_modules() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let other_source_path = tempdir.path().join("other.js");
        fs::write(other_source_path.as_path(), "function broken(").expect("write source fixture");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            "export function add(a,b){return a+b}",
            &[(
                "pkg",
                "1.2.3",
                "add.js",
                "export function add(a, b) {\n  return a + b;\n}",
            )],
        );
        connection
            .execute(
                "INSERT INTO source_files (id, file_path) VALUES (2, ?1)",
                [other_source_path.to_string_lossy().as_ref()],
            )
            .expect("insert source file");
        connection
            .execute(
                "INSERT INTO project_files (project_id, file_id) VALUES (1, 2)",
                [],
            )
            .expect("insert project file");
        connection
            .execute(
                r"
                INSERT INTO modules
                    (id, file_id, original_name, semantic_name, module_category,
                     package_name, package_version, byte_start, byte_end)
                VALUES (11, 2, 'other', 'other/index.js', 'package', 'other', NULL, 0, ?1)
                ",
                ["function broken(".len() as i64],
            )
            .expect("insert unrequested module");
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: false,
            package_names: vec!["pkg".to_string()],
            package_source_roots: Vec::new(),
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(
            outcome.audit.has(FindingCode::AstFactExtractionFailed),
            "{:?}",
            outcome.audit.findings()
        );
        assert_eq!(outcome.loaded_package_modules, 2);
        assert_eq!(outcome.loaded_package_sources, 1);
        assert_eq!(outcome.matched_modules, 1);
        assert_eq!(outcome.matched_package_surfaces, 0);
    }

    #[test]
    fn match_packages_apply_writes_best_version_from_binary_matcher() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            "export function add(a,b){return a+b}",
            &[
                (
                    "pkg",
                    "2.0.0",
                    "add.js",
                    "export function sub(a,b){return a-b}",
                ),
                (
                    "pkg",
                    "1.2.3",
                    "add.js",
                    "export function add(a, b) {\n  return a + b;\n}",
                ),
            ],
        );
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: true,
            package_names: Vec::new(),
            package_source_roots: Vec::new(),
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");
        let (package_version, evidence): (String, String) = connection
            .query_row(
                "SELECT package_version, evidence_json FROM package_attributions WHERE module_id = 10",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("package attribution should be written");

        assert!(outcome.audit.is_clean());
        assert_eq!(outcome.matched_modules, 1);
        assert_eq!(outcome.matched_package_surfaces, 0);
        assert_eq!(outcome.written_attributions, 1);
        assert_eq!(outcome.written_surfaces, 0);
        assert_eq!(package_version, "1.2.3");
        assert!(evidence.contains("exact_normalized_source_binary_search"));
    }

    #[test]
    fn match_packages_apply_writes_function_attribution() {
        // The assertion looks at the `package_function_attributions` table populated
        // by the function-level matcher. It should produce an Exact-tier match
        // for the bundle's `add` function against the package source, and
        // persist it with function_span + confidence rather than discarding
        // the row.
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            "export function add(a,b){return a+b}",
            &[(
                "pkg",
                "1.2.3",
                "add.js",
                "export function add(a, b) {\n  return a + b;\n}",
            )],
        );
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: true,
            package_names: Vec::new(),
            package_source_roots: Vec::new(),
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean());
        assert!(
            outcome.written_function_attributions >= 1,
            "expected function attribution to be persisted, outcome={:?}",
            outcome,
        );

        let (tier, span_start, span_end, package_name, package_version, matched_axes_json): (
            String,
            i64,
            i64,
            String,
            String,
            String,
        ) = connection
            .query_row(
                r"
                SELECT tier, function_span_start, function_span_end,
                       package_name, package_version, matched_axes_json
                  FROM package_function_attributions
                 WHERE module_id = 10
                ",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .expect("cascade function attribution row must exist");

        assert_eq!(tier, "exact");
        assert_eq!(package_name, "pkg");
        assert_eq!(package_version, "1.2.3");
        assert!(span_end > span_start);
        assert!(matched_axes_json.contains("ast"));
    }

    #[test]
    fn match_packages_dry_run_does_not_persist_function_attributions() {
        // With apply=false, the function-level matcher still runs (the diagnostic
        // count is non-zero in the outcome), but no rows land in the new
        // function-attributions table.
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            "export function add(a,b){return a+b}",
            &[(
                "pkg",
                "1.2.3",
                "add.js",
                "export function add(a, b) {\n  return a + b;\n}",
            )],
        );
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: false,
            package_names: Vec::new(),
            package_source_roots: Vec::new(),
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(
            outcome.function_attributions >= 1,
            "function matcher should compute"
        );
        assert_eq!(outcome.written_function_attributions, 0);
        // The new table should not exist yet since persistence never ran.
        let table_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='package_function_attributions'",
                [],
                |row| row.get(0),
            )
            .expect("sqlite_master is always queryable");
        assert_eq!(table_count, 0);
    }

    #[test]
    fn unversioned_package_versions_resolve_to_latest_cached_version() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            "export function add(a,b){return a+b}",
            &[
                (
                    "pkg",
                    "1.2.3",
                    "add.js",
                    "export function add(a,b){return a+b}",
                ),
                (
                    "pkg",
                    "2.0.0",
                    "add.js",
                    "export function add(a,b){return a+b}",
                ),
            ],
        );
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: true,
            package_names: Vec::new(),
            package_source_roots: Vec::new(),
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");
        let (
            status,
            rejection_reason,
            package_version,
            emission_mode,
            external_import_policy_version,
        ): (String, Option<String>, Option<String>, String, i64) = connection
            .query_row(
                r"
                SELECT status, rejection_reason, package_version, emission_mode,
                       external_import_policy_version
                  FROM package_attributions
                 WHERE module_id = 10
                ",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("resolved attribution should be written");
        let package_version_not_null: i64 = connection
            .query_row(
                r"
                SELECT [notnull]
                  FROM pragma_table_info('package_attributions')
                 WHERE name = 'package_version'
                ",
                [],
                |row| row.get(0),
            )
            .expect("package_version column should exist");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
        assert_eq!(outcome.matched_modules, 1);
        assert_eq!(outcome.matched_package_surfaces, 0);
        assert_eq!(outcome.written_attributions, 1);
        assert_eq!(outcome.written_surfaces, 0);
        assert_eq!(package_attribution_count(&connection), 1);
        assert_eq!(status, "accepted");
        assert_eq!(rejection_reason, None);
        assert_eq!(package_version.as_deref(), Some("2.0.0"));
        assert_eq!(emission_mode, "external_import");
        assert_eq!(
            external_import_policy_version,
            PACKAGE_ATTRIBUTION_EXTERNAL_IMPORT_POLICY_VERSION
        );
        assert_eq!(package_version_not_null, 0);
    }

    #[test]
    fn match_packages_apply_replaces_proposed_rows_with_forced_external_decisions() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut connection = package_match_connection(
            tempdir.path().join("bundle.js"),
            "export function add(a,b){return a+b}",
            &[],
        );
        connection
            .execute(
                r"
                INSERT INTO package_attributions
                    (module_id, module_original_name, package_name, package_version,
                     package_subpath, resolved_file, export_specifier, emission_mode,
                     status, evidence_json, rejection_reason, created_at, updated_at)
                VALUES
                    (10, 'm10', 'pkg', '0.0.0', NULL, NULL, NULL,
                     'external_import', 'proposed', NULL, NULL, 'now', 'now')
                ",
                [],
            )
            .expect("insert proposed attribution");
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: true,
            package_names: Vec::new(),
            package_source_roots: Vec::new(),
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");
        let (status, emission_mode, package_version, export_specifier, rejection_reason): (
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = connection
            .query_row(
                r"
                SELECT status, emission_mode, package_version, export_specifier, rejection_reason
                  FROM package_attributions
                 WHERE module_id = 10
                ",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("proposed row should be replaced");

        assert_eq!(outcome.matched_modules, 0);
        assert_eq!(outcome.written_attributions, 1);
        assert_eq!(package_attribution_count(&connection), 1);
        assert_eq!(status, "rejected");
        assert_eq!(emission_mode, "application_source");
        assert_eq!(package_version, None);
        assert_eq!(export_specifier, None);
        assert!(rejection_reason.is_some());
        reverts_input::sqlite::load_project_bundle_from_connection(&connection, 1)
            .expect("forced external attribution should satisfy generation input contract");
    }

    #[test]
    fn match_packages_apply_writes_source_import_package_surface() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let source_path = tempdir.path().join("bundle.js");
        fs::write(
            source_path.as_path(),
            "const client = require('undici'); export { client };",
        )
        .expect("write source fixture");
        let mut connection = Connection::open_in_memory().expect("open in-memory database");
        create_source_surface_schema(&connection);
        insert_source_surface_rows(&connection, source_path.to_string_lossy().as_ref());
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: true,
            package_names: Vec::new(),
            package_source_roots: Vec::new(),
            materialize_package_sources: false,
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");
        let (package_version, evidence): (String, String) = connection
            .query_row(
                "SELECT package_version, evidence_json FROM package_surfaces WHERE export_specifier = 'undici'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("package surface should be written");

        assert!(outcome.audit.is_clean());
        assert_eq!(outcome.matched_modules, 0);
        assert_eq!(outcome.matched_package_surfaces, 1);
        assert_eq!(outcome.written_attributions, 0);
        assert_eq!(outcome.written_surfaces, 1);
        assert_eq!(package_version, "2.2.1");
        assert!(evidence.contains("source_package_import_surface"));
    }

    #[test]
    fn cli_match_packages_then_generate_project_uses_written_attribution() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let database_path = tempdir.path().join("input.db");
        let app_source_path = tempdir.path().join("app.ts");
        let package_slice_path = tempdir.path().join("pkg-add.js");
        let output_dir = tempdir.path().join("out");
        let app_source = "import { add } from 'pkg/add';\nexport const total = add(1, 2);";
        let package_slice = "export function add(a,b){return a+b}";
        fs::write(app_source_path.as_path(), app_source).expect("write app source");
        fs::write(package_slice_path.as_path(), package_slice).expect("write package slice");
        let connection = Connection::open(database_path.as_path()).expect("open fixture database");
        create_match_generate_schema(&connection);
        insert_match_generate_rows(
            &connection,
            app_source_path.to_string_lossy().as_ref(),
            package_slice_path.to_string_lossy().as_ref(),
            app_source.len() as i64,
            package_slice.len() as i64,
        );
        drop(connection);

        run([
            "match-packages".to_string(),
            "--input".to_string(),
            database_path.to_string_lossy().into_owned(),
            "--project-id".to_string(),
            "1".to_string(),
            "--apply".to_string(),
        ])
        .expect("package matching should persist attribution");
        run([
            "generate-project-v2".to_string(),
            "--input".to_string(),
            database_path.to_string_lossy().into_owned(),
            "--project-id".to_string(),
            "1".to_string(),
            "--output".to_string(),
            output_dir.to_string_lossy().into_owned(),
        ])
        .expect("generation should consume persisted attribution");

        let generated_source = fs::read_to_string(output_dir.join("modules/1-entry.ts"))
            .expect("generated entry should be written");
        assert!(generated_source.contains("import { add } from 'pkg/add';"));
        assert!(generated_source.contains("export const total = add(1, 2);"));
        assert!(!generated_source.contains("__pkg_pkg_add"));
        let connection = Connection::open(database_path).expect("reopen fixture database");
        assert_eq!(package_attribution_count(&connection), 1);
        assert_eq!(package_surface_count(&connection), 1);
    }

    #[test]
    fn cli_extract_assets_then_generate_project_materializes_assets() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let database_path = tempdir.path().join("input.db");
        let app_source_path = tempdir.path().join("app.ts");
        let asset_path = tempdir.path().join("addon.node");
        let output_dir = tempdir.path().join("out");
        let app_source = "const native = require('/$bunfs/root/addon.node'); export { native };";
        fs::write(app_source_path.as_path(), app_source).expect("write app source");
        fs::write(asset_path.as_path(), b"native").expect("write native asset");
        let connection = Connection::open(database_path.as_path()).expect("open fixture database");
        create_match_generate_schema(&connection);
        connection
            .execute("INSERT INTO projects (id, name) VALUES (1, 'fixture')", [])
            .expect("insert project");
        connection
            .execute(
                "INSERT INTO source_files (id, file_path) VALUES (1, ?1)",
                [app_source_path.to_string_lossy().as_ref()],
            )
            .expect("insert source file");
        connection
            .execute(
                "INSERT INTO project_files (project_id, file_id) VALUES (1, 1)",
                [],
            )
            .expect("insert project file");
        connection
            .execute(
                r"
                INSERT INTO modules
                    (id, file_id, original_name, semantic_name, module_category,
                     package_name, package_version, byte_start, byte_end)
                VALUES (1, 1, 'entry', 'src/index', 'application', NULL, NULL, 0, ?1)
                ",
                [app_source.len() as i64],
            )
            .expect("insert app module");
        drop(connection);

        run([
            "extract-assets".to_string(),
            "--input".to_string(),
            database_path.to_string_lossy().into_owned(),
            "--project-id".to_string(),
            "1".to_string(),
            "--apply".to_string(),
        ])
        .expect("asset extraction should persist project_assets");
        run([
            "generate-project-v2".to_string(),
            "--input".to_string(),
            database_path.to_string_lossy().into_owned(),
            "--project-id".to_string(),
            "1".to_string(),
            "--output".to_string(),
            output_dir.to_string_lossy().into_owned(),
        ])
        .expect("generation should consume persisted asset");

        let generated_source = fs::read_to_string(output_dir.join("modules/1-src/index.ts"))
            .expect("generated entry should be written");
        assert!(generated_source.contains("require('./addon.node')"));
        assert!(!generated_source.contains("/$bunfs/root/addon.node"));
        assert_eq!(
            fs::read(output_dir.join("modules/1-src/addon.node")).expect("asset should be written"),
            b"native"
        );
        assert!(
            fs::read_to_string(output_dir.join("package.json"))
                .expect("package json")
                .contains("node ./scripts/copy-assets.mjs")
        );
        let connection = Connection::open(database_path).expect("reopen fixture database");
        let stored_asset_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM project_assets", [], |row| row.get(0))
            .expect("count project assets");
        assert_eq!(stored_asset_count, 1);
    }

    #[test]
    fn cli_extract_assets_can_materialize_bun_embedded_native_asset() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let database_path = tempdir.path().join("input.db");
        let app_source_path = tempdir.path().join("app.ts");
        let bun_executable_path = tempdir.path().join("fixture-bun");
        let output_dir = tempdir.path().join("out");
        let logical_path = "/$bunfs/root/native.node";
        let native_bytes = minimal_elf64_bytes();
        let app_source = format!("const native = require('{logical_path}'); export {{ native }};");
        let mut bun_executable = Vec::new();
        bun_executable.extend_from_slice(b"not the asset /$bunfs/root/native.node);");
        bun_executable.extend_from_slice(logical_path.as_bytes());
        bun_executable.push(0);
        bun_executable.extend_from_slice(native_bytes.as_slice());
        bun_executable.extend_from_slice(b"\0---- Bun! ----\n");
        fs::write(app_source_path.as_path(), app_source.as_str()).expect("write app source");
        fs::write(bun_executable_path.as_path(), bun_executable).expect("write bun executable");
        let connection = Connection::open(database_path.as_path()).expect("open fixture database");
        create_match_generate_schema(&connection);
        connection
            .execute("INSERT INTO projects (id, name) VALUES (1, 'fixture')", [])
            .expect("insert project");
        connection
            .execute(
                "INSERT INTO source_files (id, file_path) VALUES (1, ?1)",
                [app_source_path.to_string_lossy().as_ref()],
            )
            .expect("insert source file");
        connection
            .execute(
                "INSERT INTO project_files (project_id, file_id) VALUES (1, 1)",
                [],
            )
            .expect("insert project file");
        connection
            .execute(
                r"
                INSERT INTO modules
                    (id, file_id, original_name, semantic_name, module_category,
                     package_name, package_version, byte_start, byte_end)
                VALUES (1, 1, 'entry', 'src/index', 'application', NULL, NULL, 0, ?1)
                ",
                [app_source.len() as i64],
            )
            .expect("insert app module");
        drop(connection);

        run([
            "extract-assets".to_string(),
            "--input".to_string(),
            database_path.to_string_lossy().into_owned(),
            "--project-id".to_string(),
            "1".to_string(),
            "--asset-root".to_string(),
            bun_executable_path.to_string_lossy().into_owned(),
            "--apply".to_string(),
        ])
        .expect("asset extraction should persist embedded asset");
        run([
            "generate-project-v2".to_string(),
            "--input".to_string(),
            database_path.to_string_lossy().into_owned(),
            "--project-id".to_string(),
            "1".to_string(),
            "--output".to_string(),
            output_dir.to_string_lossy().into_owned(),
        ])
        .expect("generation should consume persisted embedded asset");

        assert_eq!(
            fs::read(output_dir.join("modules/1-src/native.node"))
                .expect("embedded asset should be written"),
            native_bytes
        );
        let connection = Connection::open(database_path).expect("reopen fixture database");
        let stored_source_path: String = connection
            .query_row(
                "SELECT source_path FROM project_assets WHERE logical_path = ?1",
                [logical_path],
                |row| row.get(0),
            )
            .expect("stored embedded asset");
        assert!(PathBuf::from(stored_source_path).is_file());
    }

    #[test]
    fn cli_extract_assets_accepts_multiple_roots_for_bun_and_vendor_assets() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let database_path = tempdir.path().join("input.db");
        let app_source_path = tempdir.path().join("app.ts");
        let bun_executable_path = tempdir.path().join("fixture-bun");
        let vendor_root = tempdir.path().join("package-root");
        let output_dir = tempdir.path().join("out");
        let native_logical_path = "/$bunfs/root/native.node";
        let native_bytes = minimal_elf64_bytes();
        let rg_path = vendor_root.join("vendor/ripgrep/x64-linux/rg");
        let app_source = format!(
            "\
            const native = require('{native_logical_path}');\n\
            const POL = {{ fileURLToPath(value) {{ return value; }} }};\n\
            const ODH = {{ join(...parts) {{ return parts.join('/'); }}, resolve(...parts) {{ return parts.join('/'); }} }};\n\
            const here = POL.fileURLToPath('file:///home/runner/work/app/src/tools/ripgrep.ts');\n\
            const base = ODH.join(here, '../');\n\
            const vendor = ODH.resolve(base, 'vendor', 'ripgrep');\n\
            const rg = ODH.resolve(vendor, 'x64-linux', 'rg');\n\
            export {{ native, rg }};"
        );
        let mut bun_executable = Vec::new();
        bun_executable.extend_from_slice(native_logical_path.as_bytes());
        bun_executable.push(0);
        bun_executable.extend_from_slice(native_bytes.as_slice());
        bun_executable.extend_from_slice(b"\0---- Bun! ----\n");
        fs::write(app_source_path.as_path(), app_source.as_str()).expect("write app source");
        fs::write(bun_executable_path.as_path(), bun_executable).expect("write bun executable");
        fs::create_dir_all(rg_path.parent().expect("rg parent")).expect("create vendor dirs");
        fs::write(rg_path.as_path(), b"rg-binary").expect("write rg");
        let connection = Connection::open(database_path.as_path()).expect("open fixture database");
        create_match_generate_schema(&connection);
        connection
            .execute("INSERT INTO projects (id, name) VALUES (1, 'fixture')", [])
            .expect("insert project");
        connection
            .execute(
                "INSERT INTO source_files (id, file_path) VALUES (1, ?1)",
                [app_source_path.to_string_lossy().as_ref()],
            )
            .expect("insert source file");
        connection
            .execute(
                "INSERT INTO project_files (project_id, file_id) VALUES (1, 1)",
                [],
            )
            .expect("insert project file");
        connection
            .execute(
                r"
                INSERT INTO modules
                    (id, file_id, original_name, semantic_name, module_category,
                     package_name, package_version, byte_start, byte_end)
                VALUES (1, 1, 'entry', 'src/index', 'application', NULL, NULL, 0, ?1)
                ",
                [app_source.len() as i64],
            )
            .expect("insert app module");
        drop(connection);

        run([
            "extract-assets".to_string(),
            "--input".to_string(),
            database_path.to_string_lossy().into_owned(),
            "--project-id".to_string(),
            "1".to_string(),
            "--asset-root".to_string(),
            bun_executable_path.to_string_lossy().into_owned(),
            "--asset-root".to_string(),
            vendor_root.to_string_lossy().into_owned(),
            "--apply".to_string(),
        ])
        .expect("asset extraction should persist assets from both roots");
        run([
            "generate-project-v2".to_string(),
            "--input".to_string(),
            database_path.to_string_lossy().into_owned(),
            "--project-id".to_string(),
            "1".to_string(),
            "--output".to_string(),
            output_dir.to_string_lossy().into_owned(),
        ])
        .expect("generation should consume persisted multi-root assets");

        assert_eq!(
            fs::read(output_dir.join("modules/1-src/native.node")).expect("native asset"),
            native_bytes
        );
        assert_eq!(
            fs::read(output_dir.join("modules/1-src/vendor/ripgrep/x64-linux/rg"))
                .expect("rg asset"),
            b"rg-binary"
        );
        let generated_source = fs::read_to_string(output_dir.join("modules/1-src/index.ts"))
            .expect("generated source");
        assert!(generated_source.contains("POL.fileURLToPath(import.meta.url)"));
        assert!(!generated_source.contains("/home/runner/work/app"));
        let connection = Connection::open(database_path).expect("reopen fixture database");
        let stored_assets: i64 = connection
            .query_row("SELECT COUNT(*) FROM project_assets", [], |row| row.get(0))
            .expect("count project assets");
        assert_eq!(stored_assets, 2);
    }

    #[test]
    fn bun_embedded_asset_extractor_reads_wasm_payload_without_trailing_bun_metadata() {
        let mut executable = Vec::new();
        executable.extend_from_slice(b"prefix");
        executable.extend_from_slice(b"/$bunfs/root/parser.wasm");
        executable.push(0);
        executable.extend_from_slice(minimal_wasm_bytes().as_slice());
        executable.extend_from_slice(b"\0---- Bun! ----\nmetadata");

        let extracted = extract_bun_embedded_asset_from_bytes(
            executable.as_slice(),
            "/$bunfs/root/parser.wasm",
        )
        .expect("wasm asset should be extracted");

        assert_eq!(extracted, minimal_wasm_bytes());
    }

    fn minimal_elf64_bytes() -> Vec<u8> {
        let mut bytes = vec![0; 128];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[0x28..0x30].copy_from_slice(&(64_u64).to_le_bytes());
        bytes[0x34..0x36].copy_from_slice(&(64_u16).to_le_bytes());
        bytes[0x3a..0x3c].copy_from_slice(&(64_u16).to_le_bytes());
        bytes[0x3c..0x3e].copy_from_slice(&(1_u16).to_le_bytes());
        bytes
    }

    fn minimal_wasm_bytes() -> Vec<u8> {
        b"\0asm\x01\0\0\0".to_vec()
    }

    fn package_match(module_id: ModuleId, export_specifier: &str) -> PackageMatch {
        PackageMatch {
            module_id,
            package_name: "pkg".to_string(),
            package_version: "1.0.0".to_string(),
            export_specifier: export_specifier.to_string(),
            source_path: format!("pkg@1.0.0/{export_specifier}.js"),
            normalized_source_hash: format!("hash-{}", module_id.0),
            strategy: ModuleMatchStrategy::NormalizedSourceHash,
            function_signature_matches: 0,
            string_anchor_matches: 0,
            external_importable: true,
        }
    }

    fn rejected_package_ownership(
        module_id: ModuleId,
        package_name: &str,
        package_version: &str,
    ) -> PackageAttributionInput {
        let mut attribution = PackageAttributionInput::rejected_source(
            module_id,
            package_name,
            "matched package ownership, but the evidence does not prove a safe single external import",
        );
        attribution.package_version = Some(package_version.to_string());
        attribution
    }

    fn package_match_connection(
        source_path: PathBuf,
        bundled_source: &str,
        package_sources: &[(&str, &str, &str, &str)],
    ) -> Connection {
        fs::write(source_path.as_path(), bundled_source).expect("write source fixture");
        let connection = Connection::open_in_memory().expect("open in-memory database");
        connection
            .execute_batch(
                r"
                CREATE TABLE projects (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL
                );
                CREATE TABLE source_files (
                    id INTEGER PRIMARY KEY,
                    file_path TEXT NOT NULL
                );
                CREATE TABLE project_files (
                    project_id INTEGER NOT NULL,
                    file_id INTEGER NOT NULL
                );
                CREATE TABLE modules (
                    id INTEGER PRIMARY KEY,
                    file_id INTEGER,
                    original_name TEXT NOT NULL,
                    semantic_name TEXT,
                    module_category TEXT,
                    package_name TEXT,
                    package_version TEXT,
                    byte_start INTEGER,
                    byte_end INTEGER
                );
                CREATE TABLE symbols (
                    module_id INTEGER,
                    semantic_name TEXT,
                    export_name TEXT,
                    original_name TEXT,
                    scope_level TEXT
                );
                CREATE TABLE module_dependencies (
                    module_id INTEGER,
                    dependency_id INTEGER
                );
                CREATE TABLE package_source_cache (
                    package_name TEXT NOT NULL,
                    package_version TEXT NOT NULL,
                    entry_path TEXT NOT NULL,
                    source_content TEXT NOT NULL,
                    content_hash TEXT NOT NULL,
                    external_importable INTEGER NOT NULL DEFAULT 1,
                    external_import_policy_version INTEGER NOT NULL DEFAULT 0,
                    export_specifier TEXT NOT NULL DEFAULT '',
                    fetched_at TEXT NOT NULL,
                    expires_at TEXT NOT NULL,
                    PRIMARY KEY (package_name, package_version, entry_path)
                );
                CREATE TABLE package_attributions (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    module_id INTEGER NOT NULL,
                    module_original_name TEXT NOT NULL,
                    package_name TEXT NOT NULL,
                    package_version TEXT NOT NULL,
                    package_subpath TEXT,
                    resolved_file TEXT,
                    export_specifier TEXT,
                    emission_mode TEXT NOT NULL,
                    status TEXT NOT NULL,
                    evidence_json TEXT,
                    rejection_reason TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    UNIQUE (module_id)
                );
                ",
            )
            .expect("create schema");
        connection
            .execute("INSERT INTO projects (id, name) VALUES (1, 'fixture')", [])
            .expect("insert project");
        connection
            .execute(
                "INSERT INTO source_files (id, file_path) VALUES (1, ?1)",
                [source_path.to_string_lossy().as_ref()],
            )
            .expect("insert source file");
        connection
            .execute(
                "INSERT INTO project_files (project_id, file_id) VALUES (1, 1)",
                [],
            )
            .expect("insert project file");
        connection
            .execute(
                r"
                INSERT INTO modules
                    (id, file_id, original_name, semantic_name, module_category,
                     package_name, package_version, byte_start, byte_end)
                VALUES (10, 1, 'm10', 'pkg/add.js', 'package', 'pkg', NULL, 0, ?1)
                ",
                [bundled_source.len() as i64],
            )
            .expect("insert module");
        for (package_name, package_version, entry_path, source) in package_sources {
            connection
                .execute(
                    r"
                    INSERT INTO package_source_cache
                        (package_name, package_version, entry_path, source_content,
                         content_hash, external_importable, external_import_policy_version,
                         export_specifier, fetched_at, expires_at)
                    VALUES (?1, ?2, ?3, ?4, 'hash', 1, ?5, ?6, 'now', 'later')
                    ",
                    params![
                        package_name,
                        package_version,
                        entry_path,
                        source,
                        PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION,
                        package_export_specifier(package_name, entry_path),
                    ],
                )
                .expect("insert package source");
        }
        connection
    }

    fn package_attribution_count(connection: &Connection) -> i64 {
        connection
            .query_row("SELECT COUNT(*) FROM package_attributions", [], |row| {
                row.get(0)
            })
            .expect("count package attributions")
    }

    fn package_surface_count(connection: &Connection) -> i64 {
        connection
            .query_row("SELECT COUNT(*) FROM package_surfaces", [], |row| {
                row.get(0)
            })
            .expect("count package surfaces")
    }

    fn create_source_surface_schema(connection: &Connection) {
        connection
            .execute_batch(
                r"
                CREATE TABLE projects (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL
                );
                CREATE TABLE source_files (
                    id INTEGER PRIMARY KEY,
                    file_path TEXT NOT NULL
                );
                CREATE TABLE project_files (
                    project_id INTEGER NOT NULL,
                    file_id INTEGER NOT NULL
                );
                CREATE TABLE modules (
                    id INTEGER PRIMARY KEY,
                    file_id INTEGER,
                    original_name TEXT NOT NULL,
                    semantic_name TEXT,
                    module_category TEXT,
                    package_name TEXT,
                    package_version TEXT,
                    byte_start INTEGER,
                    byte_end INTEGER
                );
                CREATE TABLE symbols (
                    module_id INTEGER,
                    semantic_name TEXT,
                    export_name TEXT,
                    original_name TEXT,
                    scope_level TEXT
                );
                CREATE TABLE module_dependencies (
                    module_id INTEGER,
                    dependency_id INTEGER
                );
                CREATE TABLE package_source_cache (
                    package_name TEXT NOT NULL,
                    package_version TEXT NOT NULL,
                    entry_path TEXT NOT NULL,
                    source_content TEXT NOT NULL,
                    content_hash TEXT NOT NULL,
                    external_importable INTEGER NOT NULL DEFAULT 1,
                    external_import_policy_version INTEGER NOT NULL DEFAULT 0,
                    export_specifier TEXT NOT NULL DEFAULT '',
                    fetched_at TEXT NOT NULL,
                    expires_at TEXT NOT NULL,
                    PRIMARY KEY (package_name, package_version, entry_path)
                );
                CREATE TABLE package_attributions (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    module_id INTEGER NOT NULL,
                    module_original_name TEXT NOT NULL,
                    package_name TEXT NOT NULL,
                    package_version TEXT NOT NULL,
                    package_subpath TEXT,
                    resolved_file TEXT,
                    export_specifier TEXT,
                    emission_mode TEXT NOT NULL,
                    status TEXT NOT NULL,
                    evidence_json TEXT,
                    rejection_reason TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    UNIQUE (module_id)
                );
                ",
            )
            .expect("create source surface schema");
    }

    fn insert_source_surface_rows(connection: &Connection, source_path: &str) {
        let app_source = fs::read_to_string(source_path).expect("source fixture should exist");
        connection
            .execute("INSERT INTO projects (id, name) VALUES (1, 'fixture')", [])
            .expect("insert project");
        connection
            .execute(
                "INSERT INTO source_files (id, file_path) VALUES (1, ?1)",
                [source_path],
            )
            .expect("insert source file");
        connection
            .execute(
                "INSERT INTO project_files (project_id, file_id) VALUES (1, 1)",
                [],
            )
            .expect("insert project file");
        connection
            .execute(
                r"
                INSERT INTO modules
                    (id, file_id, original_name, semantic_name, module_category,
                     package_name, package_version, byte_start, byte_end)
                VALUES (1, 1, 'entry', 'entry', 'application', NULL, NULL, 0, ?1)
                ",
                [app_source.len() as i64],
            )
            .expect("insert app module");
        connection
            .execute(
                r"
                INSERT INTO package_source_cache
                    (package_name, package_version, entry_path, source_content,
                     content_hash, external_importable, external_import_policy_version,
                     export_specifier, fetched_at, expires_at)
                VALUES
                    ('undici', '2.2.1', 'wrapper.mjs', 'export default {};',
                     'hash', 1, ?1, 'undici/wrapper.mjs', 'now', 'later')
                ",
                [PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION],
            )
            .expect("insert package source");
    }

    fn create_match_generate_schema(connection: &Connection) {
        connection
            .execute_batch(
                r"
                CREATE TABLE projects (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL
                );
                CREATE TABLE source_files (
                    id INTEGER PRIMARY KEY,
                    file_path TEXT NOT NULL
                );
                CREATE TABLE project_files (
                    project_id INTEGER NOT NULL,
                    file_id INTEGER NOT NULL
                );
                CREATE TABLE modules (
                    id INTEGER PRIMARY KEY,
                    file_id INTEGER,
                    original_name TEXT NOT NULL,
                    semantic_name TEXT,
                    module_category TEXT,
                    package_name TEXT,
                    package_version TEXT,
                    byte_start INTEGER,
                    byte_end INTEGER
                );
                CREATE TABLE symbols (
                    module_id INTEGER,
                    semantic_name TEXT,
                    export_name TEXT,
                    original_name TEXT,
                    scope_level TEXT
                );
                CREATE TABLE module_dependencies (
                    module_id INTEGER,
                    dependency_id INTEGER
                );
                CREATE TABLE package_source_cache (
                    package_name TEXT NOT NULL,
                    package_version TEXT NOT NULL,
                    entry_path TEXT NOT NULL,
                    source_content TEXT NOT NULL,
                    content_hash TEXT NOT NULL,
                    external_importable INTEGER NOT NULL DEFAULT 1,
                    external_import_policy_version INTEGER NOT NULL DEFAULT 0,
                    export_specifier TEXT NOT NULL DEFAULT '',
                    fetched_at TEXT NOT NULL,
                    expires_at TEXT NOT NULL,
                    PRIMARY KEY (package_name, package_version, entry_path)
                );
                CREATE TABLE package_attributions (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    module_id INTEGER NOT NULL,
                    module_original_name TEXT NOT NULL,
                    package_name TEXT NOT NULL,
                    package_version TEXT NOT NULL,
                    package_subpath TEXT,
                    resolved_file TEXT,
                    export_specifier TEXT,
                    emission_mode TEXT NOT NULL,
                    status TEXT NOT NULL,
                    evidence_json TEXT,
                    rejection_reason TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    UNIQUE (module_id)
                );
                ",
            )
            .expect("create match/generate schema");
    }

    fn insert_match_generate_rows(
        connection: &Connection,
        app_source_path: &str,
        package_slice_path: &str,
        app_len: i64,
        package_len: i64,
    ) {
        connection
            .execute("INSERT INTO projects (id, name) VALUES (1, 'fixture')", [])
            .expect("insert project");
        connection
            .execute(
                "INSERT INTO source_files (id, file_path) VALUES (1, ?1), (2, ?2)",
                params![app_source_path, package_slice_path],
            )
            .expect("insert source files");
        connection
            .execute(
                "INSERT INTO project_files (project_id, file_id) VALUES (1, 1), (1, 2)",
                [],
            )
            .expect("insert project files");
        connection
            .execute(
                r"
                INSERT INTO modules
                    (id, file_id, original_name, semantic_name, module_category,
                     package_name, package_version, byte_start, byte_end)
                VALUES
                    (1, 1, 'entry', 'entry', 'application', NULL, NULL, 0, ?1),
                    (10, 2, 'pkg_add', 'pkg/add', 'package', 'pkg', NULL, 0, ?2)
                ",
                params![app_len, package_len],
            )
            .expect("insert modules");
        connection
            .execute(
                r"
                INSERT INTO package_source_cache
                    (package_name, package_version, entry_path, source_content,
                     content_hash, external_importable, external_import_policy_version,
                     export_specifier, fetched_at, expires_at)
                VALUES
                    ('pkg', '1.2.3', 'add', 'export function add(a, b) { return a + b; }',
                     'hash', 1, ?1, 'pkg/add', 'now', 'later')
                ",
                [PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION],
            )
            .expect("insert package source");
    }
}
