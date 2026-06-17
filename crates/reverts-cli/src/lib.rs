mod args;
mod commands;
mod errors;
mod help;
mod package_match_usecase;
mod package_source_workflow;
mod persistence;
mod pkg_sources;
mod project_writer;
#[cfg(test)]
mod tests;

pub use args::{
    ExtractAssetsArgs, MatchPackagesArgs, MatchPackagesReportArgs, PackageCacheArgs,
    PackageExternalizationHintsArgs, PackageVersionDiagnosticsArgs, RuntimeInventoryArgs,
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
pub use commands::symbol_names::SymbolNamesArgs;
pub use errors::{
    CliError, CliRunError, ExtractAssetsError, MatchPackagesError, RuntimeInventoryError,
    SymbolNamesError,
};
pub use help::{HelpTopic, help_text, version_text};

use args::{next_path, next_value, parse_project_id};
pub(crate) use package_source_workflow::{
    clean_package_entry_path, enrich_package_modules_from_source_units,
    externalization_hint_candidates_from_cache,
    filter_package_sources_to_referenced_package_versions, hint_export_specifier_matches_package,
    load_package_sources, package_export_specifier, package_module_source_quality_counts,
    package_source_load_scope, remove_package_attributions_for_revalidation,
};
#[cfg(test)]
pub(crate) use package_source_workflow::{
    package_graph_component_scope, promote_package_sources_with_externalization_hints,
};
use persistence::externalization_hints::{
    PACKAGE_EXTERNALIZATION_HINT_POLICY_VERSION, PackageExternalizationHint,
    persist_package_externalization_hints,
};
pub(crate) use persistence::source_cache::PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION;
#[cfg(test)]
pub(crate) use persistence::source_cache::{
    persist_package_source_cache, stale_package_source_cache_versions,
};
#[cfg(test)]
pub(crate) use pkg_sources::filtering::{
    filter_package_sources_to_best_build_variants, filter_package_sources_to_relevant_path_hints,
};
pub(crate) use pkg_sources::version_resolution::{
    PackageVersionResolutionEvidence, package_version_resolution_evidence,
    package_versions_by_module, resolve_package_version_hints_to_available_sources,
};
#[cfg(test)]
pub(crate) use pkg_sources::version_resolution::{
    PackageVersionResolutionPlan, best_matching_package_version_by_binary_search,
    network_package_version_resolution_hints, package_version_hints_for_materialization,
    parse_npm_versions_json, resolve_package_version_hint_from_versions,
    stale_cache_version_hints_for_materialization,
};

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use reverts_ir::hash::fnv1a_hex as stable_hash;
use reverts_observe::AuditReport;
use reverts_package_matcher::{
    PackageSource, is_exact_package_version_hint, package_source_exported_members,
    package_source_normalized_hash, package_source_public_export_proofs,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliCommand {
    Help(HelpTopic),
    Version,
    GenerateProjectV2(GenerateProjectV2Args),
    MatchPackages(MatchPackagesArgs),
    MatchPackagesReport(MatchPackagesReportArgs),
    PackageVersionDiagnostics(PackageVersionDiagnosticsArgs),
    PackageCacheAudit(PackageCacheArgs),
    PackageCachePruneStale(PackageCacheArgs),
    PackageExternalizationHints(PackageExternalizationHintsArgs),
    ExtractAssets(ExtractAssetsArgs),
    RuntimeInventory(RuntimeInventoryArgs),
    SymbolNames(SymbolNamesArgs),
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
                        HelpTopic::PackageVersionDiagnostics => {
                            Ok(Self::PackageVersionDiagnostics(
                                PackageVersionDiagnosticsArgs::parse(args)?,
                            ))
                        }
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
                        HelpTopic::SymbolNames => {
                            Ok(Self::SymbolNames(SymbolNamesArgs::parse(args)?))
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
        CliCommand::PackageVersionDiagnostics(args) => {
            commands::package_version_diagnostics::run(args)
        }
        CliCommand::PackageCacheAudit(args) => commands::package_cache::run_audit(args),
        CliCommand::PackageCachePruneStale(args) => commands::package_cache::run_prune_stale(args),
        CliCommand::PackageExternalizationHints(args) => {
            commands::package_cache::run_externalization_hints(args)
        }
        CliCommand::ExtractAssets(args) => commands::extract_assets::run(args),
        CliCommand::RuntimeInventory(args) => commands::runtime_inventory::run(args),
        CliCommand::SymbolNames(args) => commands::symbol_names::run(args),
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
    package_match_usecase::match_packages_from_connection(connection, args)
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
