use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use reverts_input::sqlite::{
    SqliteInputError, load_project_bundle_from_sqlite, load_project_rows_from_connection,
};
use reverts_input::{
    InputRows, PackageAttributionInput, PackageAttributionStatus, PackageEmissionMode,
};
use reverts_ir::{ModuleId, ModuleKind};
use reverts_observe::AuditReport;
use reverts_package_matcher::{
    ExactPackageMatcher, PackageMatch, PackageMatchReport, PackageSource,
};
use reverts_pipeline::{EmittedFile, PipelineError, generate_project_from_input};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params, params_from_iter};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerateProjectV2Args {
    pub input: PathBuf,
    pub output: PathBuf,
    pub project_id: u32,
}

impl GenerateProjectV2Args {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let mut input = None;
        let mut output = None;
        let mut project_id = None;
        let mut args = args.into_iter().collect::<Vec<_>>();
        if args
            .first()
            .is_some_and(|argument| argument == "generate-project-v2")
        {
            args.remove(0);
        }
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--input" => input = Some(next_path(&mut args, "--input")?),
                "--output" => output = Some(next_path(&mut args, "--output")?),
                "--project-id" => {
                    project_id = Some(parse_project_id(next_value(&mut args, "--project-id")?)?);
                }
                other => return Err(CliError::UnknownArgument(other.to_string())),
            }
        }

        Ok(Self {
            input: input.ok_or(CliError::MissingArgument("--input"))?,
            output: output.ok_or(CliError::MissingArgument("--output"))?,
            project_id: project_id.ok_or(CliError::MissingArgument("--project-id"))?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchPackagesArgs {
    pub input: PathBuf,
    pub project_id: u32,
    pub apply: bool,
    pub package_names: Vec<String>,
}

impl MatchPackagesArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let mut input = None;
        let mut project_id = None;
        let mut apply = false;
        let mut package_names = Vec::new();
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
                other => return Err(CliError::UnknownArgument(other.to_string())),
            }
        }

        Ok(Self {
            input: input.ok_or(CliError::MissingArgument("--input"))?,
            project_id: project_id.ok_or(CliError::MissingArgument("--project-id"))?,
            apply,
            package_names,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliCommand {
    GenerateProjectV2(GenerateProjectV2Args),
    MatchPackages(MatchPackagesArgs),
}

impl CliCommand {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let args = args.into_iter().collect::<Vec<_>>();
        match args.first().map(String::as_str) {
            Some("generate-project-v2") => {
                Ok(Self::GenerateProjectV2(GenerateProjectV2Args::parse(args)?))
            }
            Some("match-packages") => Ok(Self::MatchPackages(MatchPackagesArgs::parse(args)?)),
            Some(argument) if argument.starts_with("--") => {
                Ok(Self::GenerateProjectV2(GenerateProjectV2Args::parse(args)?))
            }
            Some(command) => Err(CliError::UnknownCommand(command.to_string())),
            None => Err(CliError::MissingCommand),
        }
    }
}

fn next_path(
    args: &mut impl Iterator<Item = String>,
    flag: &'static str,
) -> Result<PathBuf, CliError> {
    args.next()
        .map(PathBuf::from)
        .ok_or(CliError::MissingArgument(flag))
}

fn next_value(
    args: &mut impl Iterator<Item = String>,
    flag: &'static str,
) -> Result<String, CliError> {
    args.next().ok_or(CliError::MissingArgument(flag))
}

fn parse_project_id(value: String) -> Result<u32, CliError> {
    let parsed = value
        .parse::<u32>()
        .map_err(|_error| CliError::InvalidProjectId(value.clone()))?;
    if parsed == 0 {
        Err(CliError::InvalidProjectId(value))
    } else {
        Ok(parsed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    MissingCommand,
    MissingArgument(&'static str),
    InvalidProjectId(String),
    InvalidPackageName(String),
    UnknownCommand(String),
    UnknownArgument(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCommand => write!(formatter, "missing command"),
            Self::MissingArgument(argument) => write!(formatter, "missing argument {argument}"),
            Self::InvalidProjectId(value) => write!(formatter, "invalid project id {value}"),
            Self::InvalidPackageName(value) => write!(formatter, "invalid package name {value}"),
            Self::UnknownCommand(command) => write!(formatter, "unknown command {command}"),
            Self::UnknownArgument(argument) => write!(formatter, "unknown argument {argument}"),
        }
    }
}

impl Error for CliError {}

pub fn run(args: impl IntoIterator<Item = String>) -> Result<(), CliRunError> {
    match CliCommand::parse(args).map_err(CliRunError::Args)? {
        CliCommand::GenerateProjectV2(args) => run_generate_project(args),
        CliCommand::MatchPackages(args) => run_match_packages(args),
    }
}

fn run_generate_project(args: GenerateProjectV2Args) -> Result<(), CliRunError> {
    let input = load_project_bundle_from_sqlite(&args.input, args.project_id)
        .map_err(CliRunError::LoadInput)?;
    let run = generate_project_from_input(input).map_err(CliRunError::Pipeline)?;

    if !run.audit.is_clean() {
        return Err(CliRunError::AuditRejected(format_audit_findings(
            &run.audit,
        )));
    }

    let written = write_emitted_project(&run.project.files, &args.output)?;
    println!(
        "generated project {} into {} with {written} files",
        args.project_id,
        args.output.display()
    );
    Ok(())
}

fn run_match_packages(args: MatchPackagesArgs) -> Result<(), CliRunError> {
    let outcome = match_packages_from_sqlite(&args).map_err(CliRunError::MatchPackages)?;
    println!(
        "matched packages for project {} from {} cached source(s): {} accepted, {} written, {} audit finding(s)",
        outcome.project_id,
        outcome.loaded_package_sources,
        outcome.matched_modules,
        outcome.written_attributions,
        outcome.audit.findings().len()
    );
    if !outcome.audit.is_clean() {
        println!("{}", format_audit_findings(&outcome.audit));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchPackagesOutcome {
    pub project_id: u32,
    pub loaded_package_modules: usize,
    pub loaded_package_sources: usize,
    pub matched_modules: usize,
    pub written_attributions: usize,
    pub audit: AuditReport,
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
    let rows = load_project_rows_from_connection(connection, args.project_id)
        .map_err(MatchPackagesError::LoadInput)?;
    let package_names = package_source_filter(&rows, &args.package_names);
    let package_sources = load_package_sources(connection, &package_names)?;
    let report = ExactPackageMatcher.match_rows(&rows, &package_sources);
    let written_attributions = if args.apply {
        persist_package_attributions(connection, &rows, &report)?
    } else {
        0
    };

    Ok(MatchPackagesOutcome {
        project_id: args.project_id,
        loaded_package_modules: rows
            .modules
            .iter()
            .filter(|module| module.kind == ModuleKind::Package)
            .count(),
        loaded_package_sources: package_sources.len(),
        matched_modules: report.matches.len(),
        written_attributions,
        audit: report.audit,
    })
}

fn package_source_filter(rows: &InputRows, requested_package_names: &[String]) -> BTreeSet<String> {
    if !requested_package_names.is_empty() {
        return requested_package_names.iter().cloned().collect();
    }

    rows.modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
        .filter(|module| !has_accepted_external_attribution(rows, module.id))
        .filter_map(|module| module.package_name.clone())
        .collect()
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
) -> Result<Vec<PackageSource>, MatchPackagesError> {
    if !sqlite_table_exists(connection, "package_source_cache")
        .map_err(MatchPackagesError::QueryPackageSources)?
    {
        return Err(MatchPackagesError::MissingTable("package_source_cache"));
    }

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
    let package_sources = if package_names.is_empty() {
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
    };
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

fn persist_package_attributions(
    connection: &mut Connection,
    rows: &InputRows,
    report: &PackageMatchReport,
) -> Result<usize, MatchPackagesError> {
    if !sqlite_table_exists(connection, "package_attributions")
        .map_err(MatchPackagesError::WriteAttribution)?
    {
        return Err(MatchPackagesError::MissingTable("package_attributions"));
    }

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

    transaction
        .commit()
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(written)
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

fn write_emitted_project(files: &[EmittedFile], output: &Path) -> Result<usize, CliRunError> {
    fs::create_dir_all(output).map_err(|source| CliRunError::WriteOutput {
        path: output.to_path_buf(),
        source,
    })?;

    for file in files {
        let path = checked_output_path(output, file.path.as_str())?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| CliRunError::WriteOutput {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        fs::write(&path, file.source.as_bytes())
            .map_err(|source| CliRunError::WriteOutput { path, source })?;
    }

    Ok(files.len())
}

fn checked_output_path(output: &Path, relative: &str) -> Result<PathBuf, CliRunError> {
    let relative = Path::new(relative);
    if relative.is_absolute() {
        return Err(CliRunError::UnsafeOutputPath(relative.to_path_buf()));
    }

    let mut path = output.to_path_buf();
    for component in relative.components() {
        match component {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(CliRunError::UnsafeOutputPath(relative.to_path_buf()));
            }
        }
    }
    Ok(path)
}

fn format_audit_findings(audit: &AuditReport) -> String {
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

#[derive(Debug)]
pub enum MatchPackagesError {
    OpenDatabase {
        path: PathBuf,
        source: rusqlite::Error,
    },
    ConfigureDatabase(rusqlite::Error),
    LoadInput(SqliteInputError),
    QueryPackageSources(rusqlite::Error),
    WriteAttribution(rusqlite::Error),
    MissingTable(&'static str),
    MissingMatchEvidence {
        module_id: ModuleId,
    },
    MissingModuleForAttribution {
        module_id: ModuleId,
    },
    InvalidAttribution {
        module_id: ModuleId,
        message: String,
    },
}

impl fmt::Display for MatchPackagesError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenDatabase { path, source } => {
                write!(formatter, "failed to open {}: {source}", path.display())
            }
            Self::ConfigureDatabase(source) => {
                write!(formatter, "failed to configure SQLite: {source}")
            }
            Self::LoadInput(source) => write!(formatter, "{source}"),
            Self::QueryPackageSources(source) => {
                write!(formatter, "failed to load package source cache: {source}")
            }
            Self::WriteAttribution(source) => {
                write!(formatter, "failed to write package attribution: {source}")
            }
            Self::MissingTable(table) => {
                write!(formatter, "required SQLite table is missing: {table}")
            }
            Self::MissingMatchEvidence { module_id } => {
                write!(
                    formatter,
                    "package attribution for module {} has no match evidence",
                    module_id.0
                )
            }
            Self::MissingModuleForAttribution { module_id } => {
                write!(
                    formatter,
                    "package attribution references unknown module {}",
                    module_id.0
                )
            }
            Self::InvalidAttribution { module_id, message } => {
                write!(
                    formatter,
                    "invalid package attribution for module {}: {message}",
                    module_id.0
                )
            }
        }
    }
}

impl Error for MatchPackagesError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::OpenDatabase { source, .. }
            | Self::ConfigureDatabase(source)
            | Self::QueryPackageSources(source)
            | Self::WriteAttribution(source) => Some(source),
            Self::LoadInput(source) => Some(source),
            Self::MissingTable(_)
            | Self::MissingMatchEvidence { .. }
            | Self::MissingModuleForAttribution { .. }
            | Self::InvalidAttribution { .. } => None,
        }
    }
}

#[derive(Debug)]
pub enum CliRunError {
    Args(CliError),
    LoadInput(SqliteInputError),
    Pipeline(PipelineError),
    MatchPackages(MatchPackagesError),
    AuditRejected(String),
    UnsafeOutputPath(PathBuf),
    WriteOutput { path: PathBuf, source: io::Error },
}

impl fmt::Display for CliRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Args(source) => write!(formatter, "{source}"),
            Self::LoadInput(source) => write!(formatter, "{source}"),
            Self::Pipeline(source) => write!(formatter, "{source}"),
            Self::MatchPackages(source) => write!(formatter, "{source}"),
            Self::AuditRejected(summary) => {
                write!(
                    formatter,
                    "generated project was rejected by audit:\n{summary}"
                )
            }
            Self::UnsafeOutputPath(path) => {
                write!(
                    formatter,
                    "emitted file path is not safe: {}",
                    path.display()
                )
            }
            Self::WriteOutput { path, source } => {
                write!(formatter, "failed to write {}: {source}", path.display())
            }
        }
    }
}

impl Error for CliRunError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Args(source) => Some(source),
            Self::LoadInput(source) => Some(source),
            Self::Pipeline(source) => Some(source),
            Self::MatchPackages(source) => Some(source),
            Self::WriteOutput { source, .. } => Some(source),
            Self::AuditRejected(_) | Self::UnsafeOutputPath(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use reverts_observe::FindingCode;
    use rusqlite::{Connection, params};

    use super::{
        CliCommand, CliError, GenerateProjectV2Args, MatchPackagesArgs, checked_output_path,
        match_packages_from_connection,
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
            "--apply".to_string(),
        ])
        .expect("args should parse");

        assert_eq!(args.input, PathBuf::from("input.db"));
        assert_eq!(args.project_id, 13495);
        assert_eq!(args.package_names, vec!["pkg"]);
        assert!(args.apply);

        let old_command = CliCommand::parse(["match-packages-v2".to_string()]);
        assert!(
            matches!(old_command, Err(CliError::UnknownCommand(command)) if command == "match-packages-v2")
        );
    }

    #[test]
    fn output_paths_cannot_escape_output_directory() {
        let error = checked_output_path(PathBuf::from("out").as_path(), "../escape.ts");

        assert!(error.is_err());
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
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.is_clean());
        assert_eq!(outcome.loaded_package_modules, 1);
        assert_eq!(outcome.loaded_package_sources, 1);
        assert_eq!(outcome.matched_modules, 1);
        assert_eq!(outcome.written_attributions, 0);
        assert_eq!(package_attribution_count(&connection), 0);
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
        assert_eq!(outcome.written_attributions, 1);
        assert_eq!(package_version, "1.2.3");
        assert!(evidence.contains("exact_normalized_source_binary_search"));
    }

    #[test]
    fn ambiguous_package_versions_are_not_written() {
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
        };

        let outcome =
            match_packages_from_connection(&mut connection, &args).expect("match should run");

        assert!(outcome.audit.has(FindingCode::AmbiguousPackageMatch));
        assert_eq!(outcome.matched_modules, 0);
        assert_eq!(outcome.written_attributions, 0);
        assert_eq!(package_attribution_count(&connection), 0);
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
}
