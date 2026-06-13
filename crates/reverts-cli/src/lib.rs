mod commands;
mod errors;
mod help;

pub use commands::generate_project::GenerateProjectV2Args;
pub use errors::{CliError, CliRunError, ExtractAssetsError, MatchPackagesError};
pub use help::{HelpTopic, help_text, version_text};

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use reverts_graph::FunctionExtractor;
use reverts_input::sqlite::load_project_rows_from_connection;
use reverts_input::{
    AssetKind, InputRows, ModuleInput, PackageAttributionInput, PackageAttributionStatus,
    PackageEmissionMode, SourceFileInput,
};
use reverts_ir::{ModuleId, ModuleKind};
use reverts_observe::AuditReport;
use reverts_package_matcher::{
    BestVersionMatch, CascadeMatchReport, ModuleMatchStrategy, PackageMatch, PackageSource,
    VersionedPackageMatchReport, VersionedPackageMatcher, match_with_cascade,
    package_import_names_from_sources,
};
use reverts_pipeline::{AssetReference, collect_required_asset_references_from_rows};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params, params_from_iter};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchPackagesArgs {
    pub input: PathBuf,
    pub project_id: u32,
    pub apply: bool,
    pub package_names: Vec<String>,
    pub package_source_roots: Vec<PathBuf>,
}

impl MatchPackagesArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let mut input = None;
        let mut project_id = None;
        let mut apply = false;
        let mut package_names = Vec::new();
        let mut package_source_roots = Vec::new();
        let mut args = args.into_iter().collect::<Vec<_>>();
        if args
            .first()
            .is_some_and(|argument| argument == "match-packages")
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
                other => return Err(CliError::UnknownArgument(other.to_string())),
            }
        }

        Ok(Self {
            input: input.ok_or(CliError::MissingArgument("--input"))?,
            project_id: project_id.ok_or(CliError::MissingArgument("--project-id"))?,
            apply,
            package_names,
            package_source_roots,
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
            .is_some_and(|argument| argument == "extract-assets")
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
pub enum CliCommand {
    Help(HelpTopic),
    Version,
    GenerateProjectV2(GenerateProjectV2Args),
    MatchPackages(MatchPackagesArgs),
    ExtractAssets(ExtractAssetsArgs),
}

impl CliCommand {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let args = args.into_iter().collect::<Vec<_>>();
        match args.first().map(String::as_str) {
            Some(argument) if is_help_flag(argument) => parse_top_level_help(args.as_slice()),
            Some(argument) if is_version_flag(argument) => parse_version(args.as_slice()),
            Some("help") => parse_help_command(args.as_slice()),
            Some("version") => parse_version(args.as_slice()),
            Some("generate-project-v2") => {
                if is_command_help(args.as_slice()) {
                    return Ok(Self::Help(HelpTopic::GenerateProjectV2));
                }
                Ok(Self::GenerateProjectV2(GenerateProjectV2Args::parse(args)?))
            }
            Some("match-packages") => {
                if is_command_help(args.as_slice()) {
                    return Ok(Self::Help(HelpTopic::MatchPackages));
                }
                Ok(Self::MatchPackages(MatchPackagesArgs::parse(args)?))
            }
            Some("extract-assets") => {
                if is_command_help(args.as_slice()) {
                    return Ok(Self::Help(HelpTopic::ExtractAssets));
                }
                Ok(Self::ExtractAssets(ExtractAssetsArgs::parse(args)?))
            }
            Some(argument) if argument.starts_with("--") => {
                Ok(Self::GenerateProjectV2(GenerateProjectV2Args::parse(args)?))
            }
            Some(command) => Err(CliError::UnknownCommand(command.to_string())),
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
    match command {
        "generate-project-v2" => Ok(HelpTopic::GenerateProjectV2),
        "match-packages" => Ok(HelpTopic::MatchPackages),
        "extract-assets" => Ok(HelpTopic::ExtractAssets),
        other => Err(CliError::UnknownCommand(other.to_string())),
    }
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
        CliCommand::ExtractAssets(args) => run_extract_assets(args),
    }
}

fn run_match_packages(args: MatchPackagesArgs) -> Result<(), CliRunError> {
    let outcome = match_packages_from_sqlite(&args).map_err(CliRunError::MatchPackages)?;
    println!(
        "matched packages for project {} from {} cached source(s): {} module attribution(s), {} package surface(s), {} attribution(s) written, {} surface(s) written, {} cascade attribution(s) ({} written), {} audit finding(s)",
        outcome.project_id,
        outcome.loaded_package_sources,
        outcome.matched_modules,
        outcome.matched_package_surfaces,
        outcome.written_attributions,
        outcome.written_surfaces,
        outcome.cascade_attributions,
        outcome.written_cascade_attributions,
        outcome.audit.findings().len()
    );
    if !outcome.audit.is_clean() {
        println!("{}", format_audit_findings(&outcome.audit));
    }
    Ok(())
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchPackagesOutcome {
    pub project_id: u32,
    pub loaded_package_modules: usize,
    pub loaded_package_sources: usize,
    pub matched_modules: usize,
    pub matched_package_surfaces: usize,
    pub written_attributions: usize,
    pub written_surfaces: usize,
    /// Number of attribution rows produced by the cascade-match pipeline
    /// (computed regardless of whether persistence ran).
    pub cascade_attributions: usize,
    /// Number of cascade-match rows actually written to
    /// `package_function_attributions`. Zero when `apply=false`.
    pub written_cascade_attributions: usize,
    pub audit: AuditReport,
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

pub fn match_packages_from_connection(
    connection: &mut Connection,
    args: &MatchPackagesArgs,
) -> Result<MatchPackagesOutcome, MatchPackagesError> {
    let mut rows = load_project_rows_from_connection(connection, args.project_id)
        .map_err(MatchPackagesError::LoadInput)?;

    // Bundle-aware module extraction (Phase α): split recognised bundle
    // wrappers into per-module rows before the matcher sees them.
    let extraction = reverts_bundle::extract(&rows);
    let extraction_audit = extraction.audit.clone();
    // Snapshot new_modules before merge_into consumes the extraction —
    // we need them later to persist synthetic rows into the SQLite
    // modules table so cascade attributions can FK them.
    let synthetic_modules: Vec<reverts_input::ModuleInput> = extraction.new_modules.clone();
    extraction.merge_into(&mut rows);

    let package_names = package_source_filter(&rows, &args.package_names);
    let package_sources =
        load_package_sources(connection, &package_names, &args.package_source_roots)?;
    let mut report = if args.package_names.is_empty() {
        VersionedPackageMatcher::default().match_rows(&rows, &package_sources)
    } else {
        VersionedPackageMatcher::default().match_rows_for_packages(
            &rows,
            &package_sources,
            &package_names,
        )
    };

    // Phase-1: also run the cascade pipeline alongside the legacy matcher.
    let fingerprints_by_module = fingerprints_from_rows(
        &rows,
        (!args.package_names.is_empty()).then_some(&package_names),
    );
    let cascade_report =
        match_with_cascade_scoped_by_module_hints(&rows, &fingerprints_by_module, &package_sources);
    promote_cascade_function_coverage_to_module_attributions(
        &rows,
        &fingerprints_by_module,
        &cascade_report,
        &mut report,
    );

    let (written_attributions, written_surfaces, written_cascade_attributions) = if args.apply {
        // Persist synthetic modules first so the FK from
        // package_attributions.module_id and
        // package_function_attributions.module_id resolves.
        persist_synthetic_modules(connection, &synthetic_modules)?;
        // A few synthetic modules may have been skipped by `INSERT OR
        // IGNORE` due to `UNIQUE(file_id, original_name)` collisions
        // with pre-existing legacy rows. Build the set of module ids
        // that actually live in the SQLite `modules` table now, and
        // filter cascade attributions to only those — the alternative
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
        let cascade_report_filtered = reverts_package_matcher::CascadeMatchReport {
            attributions: cascade_report
                .attributions
                .iter()
                .filter(|a| persisted_ids.contains(&a.module_id))
                .cloned()
                .collect(),
            audit: cascade_report.audit.clone(),
        };
        (
            persist_package_attributions(connection, &rows, &report, &package_names)?,
            persist_package_surfaces(connection, &rows, &report)?,
            persist_cascade_attributions(connection, &rows, &cascade_report_filtered)?,
        )
    } else {
        (0, 0, 0)
    };

    let mut audit = extraction_audit;
    audit.extend(report.audit);
    audit.extend(cascade_report.audit);

    Ok(MatchPackagesOutcome {
        project_id: args.project_id,
        loaded_package_modules: rows
            .modules
            .iter()
            .filter(|module| module.kind == ModuleKind::Package)
            .count(),
        loaded_package_sources: package_sources.len(),
        matched_modules: report.matches.len(),
        matched_package_surfaces: report.surfaces.len(),
        written_attributions,
        written_surfaces,
        cascade_attributions: cascade_report.attributions.len(),
        written_cascade_attributions,
        audit,
    })
}

/// Builds per-module function fingerprints from raw input rows using a default
/// (empty) control-flow graph, mirroring the approach used inside the cascade
/// index builder for package sources.
fn fingerprints_from_rows(
    rows: &InputRows,
    package_filter: Option<&BTreeSet<String>>,
) -> BTreeMap<ModuleId, Vec<reverts_ir::FunctionFingerprint>> {
    let mut out = BTreeMap::new();
    for module in &rows.modules {
        if let Some(package_filter) = package_filter {
            if module.kind != ModuleKind::Package
                || !module
                    .package_name
                    .as_deref()
                    .is_some_and(|package_name| package_filter.contains(package_name))
            {
                continue;
            }
        }
        if let Some(slice) = rows.module_source_slice(module.id) {
            let fps = FunctionExtractor::fingerprint(module.id, slice.source);
            if !fps.is_empty() {
                out.insert(module.id, fps);
            }
        }
    }
    out
}

fn match_with_cascade_scoped_by_module_hints(
    rows: &InputRows,
    fingerprints_by_module: &BTreeMap<ModuleId, Vec<reverts_ir::FunctionFingerprint>>,
    package_sources: &[PackageSource],
) -> CascadeMatchReport {
    let modules_by_id = rows
        .modules
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let mut grouped_fingerprints = BTreeMap::<
        (Option<String>, Option<String>),
        BTreeMap<ModuleId, Vec<reverts_ir::FunctionFingerprint>>,
    >::new();
    for (module_id, fingerprints) in fingerprints_by_module {
        let scope = modules_by_id.get(module_id).and_then(|module| {
            if module.kind != ModuleKind::Package {
                return None;
            }
            let package_name = module.package_name.as_ref()?.trim();
            if package_name.is_empty() {
                return None;
            }
            let package_version = module
                .package_version
                .as_deref()
                .map(str::trim)
                .filter(|version| !version.is_empty())
                .map(ToString::to_string);
            Some((Some(package_name.to_string()), package_version))
        });
        grouped_fingerprints
            .entry(scope.unwrap_or((None, None)))
            .or_default()
            .insert(*module_id, fingerprints.clone());
    }

    let mut merged = CascadeMatchReport {
        attributions: Vec::new(),
        audit: AuditReport::default(),
    };
    for ((package_name, package_version), scoped_fingerprints) in grouped_fingerprints {
        let scoped_sources = package_sources
            .iter()
            .filter(|source| {
                package_name
                    .as_deref()
                    .map_or(true, |name| source.package_name == name)
                    && package_version
                        .as_deref()
                        .map_or(true, |version| source.package_version == version)
            })
            .cloned()
            .collect::<Vec<_>>();
        if scoped_sources.is_empty() {
            continue;
        }
        let report = match_with_cascade(&scoped_fingerprints, &scoped_sources);
        merged.attributions.extend(report.attributions);
        merged.audit.extend(report.audit);
    }
    merged
}

fn promote_cascade_function_coverage_to_module_attributions(
    rows: &InputRows,
    fingerprints_by_module: &BTreeMap<ModuleId, Vec<reverts_ir::FunctionFingerprint>>,
    cascade_report: &CascadeMatchReport,
    report: &mut VersionedPackageMatchReport,
) {
    let already_accepted = report
        .attributions
        .iter()
        .chain(rows.package_attributions.iter())
        .filter(|attribution| {
            attribution.status == PackageAttributionStatus::Accepted
                && attribution.emission_mode == PackageEmissionMode::ExternalImport
        })
        .map(|attribution| attribution.module_id)
        .collect::<BTreeSet<_>>();
    let matched_modules = report
        .matches
        .iter()
        .map(|package_match| package_match.module_id)
        .collect::<BTreeSet<_>>();
    let cascade_attributions_by_module = cascade_report.attributions.iter().fold(
        BTreeMap::<ModuleId, Vec<&PackageAttributionInput>>::new(),
        |mut by_module, attribution| {
            by_module
                .entry(attribution.module_id)
                .or_default()
                .push(attribution);
            by_module
        },
    );

    for module in &rows.modules {
        if module.kind != ModuleKind::Package
            || already_accepted.contains(&module.id)
            || matched_modules.contains(&module.id)
        {
            continue;
        }
        let Some(expected_package_name) = module.package_name.as_deref() else {
            continue;
        };
        let Some(fingerprints) = fingerprints_by_module.get(&module.id) else {
            continue;
        };
        let Some(cascade_attributions) = cascade_attributions_by_module.get(&module.id) else {
            continue;
        };
        if fingerprints.is_empty() || cascade_attributions.len() != fingerprints.len() {
            continue;
        }
        let covered_spans = cascade_attributions
            .iter()
            .filter_map(|attribution| attribution.function_span)
            .collect::<BTreeSet<_>>();
        if covered_spans.len() != fingerprints.len() {
            continue;
        }
        let decisions = cascade_attributions
            .iter()
            .filter_map(|attribution| {
                Some((
                    attribution.package_name.as_str(),
                    attribution.package_version.as_deref()?,
                    attribution.export_specifier.as_deref()?,
                ))
            })
            .collect::<BTreeSet<_>>();
        if decisions.len() != 1 {
            continue;
        }
        let (package_name, package_version, export_specifier) =
            decisions.into_iter().next().expect("one decision");
        if package_name != expected_package_name {
            continue;
        }
        if module
            .package_version
            .as_deref()
            .map(str::trim)
            .filter(|version| !version.is_empty())
            .is_some_and(|expected_version| package_version != expected_version)
        {
            continue;
        }

        let mut attribution = PackageAttributionInput::accepted_external(
            module.id,
            package_name,
            package_version,
            export_specifier,
        );
        if let Some(subpath) = package_subpath_from_export_specifier(package_name, export_specifier)
        {
            attribution = attribution.with_subpath(subpath);
        }
        report.attributions.push(attribution);
        report.matches.push(PackageMatch {
            module_id: module.id,
            package_name: package_name.to_string(),
            package_version: package_version.to_string(),
            export_specifier: export_specifier.to_string(),
            source_path: format!("cascade:{export_specifier}"),
            normalized_source_hash: String::new(),
            strategy: ModuleMatchStrategy::CascadeFunctionCoverage,
            function_signature_matches: fingerprints.len(),
            string_anchor_matches: 0,
            external_importable: true,
        });
    }
}

fn package_subpath_from_export_specifier(
    package_name: &str,
    export_specifier: &str,
) -> Option<String> {
    if export_specifier == package_name {
        return None;
    }
    export_specifier
        .strip_prefix(package_name)
        .and_then(|subpath| subpath.strip_prefix('/'))
        .filter(|subpath| !subpath.trim().is_empty())
        .map(ToString::to_string)
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

fn package_source_filter(rows: &InputRows, requested_package_names: &[String]) -> BTreeSet<String> {
    if !requested_package_names.is_empty() {
        return requested_package_names.iter().cloned().collect();
    }

    let mut package_names = rows
        .modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
        .filter(|module| !has_accepted_external_attribution(rows, module.id))
        .filter_map(|module| module.package_name.clone())
        .collect::<BTreeSet<_>>();
    package_names.extend(package_import_names_from_sources(rows));
    package_names
}

fn has_accepted_external_attribution(rows: &InputRows, module_id: ModuleId) -> bool {
    rows.package_attributions.iter().any(|attribution| {
        attribution.module_id == module_id
            && attribution.status == PackageAttributionStatus::Accepted
            && attribution.emission_mode == PackageEmissionMode::ExternalImport
    })
}

fn load_package_sources(
    connection: &Connection,
    package_names: &BTreeSet<String>,
    package_source_roots: &[PathBuf],
) -> Result<Vec<PackageSource>, MatchPackagesError> {
    let has_package_source_cache = sqlite_table_exists(connection, "package_source_cache")
        .map_err(MatchPackagesError::QueryPackageSources)?;
    if !has_package_source_cache && package_source_roots.is_empty() {
        return Err(MatchPackagesError::MissingTable("package_source_cache"));
    }

    let mut package_sources = if has_package_source_cache {
        let mut sql = String::from(
            r"
            SELECT package_name, package_version, entry_path, source_content
              FROM package_source_cache
             WHERE TRIM(COALESCE(package_name, '')) != ''
               AND TRIM(COALESCE(package_version, '')) != ''
               AND TRIM(COALESCE(entry_path, '')) != ''
               AND TRIM(COALESCE(source_content, '')) != ''
            ",
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
    dedup_package_sources(&mut package_sources);
    Ok(package_sources)
}

fn package_source_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PackageSource> {
    let package_name = row.get::<_, String>(0)?;
    let package_version = row.get::<_, String>(1)?;
    let entry_path = row.get::<_, String>(2)?;
    let source = row.get::<_, String>(3)?;
    let export_specifier = package_export_specifier(package_name.as_str(), entry_path.as_str());
    let source_path = format!("{package_name}@{package_version}/{entry_path}");
    Ok(PackageSource::external(
        package_name,
        package_version,
        export_specifier,
        source_path,
        source,
    ))
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

fn package_dir_candidates(root: &Path, package_name: &str) -> Vec<PathBuf> {
    let package_path = package_name
        .split('/')
        .fold(PathBuf::new(), |path, segment| path.join(segment));
    let mut candidates = Vec::new();
    candidates.push(root.join("node_modules").join(&package_path));
    candidates.push(root.join(&package_path));
    candidates.push(root.to_path_buf());
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
    importable_paths: BTreeMap<String, String>,
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
    let Ok(value) = serde_json::from_str::<serde_json::Value>(content.as_str()) else {
        return Ok(None);
    };
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
        importable_paths: package_importable_paths(value.as_object(), package_name.as_str()),
        name: package_name,
        version: package_version.trim().to_string(),
    }))
}

fn collect_local_package_sources(
    package_dir: &Path,
    metadata: &LocalPackageMetadata,
    sources: &mut Vec<PackageSource>,
) -> Result<(), MatchPackagesError> {
    let mut stack = vec![package_dir.to_path_buf()];
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
            let source = fs::read_to_string(path.as_path()).map_err(|source| {
                MatchPackagesError::ReadPackageSourceRoot {
                    path: path.clone(),
                    source,
                }
            })?;
            let source_path = format!("{}@{}/{}", metadata.name, metadata.version, rel_path);
            if let Some(export_specifier) = metadata.importable_paths.get(rel_path.as_str()) {
                sources.push(PackageSource::external(
                    metadata.name.as_str(),
                    metadata.version.as_str(),
                    export_specifier.as_str(),
                    source_path,
                    source,
                ));
            } else {
                let export_specifier =
                    package_export_specifier(metadata.name.as_str(), rel_path.as_str());
                sources.push(PackageSource::source_only(
                    metadata.name.as_str(),
                    metadata.version.as_str(),
                    export_specifier,
                    source_path,
                    source,
                ));
            }
        }
    }
    Ok(())
}

fn package_importable_paths(
    package_json: Option<&serde_json::Map<String, serde_json::Value>>,
    package_name: &str,
) -> BTreeMap<String, String> {
    let mut importable_paths = BTreeMap::new();
    let Some(package_json) = package_json else {
        return importable_paths;
    };

    if let Some(exports) = package_json.get("exports") {
        collect_exports_importable_paths(exports, package_name, ".", &mut importable_paths);
        return importable_paths;
    }

    for field in ["module", "main", "browser"] {
        if let Some(target) = package_json.get(field).and_then(serde_json::Value::as_str) {
            insert_importable_target(&mut importable_paths, target, package_name);
        }
    }
    insert_importable_target(&mut importable_paths, "index.js", package_name);
    importable_paths
}

fn collect_exports_importable_paths(
    value: &serde_json::Value,
    package_name: &str,
    export_key: &str,
    importable_paths: &mut BTreeMap<String, String>,
) {
    let Some(export_specifier) = export_key_to_specifier(package_name, export_key) else {
        return;
    };
    match value {
        serde_json::Value::String(target) => {
            insert_importable_target(importable_paths, target, export_specifier.as_str());
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_exports_importable_paths(item, package_name, export_key, importable_paths);
            }
        }
        serde_json::Value::Object(object) => {
            if object.keys().any(|key| key == "." || key.starts_with("./")) {
                for (nested_export_key, nested_value) in object {
                    collect_exports_importable_paths(
                        nested_value,
                        package_name,
                        nested_export_key,
                        importable_paths,
                    );
                }
            } else {
                for condition in ["import", "require", "default", "node", "browser"] {
                    if let Some(nested_value) = object.get(condition) {
                        collect_exports_importable_paths(
                            nested_value,
                            package_name,
                            export_key,
                            importable_paths,
                        );
                    }
                }
            }
        }
        _ => {}
    }
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

fn insert_importable_target(
    importable_paths: &mut BTreeMap<String, String>,
    target: &str,
    export_specifier: &str,
) {
    let Some(clean_target) = clean_export_target(target) else {
        return;
    };
    for candidate in importable_target_candidates(clean_target.as_str()) {
        importable_paths
            .entry(candidate)
            .or_insert_with(|| export_specifier.to_string());
    }
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

fn should_descend_package_source_dir(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return true;
    };
    !matches!(
        name.to_ascii_lowercase().as_str(),
        "node_modules" | "test" | "tests" | "__tests__" | "coverage" | "benchmark" | "benchmarks"
    )
}

fn is_local_package_source_candidate(rel_path: &str) -> bool {
    let lower = rel_path.to_ascii_lowercase();
    if lower.ends_with(".d.ts")
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
        Some("js" | "mjs" | "cjs" | "ts" | "tsx")
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
    let mut seen = BTreeSet::new();
    package_sources.retain(|source| {
        seen.insert((
            source.package_name.clone(),
            source.package_version.clone(),
            source.source_path.clone(),
        ))
    });
}

fn persist_package_attributions(
    connection: &mut Connection,
    rows: &InputRows,
    report: &VersionedPackageMatchReport,
    matched_package_names: &BTreeSet<String>,
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
    let modules_by_id = rows
        .modules
        .iter()
        .map(|module| (module.id, module.original_name.as_str()))
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
        let module_original_name = modules_by_id.get(&attribution.module_id).copied().ok_or(
            MatchPackagesError::MissingModuleForAttribution {
                module_id: attribution.module_id,
            },
        )?;
        persist_package_attribution(
            &transaction,
            module_original_name,
            attribution,
            module_match,
        )?;
        written += 1;
    }
    for attribution in &rejected_attributions {
        let module_original_name = modules_by_id.get(&attribution.module_id).copied().ok_or(
            MatchPackagesError::MissingModuleForAttribution {
                module_id: attribution.module_id,
            },
        )?;
        persist_rejected_package_attribution(&transaction, module_original_name, attribution)?;
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

        let reason = decision_reasons
            .get(package_name)
            .map(String::as_str)
            .unwrap_or("package matcher did not produce an accepted attribution for this package");
        rejected.push(PackageAttributionInput::rejected_source(
            module.id,
            package_name,
            reason,
        ));
    }
    Ok(rejected)
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
            "package version search found more than one best version".to_string()
        }
        BestVersionMatch::NoMatch { scores, .. } if scores.is_empty() => {
            "no cached package source was available for this package".to_string()
        }
        BestVersionMatch::NoMatch { .. } => {
            "package version search found no usable evidence".to_string()
        }
        BestVersionMatch::InsufficientEvidence { .. } => {
            "package version evidence did not satisfy the acceptance threshold".to_string()
        }
    }
}

fn persist_package_attribution(
    connection: &Connection,
    module_original_name: &str,
    attribution: &PackageAttributionInput,
    module_match: &PackageMatch,
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
    let evidence = serde_json::json!({
        "matcher": "exact_normalized_source_binary_search",
        "package_name": module_match.package_name,
        "package_version": module_match.package_version,
        "export_specifier": module_match.export_specifier,
        "source_path": module_match.source_path,
        "normalized_source_hash": module_match.normalized_source_hash,
        "match_strategy": module_match.strategy.as_str(),
        "function_signature_matches": module_match.function_signature_matches,
        "string_anchor_matches": module_match.string_anchor_matches,
        "writes_package_version": true,
    })
    .to_string();
    connection
        .execute(
            r"
            INSERT INTO package_attributions
                (module_id, module_original_name, package_name, package_version,
                 package_subpath, resolved_file, export_specifier, emission_mode,
                 status, evidence_json, rejection_reason, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'external_import',
                    'accepted', ?8, NULL, datetime('now'), datetime('now'))
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
            ],
        )
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(())
}

fn persist_rejected_package_attribution(
    connection: &Connection,
    module_original_name: &str,
    attribution: &PackageAttributionInput,
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

    let evidence = serde_json::json!({
        "matcher": "exact_normalized_source_binary_search",
        "package_name": attribution.package_name,
        "status": "rejected",
        "rejection_reason": rejection_reason,
        "writes_package_version": false,
    })
    .to_string();
    connection
        .execute(
            r"
            INSERT INTO package_attributions
                (module_id, module_original_name, package_name, package_version,
                 package_subpath, resolved_file, export_specifier, emission_mode,
                 status, evidence_json, rejection_reason, created_at, updated_at)
            VALUES (?1, ?2, ?3, NULL, NULL, NULL, NULL, 'application_source',
                    'rejected', ?4, ?5, datetime('now'), datetime('now'))
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
                updated_at = datetime('now')
            ",
            params![
                i64::from(attribution.module_id.0),
                module_original_name,
                attribution.package_name.as_str(),
                evidence,
                rejection_reason,
            ],
        )
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(())
}

/// Persist bundle-extraction synthetic modules into the SQLite `modules`
/// table. Required when `apply: true` so that cascade attribution rows
/// can satisfy their `module_id REFERENCES modules(id)` foreign key.
///
/// Uses `INSERT OR IGNORE` to allow idempotent re-runs: if a previous
/// run already wrote a row with the same `(file_id, original_name)`,
/// the duplicate is silently skipped. Synthetic-id collisions across
/// runs are avoided by `reverts_bundle::extract`'s allocator starting
/// past the max real module id at load time.
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

fn persist_cascade_attributions(
    connection: &mut Connection,
    rows: &InputRows,
    report: &CascadeMatchReport,
) -> Result<usize, MatchPackagesError> {
    if report.attributions.is_empty() {
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

    for attribution in &report.attributions {
        let Some(function_span) = attribution.function_span else {
            // Function-level attribution requires a span; cascade only emits
            // rows with `with_function_span(...)`, so this is a programmer
            // error rather than user input — surface it instead of skipping.
            return Err(MatchPackagesError::InvalidAttribution {
                module_id: attribution.module_id,
                message: "cascade attribution missing function_span".to_string(),
            });
        };
        let Some(confidence) = attribution.confidence.as_ref() else {
            return Err(MatchPackagesError::InvalidAttribution {
                module_id: attribution.module_id,
                message: "cascade attribution missing confidence".to_string(),
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
                message: "cascade attribution missing package version".to_string(),
            },
        )?;
        let export_specifier = attribution.export_specifier.as_deref().ok_or(
            MatchPackagesError::InvalidAttribution {
                module_id: attribution.module_id,
                message: "cascade attribution missing export specifier".to_string(),
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

fn collect_sqlite_rows<T>(
    rows: impl Iterator<Item = rusqlite::Result<T>>,
) -> rusqlite::Result<Vec<T>> {
    rows.collect()
}

fn sqlite_placeholders(count: usize) -> String {
    (0..count).map(|_| "?").collect::<Vec<_>>().join(", ")
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
    use std::fs;
    use std::path::PathBuf;

    use reverts_observe::FindingCode;
    use reverts_pipeline::{EmittedAsset, EmittedFile, RuntimeDependency};
    use rusqlite::{Connection, params};

    use super::commands::generate_project::{checked_output_path, write_emitted_project};
    use super::{
        CliCommand, CliError, ExtractAssetsArgs, GenerateProjectV2Args, HelpTopic,
        MatchPackagesArgs, extract_bun_embedded_asset_from_bytes, help_text,
        match_packages_from_connection, run, version_text,
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
        assert!(args.apply);

        let old_command = CliCommand::parse(["match-packages-v2".to_string()]);
        assert!(
            matches!(old_command, Err(CliError::UnknownCommand(command)) if command == "match-packages-v2")
        );
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
        assert!(help_text(HelpTopic::ExtractAssets).contains("--asset-root <DIR-OR-BUN-EXE>"));
        assert!(version_text().starts_with("reverts-cli "));
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
            fs::read_to_string(tempdir.path().join("tsconfig.json"))
                .expect("tsconfig")
                .contains("\"modules/**/*.ts\"")
        );
        assert!(tempdir.path().join("tsconfig.runtime.json").exists());
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
        };
        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");
        assert!(
            outcome.loaded_package_modules >= 1,
            "extraction should have produced at least one package module: {outcome:?}"
        );
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
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean());
        assert_eq!(outcome.loaded_package_modules, 1);
        assert_eq!(outcome.loaded_package_sources, 1);
        assert_eq!(outcome.matched_modules, 1);
        assert_eq!(outcome.matched_package_surfaces, 0);
        assert_eq!(outcome.written_attributions, 0);
        assert_eq!(outcome.written_surfaces, 0);
        assert_eq!(package_attribution_count(&connection), 0);
    }

    #[test]
    fn match_packages_loads_source_only_files_from_package_source_root() {
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
        let args = MatchPackagesArgs {
            input: PathBuf::from("unused.db"),
            project_id: 1,
            apply: true,
            package_names: vec!["pkg".to_string()],
            package_source_roots: vec![tempdir.path().join("project")],
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
        assert_eq!(
            outcome.loaded_package_sources, 1,
            "only lib/add.js should be loaded; tests must be skipped"
        );
        assert_eq!(outcome.matched_modules, 1);
        assert_eq!(
            outcome.written_attributions, 1,
            "source-only ownership matches are persisted as application_source decisions"
        );
        assert_eq!(
            outcome.cascade_attributions, 0,
            "source-only roots must not feed external cascade attribution"
        );
        assert_eq!(outcome.written_cascade_attributions, 0);
        let (status, emission_mode, rejection_reason, package_version): (
            String,
            String,
            String,
            Option<String>,
        ) = connection
            .query_row(
                r"
                SELECT status, emission_mode, rejection_reason, package_version
                  FROM package_attributions
                 WHERE module_id = 10
                ",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("source-only match should leave a rejected attribution");
        assert_eq!(status, "rejected");
        assert_eq!(emission_mode, "application_source");
        assert_eq!(package_version, None);
        assert!(rejection_reason.contains("source-only"));
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
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
        assert_eq!(outcome.loaded_package_sources, 1);
        assert_eq!(outcome.matched_modules, 1);
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
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
        assert_eq!(outcome.loaded_package_sources, 1);
        assert_eq!(outcome.matched_modules, 1);
        assert_eq!(outcome.cascade_attributions, 0);
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
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
        assert_eq!(
            outcome.matched_modules, 1,
            "legacy module-level matcher should be rescued by cascade coverage"
        );
        assert_eq!(outcome.written_attributions, 1);
        assert!(outcome.written_cascade_attributions >= 1);
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
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean(), "{:?}", outcome.audit.findings());
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
    fn match_packages_apply_writes_cascade_function_attribution() {
        // Same fixture as the legacy-matcher test, but the assertion looks at
        // the new `package_function_attributions` table populated by the
        // cascade pipeline. The cascade should produce an Exact-tier match
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
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean());
        assert!(
            outcome.written_cascade_attributions >= 1,
            "expected cascade attribution to be persisted, outcome={:?}",
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
    fn match_packages_dry_run_does_not_persist_cascade_attributions() {
        // With apply=false, the cascade pipeline still RUNS (the diagnostic
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
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.cascade_attributions >= 1, "cascade should compute");
        assert_eq!(outcome.written_cascade_attributions, 0);
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
    fn ambiguous_package_versions_write_rejected_attribution() {
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
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");
        let (status, rejection_reason, package_version, emission_mode): (
            String,
            String,
            Option<String>,
            String,
        ) = connection
            .query_row(
                r"
                SELECT status, rejection_reason, package_version, emission_mode
                  FROM package_attributions
                 WHERE module_id = 10
                ",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("rejected attribution should be written");
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

        assert!(outcome.audit.has(FindingCode::AmbiguousPackageMatch));
        assert_eq!(outcome.matched_modules, 0);
        assert_eq!(outcome.matched_package_surfaces, 0);
        assert_eq!(outcome.written_attributions, 1);
        assert_eq!(outcome.written_surfaces, 0);
        assert_eq!(package_attribution_count(&connection), 1);
        assert_eq!(status, "rejected");
        assert!(rejection_reason.contains("more than one best version"));
        assert_eq!(package_version, None);
        assert_eq!(emission_mode, "application_source");
        assert_eq!(package_version_not_null, 0);
    }

    #[test]
    fn match_packages_apply_replaces_proposed_rows_with_rejected_decisions() {
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
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");
        let (status, package_version, rejection_reason): (String, Option<String>, String) =
            connection
                .query_row(
                    r"
                    SELECT status, package_version, rejection_reason
                      FROM package_attributions
                     WHERE module_id = 10
                    ",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .expect("proposed row should be replaced");

        assert_eq!(outcome.matched_modules, 0);
        assert_eq!(outcome.written_attributions, 1);
        assert_eq!(package_attribution_count(&connection), 1);
        assert_eq!(status, "rejected");
        assert_eq!(package_version, None);
        assert!(rejection_reason.contains("no cached package source"));
        reverts_input::sqlite::load_project_bundle_from_connection(&connection, 1)
            .expect("rejected attribution should satisfy generation input contract");
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
                         content_hash, fetched_at, expires_at)
                    VALUES (?1, ?2, ?3, ?4, 'hash', 'now', 'later')
                    ",
                    params![package_name, package_version, entry_path, source],
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
                     content_hash, fetched_at, expires_at)
                VALUES
                    ('undici', '2.2.1', 'wrapper.mjs', 'export default {};',
                     'hash', 'now', 'later')
                ",
                [],
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
                     content_hash, fetched_at, expires_at)
                VALUES
                    ('pkg', '1.2.3', 'add', 'export function add(a, b) { return a + b; }',
                     'hash', 'now', 'later')
                ",
                [],
            )
            .expect("insert package source");
    }
}
