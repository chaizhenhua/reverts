mod args;
mod commands;
mod errors;
mod help;
mod persistence;
mod pkg_sources;

pub use args::{
    ExtractAssetsArgs, MatchPackagesArgs, MatchPackagesReportArgs, PackageCacheArgs,
    PackageExternalizationHintsArgs, RuntimeInventoryArgs,
};
pub use commands::extract_assets::{
    ExtractAssetsOutcome, extract_assets_from_connection, extract_assets_from_sqlite,
};
pub use commands::generate_project::GenerateProjectV2Args;
pub use commands::package_cache::{PackageCacheAuditOutcome, package_cache_audit_from_sqlite};
pub use commands::runtime_inventory::{
    RuntimeInventoryCounts, RuntimeInventoryOutcome, RuntimeInventoryProject,
    RuntimeLineAttributionBucket, RuntimeLineAttributionItem, RuntimeLineAttributionReport,
    runtime_inventory_from_sqlite,
};
pub use errors::{
    CliError, CliRunError, ExtractAssetsError, MatchPackagesError, RuntimeInventoryError,
};
pub use help::{HelpTopic, help_text, version_text};

use args::{next_path, next_value, parse_project_id};
use persistence::externalization_hints::{
    PACKAGE_EXTERNALIZATION_HINT_POLICY_VERSION, PackageExternalizationHint,
    persist_package_externalization_hints,
};

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use reverts_input::sqlite::load_project_rows_from_connection;
use reverts_input::{InputRows, ModuleDependencyTarget, ModuleInput, PackageEmissionMode};
use reverts_ir::hash::fnv1a_hex as stable_hash;
use reverts_ir::{ModuleId, ModuleKind, is_valid_package_name, split_bare_specifier};
use reverts_js::normalize_source_for_pipeline;
use reverts_observe::{AuditFinding, AuditReport, FindingCode};
use reverts_package::is_node_builtin;
use reverts_package_matcher::{
    PackageModuleSourceQuality, PackageSource, clean_package_semantic_path_hint,
    has_accepted_external_attribution, is_exact_package_version_hint, match_packages_with_pipeline,
    normalize_hint_text, package_import_names_from_sources, package_module_source_quality,
    package_source_entry_path, package_source_exported_members, package_source_normalized_hash,
    package_source_public_export_proofs, package_source_semantic_surface_hint_score,
    strip_source_extension,
};
use reverts_pipeline::prepare_input_rows_for_pipeline;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params, params_from_iter};
use semver::{BuildMetadata, Comparator, Op, Version, VersionReq};

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
        CliCommand::MatchPackages(args) => commands::match_packages::run(args),
        CliCommand::MatchPackagesReport(args) => commands::match_packages::run_report(args),
        CliCommand::PackageCacheAudit(args) => commands::package_cache::run_audit(args),
        CliCommand::PackageCachePruneStale(args) => commands::package_cache::run_prune_stale(args),
        CliCommand::PackageExternalizationHints(args) => {
            commands::package_cache::run_externalization_hints(args)
        }
        CliCommand::ExtractAssets(args) => commands::extract_assets::run(args),
        CliCommand::RuntimeInventory(args) => commands::runtime_inventory::run(args),
    }
}

pub(crate) fn pct(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        (numerator as f64 * 100.0) / denominator as f64
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
        persistence::attributions::filter_unsafe_interpackage_external_attributions(
            &rows,
            &mut report,
        );
    let function_attributions = pipeline_report.function_attributions;
    let function_ownership_matches = pipeline_report.function_ownership_matches;

    let (written_attributions, written_surfaces, written_function_attributions) = if args.apply {
        // Persist synthetic modules first so the FK from
        // package_attributions.module_id and
        // package_function_attributions.module_id resolves.
        persistence::synthetic_modules::persist_synthetic_modules(connection, &synthetic_modules)?;
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
            persistence::attributions::persist_package_attributions(
                connection,
                &rows,
                &report,
                &package_names,
                &package_version_resolutions,
            )?,
            persistence::package_surfaces::persist_package_surfaces(connection, &rows, &report)?,
            persistence::function_attributions::persist_function_attributions(
                connection,
                &rows,
                &persistable_function_attributions,
            )?,
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
    let source_elimination = persistence::attributions::package_source_elimination_stats_for_report(
        &rows,
        &report,
        loaded_package_modules,
    );
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

pub(crate) const PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION: i64 = 4;

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

pub(crate) fn package_export_specifier(package_name: &str, entry_path: &str) -> String {
    let clean_path = clean_package_entry_path(entry_path);
    if clean_path.is_empty() || clean_path == "." {
        package_name.to_string()
    } else {
        format!("{package_name}/{clean_path}")
    }
}

pub(crate) fn clean_package_entry_path(entry_path: &str) -> String {
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
            for package_dir in
                pkg_sources::package_dir_candidates(root.as_path(), package_name.as_str())
            {
                let Some(metadata) = pkg_sources::local_package_metadata(package_dir.as_path())?
                else {
                    continue;
                };
                if metadata.name != *package_name {
                    continue;
                }
                pkg_sources::collect_local_package_sources(
                    package_dir.as_path(),
                    &metadata,
                    &mut sources,
                )?;
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
    for package_dir in pkg_sources::package_dir_candidates(temp_root, package_name) {
        let Some(metadata) = pkg_sources::local_package_metadata(package_dir.as_path())? else {
            continue;
        };
        if metadata.name == package_name && metadata.version == package_version {
            pkg_sources::collect_local_package_sources(package_dir.as_path(), &metadata, sources)?;
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

const PACKAGE_SOURCE_PATH_HINT_FILTER_MIN_SOURCES: usize = 256;

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
pub(crate) struct PackageVersionResolutionEvidence {
    pub(crate) requested_version: Option<String>,
    pub(crate) resolved_version: String,
    pub(crate) reason: &'static str,
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

pub(crate) fn sqlite_table_exists(connection: &Connection, table: &str) -> rusqlite::Result<bool> {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_row| Ok(()),
        )
        .optional()
        .map(|value| value.is_some())
}

pub(crate) fn sqlite_table_has_column(
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

pub(crate) fn collect_sqlite_rows<T>(
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
    use super::commands::runtime_inventory::{
        RuntimeSourceSpanOwner, runtime_emitted_setter_blockers_from_files,
        runtime_inventory_counts_from_files, runtime_inventory_project_selections,
        runtime_line_attribution_from_files, runtime_module_owner_label,
        runtime_original_name_owners_by_binding, runtime_source_span_owner_label_for_range,
    };
    use super::persistence::attributions::{
        externalization_chain_proofs, filter_unsafe_interpackage_external_attributions,
        package_source_elimination_stats_for_report, source_eliminated_package_modules_for_report,
    };
    use super::pkg_sources::{
        collect_local_package_sources, json_package_source_module, local_package_metadata,
    };
    use super::{
        CliCommand, CliError, ExtractAssetsArgs, GenerateProjectV2Args, HelpTopic,
        MatchPackagesArgs, MatchPackagesError, PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION,
        PackageExternalizationHintsArgs, PackageVersionResolutionPlan, RuntimeInventoryArgs,
        best_matching_package_version_by_binary_search, dedup_audit_report,
        filter_package_sources_to_best_build_variants,
        filter_package_sources_to_relevant_path_hints, help_text, load_package_sources,
        match_packages_from_connection, network_package_version_resolution_hints,
        package_export_specifier, package_externalization_hints_from_connection,
        package_graph_component_scope, package_version_hints_for_materialization,
        package_version_resolution_evidence, package_versions_by_module, parse_npm_versions_json,
        persist_package_source_cache, promote_package_sources_with_externalization_hints,
        remove_package_attributions_for_revalidation,
        resolve_package_version_hints_to_available_sources, run, stable_hash,
        stale_cache_version_hints_for_materialization, stale_package_source_cache_versions,
        version_text,
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

        let extracted = super::commands::extract_assets::extract_bun_embedded_asset_from_bytes(
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
