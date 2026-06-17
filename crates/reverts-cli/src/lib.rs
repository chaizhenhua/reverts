mod args;
mod commands;
mod errors;
mod help;
mod persistence;
mod pkg_sources;
#[cfg(test)]
mod tests;

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
use persistence::repository::{MatchPackagePersistence, SqliteMatchPackagePersistence};
pub(crate) use persistence::source_cache::{
    PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION, package_source_cache_entry_path,
    package_source_from_row, persist_package_source_cache, stale_package_source_cache_versions,
};
pub(crate) use pkg_sources::filtering::{
    dedup_package_sources, filter_package_sources_to_best_build_variants,
    filter_package_sources_to_relevant_path_hints,
};
pub(crate) use pkg_sources::version_resolution::{
    PackageVersionResolutionEvidence, materialize_package_sources_from_hints,
    package_version_resolution_evidence, package_versions_by_module,
    resolve_package_version_hints_to_available_sources,
};
#[cfg(test)]
pub(crate) use pkg_sources::version_resolution::{
    PackageVersionResolutionPlan, best_matching_package_version_by_binary_search,
    network_package_version_resolution_hints, package_version_hints_for_materialization,
    parse_npm_versions_json, resolve_package_version_hint_from_versions,
    stale_cache_version_hints_for_materialization,
};

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use reverts_input::sqlite::load_project_rows_from_connection;
use reverts_input::{InputRows, ModuleDependencyTarget, PackageEmissionMode};
use reverts_ir::hash::fnv1a_hex as stable_hash;
use reverts_ir::{ModuleKind, is_valid_package_name, split_bare_specifier};
use reverts_observe::{AuditFinding, AuditReport, FindingCode};
use reverts_package::is_node_builtin;
use reverts_package_matcher::{
    PackageModuleSourceQuality, PackageSource, clean_package_semantic_path_hint,
    has_accepted_external_attribution, is_exact_package_version_hint, match_packages_with_pipeline,
    package_import_names_from_sources, package_module_source_quality, package_source_entry_path,
    package_source_exported_members, package_source_normalized_hash,
    package_source_public_export_proofs, strip_source_extension,
};
use reverts_pipeline::prepare_input_rows_for_pipeline;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params_from_iter};
use semver::Version;

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
        let mut persistence = SqliteMatchPackagePersistence::new(connection);
        let outcome = persistence.persist_match_package_outputs(
            &rows,
            &synthetic_modules,
            &report,
            &package_names,
            &package_version_resolutions,
            &function_attributions,
        )?;
        (
            outcome.written_attributions,
            outcome.written_surfaces,
            outcome.written_function_attributions,
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

pub(crate) fn sqlite_placeholders(count: usize) -> String {
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
