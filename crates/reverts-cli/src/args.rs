//! Clap-backed argument structs for every reverts-cli subcommand.
//!
//! The public structs are the typed contract consumed by command runners. Each
//! `parse` method keeps the test-friendly API used by the crate while delegating
//! option parsing to `clap`.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::Args;

use crate::errors::CliError;
use crate::help;

#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct MatchPackagesArgs {
    #[arg(long)]
    pub input: PathBuf,
    #[arg(long, value_parser = parse_project_id)]
    pub project_id: u32,
    #[arg(long)]
    pub apply: bool,
    #[arg(long = "package-name", value_parser = parse_package_name)]
    pub package_names: Vec<String>,
    #[arg(long = "package-source-root")]
    pub package_source_roots: Vec<PathBuf>,
    #[arg(long)]
    pub materialize_package_sources: bool,
}

impl MatchPackagesArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        parse_subcommand_args(args, help::MATCH_PACKAGES_COMMAND)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct MatchPackagesReportArgs {
    #[arg(long)]
    pub input: PathBuf,
    #[arg(long)]
    pub all_projects: bool,
    #[arg(long, value_parser = parse_limit)]
    pub limit: Option<u32>,
    #[arg(long)]
    pub newest: bool,
    #[arg(long = "package-name", value_parser = parse_package_name)]
    pub package_names: Vec<String>,
    #[arg(long = "package-source-root")]
    pub package_source_roots: Vec<PathBuf>,
    #[arg(long)]
    pub materialize_package_sources: bool,
}

impl MatchPackagesReportArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let parsed = parse_subcommand_args(args, help::MATCH_PACKAGES_REPORT_COMMAND)?;
        validate_match_packages_report_args(parsed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct PackageVersionDiagnosticsArgs {
    #[arg(long)]
    pub input: PathBuf,
    #[arg(long, value_parser = parse_project_id)]
    pub project_id: u32,
    #[arg(long = "package-name", value_parser = parse_package_name)]
    pub package_names: Vec<String>,
    #[arg(long = "package-source-root")]
    pub package_source_roots: Vec<PathBuf>,
    #[arg(long)]
    pub materialize_package_sources: bool,
    #[arg(long, default_value_t = 5, value_parser = parse_limit)]
    pub top: u32,
}

impl PackageVersionDiagnosticsArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        parse_subcommand_args(args, help::PACKAGE_VERSION_DIAGNOSTICS_COMMAND)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct PackageCacheArgs {
    #[arg(long)]
    pub input: PathBuf,
    #[arg(long)]
    pub apply: bool,
}

impl PackageCacheArgs {
    pub fn parse(
        args: impl IntoIterator<Item = String>,
        command: &'static str,
    ) -> Result<Self, CliError> {
        parse_subcommand_args(args, command)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct PackageExternalizationHintsArgs {
    #[arg(long)]
    pub input: PathBuf,
    #[arg(long = "package-name", value_parser = parse_package_name)]
    pub package_names: Vec<String>,
    #[arg(long, value_parser = parse_limit)]
    pub limit: Option<u32>,
    #[arg(long)]
    pub apply: bool,
}

impl PackageExternalizationHintsArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        parse_subcommand_args(args, help::PACKAGE_EXTERNALIZATION_HINTS_COMMAND)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct ExtractAssetsArgs {
    #[arg(long)]
    pub input: PathBuf,
    #[arg(long, value_parser = parse_project_id)]
    pub project_id: u32,
    #[arg(long)]
    pub apply: bool,
    #[arg(long = "asset-root")]
    pub asset_roots: Vec<PathBuf>,
}

impl ExtractAssetsArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        parse_subcommand_args(args, help::EXTRACT_ASSETS_COMMAND)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct ImportUnpackedArgs {
    #[arg(long)]
    pub input: PathBuf,
    #[arg(long)]
    pub manifest: PathBuf,
    #[arg(long = "project-name")]
    pub project_name: String,
    #[arg(long = "output-db")]
    pub output_db: PathBuf,
    #[arg(long = "ignore-native-assets")]
    pub ignore_native_assets: bool,
    #[arg(long = "max-source-bytes", value_parser = parse_byte_limit)]
    pub max_source_bytes: Option<u64>,
    #[arg(long = "bundle-source-bytes", value_parser = parse_byte_limit)]
    pub bundle_source_bytes: Option<u64>,
}

impl ImportUnpackedArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        parse_subcommand_args(args, help::IMPORT_UNPACKED_COMMAND)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct RuntimeInventoryArgs {
    #[arg(long, value_parser = parse_project_id)]
    pub project_id: Option<u32>,
    #[arg(long)]
    pub all_projects: bool,
    #[arg(long, value_parser = parse_limit)]
    pub limit: Option<u32>,
    #[arg(long)]
    pub newest: bool,
    #[arg(long, value_parser = parse_byte_limit)]
    pub max_source_bytes: Option<u64>,
    #[arg(long)]
    pub setter_blockers: bool,
    #[arg(long)]
    pub runtime_attribution: bool,
    #[arg(long)]
    pub package_source_blockers: bool,
    #[arg(long)]
    pub input: PathBuf,
}

impl RuntimeInventoryArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let parsed = parse_subcommand_args(args, help::RUNTIME_INVENTORY_COMMAND)?;
        validate_runtime_inventory_args(parsed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamingProgressTier {
    PublicSurface,
    Declarations,
    Full,
}

fn parse_target_level(value: &str) -> Result<NamingProgressTier, String> {
    match value {
        "public-surface" => Ok(NamingProgressTier::PublicSurface),
        "declarations" => Ok(NamingProgressTier::Declarations),
        "full" => Ok(NamingProgressTier::Full),
        other => Err(format!(
            "invalid --target-level {other}; expected public-surface | declarations | full"
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct NamingProgressArgs {
    #[arg(long)]
    pub input: PathBuf,
    #[arg(long, value_parser = parse_project_id)]
    pub project_id: u32,
    #[arg(long = "target-level", value_parser = parse_target_level, default_value = "full")]
    pub target_level: NamingProgressTier,
}

impl NamingProgressArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        parse_subcommand_args(args, help::NAMING_PROGRESS_COMMAND)
    }
}

pub(crate) fn parse_subcommand_args<T>(
    args: impl IntoIterator<Item = String>,
    command: &'static str,
) -> Result<T, CliError>
where
    T: Args + clap::FromArgMatches,
{
    let mut args = args.into_iter().collect::<Vec<_>>();
    if args.first().is_some_and(|argument| argument == command) {
        args.remove(0);
    }
    parse_args_with_name::<T>(command, args)
}

pub(crate) fn parse_args_with_name<T>(
    command: &'static str,
    args: Vec<String>,
) -> Result<T, CliError>
where
    T: Args + clap::FromArgMatches,
{
    let command = T::augment_args(clap::Command::new(command).disable_help_flag(true));
    let argv = std::iter::once(OsString::from(command.get_name().to_string()))
        .chain(args.into_iter().map(OsString::from))
        .collect::<Vec<_>>();
    let matches = command
        .try_get_matches_from(argv)
        .map_err(clap_error_to_cli)?;
    T::from_arg_matches(&matches).map_err(|error| CliError::UnknownArgument(error.to_string()))
}

pub(crate) fn parse_project_id(value: &str) -> Result<u32, String> {
    parse_positive_u32(value, "project id")
}

pub(crate) fn parse_limit(value: &str) -> Result<u32, String> {
    parse_positive_u32(value, "limit")
}

pub(crate) fn parse_byte_limit(value: &str) -> Result<u64, String> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_error| format!("invalid byte limit {value}"))?;
    if parsed == 0 {
        Err(format!("invalid byte limit {value}"))
    } else {
        Ok(parsed)
    }
}

fn parse_positive_u32(value: &str, label: &str) -> Result<u32, String> {
    let parsed = value
        .parse::<u32>()
        .map_err(|_error| format!("invalid {label} {value}"))?;
    if parsed == 0 {
        Err(format!("invalid {label} {value}"))
    } else {
        Ok(parsed)
    }
}

fn parse_package_name(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        Err(format!("invalid package name {value}"))
    } else {
        Ok(value.to_string())
    }
}

pub(crate) fn clap_error_to_cli(error: clap::Error) -> CliError {
    let message = error.to_string();
    if let Some(value) = parse_prefixed_error_value(message.as_str(), "invalid project id ") {
        return CliError::InvalidProjectId(value);
    }
    if let Some(value) = parse_prefixed_error_value(message.as_str(), "invalid limit ") {
        return CliError::InvalidLimit(value);
    }
    if let Some(value) = parse_prefixed_error_value(message.as_str(), "invalid byte limit ") {
        return CliError::InvalidByteLimit(value);
    }
    if let Some(value) = parse_prefixed_error_value(message.as_str(), "invalid package name ") {
        return CliError::InvalidPackageName(value);
    }
    match error.kind() {
        clap::error::ErrorKind::UnknownArgument => CliError::UnknownArgument(message),
        clap::error::ErrorKind::MissingRequiredArgument => {
            CliError::MissingArgument(missing_required_argument(message.as_str()))
        }
        _ => CliError::UnknownArgument(message),
    }
}

fn parse_prefixed_error_value(message: &str, prefix: &str) -> Option<String> {
    let start = message.find(prefix)? + prefix.len();
    let rest = &message[start..];
    let end = rest.find(['\n', '\'', '"']).unwrap_or(rest.len());
    let value = rest[..end].trim().trim_end_matches('.').to_string();
    if value.is_empty() { None } else { Some(value) }
}

fn missing_required_argument(message: &str) -> &'static str {
    for argument in [
        "--input",
        "--project-id",
        "--output",
        "--all-projects",
        "--list | --set | --clear | --batch",
    ] {
        if message.contains(argument) {
            return argument;
        }
    }
    "required argument"
}

fn validate_match_packages_report_args(
    args: MatchPackagesReportArgs,
) -> Result<MatchPackagesReportArgs, CliError> {
    if !args.all_projects {
        Err(CliError::MissingArgument("--all-projects"))
    } else {
        Ok(args)
    }
}

fn validate_runtime_inventory_args(
    args: RuntimeInventoryArgs,
) -> Result<RuntimeInventoryArgs, CliError> {
    match (args.project_id, args.all_projects) {
        (Some(_), true) => Err(CliError::UnknownArgument("--all-projects".to_string())),
        (None, false) => Err(CliError::MissingArgument("--project-id")),
        _ => Ok(args),
    }
}

#[cfg(test)]
mod tests {
    use super::{NamingProgressArgs, NamingProgressTier};

    #[test]
    fn naming_progress_parses_target_level() {
        let args = NamingProgressArgs::parse(
            [
                "--input",
                "in.db",
                "--project-id",
                "1",
                "--target-level",
                "public-surface",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .expect("should parse");
        assert_eq!(args.project_id, 1);
        assert_eq!(args.target_level, NamingProgressTier::PublicSurface);
    }

    #[test]
    fn naming_progress_defaults_target_level_to_full() {
        let args = NamingProgressArgs::parse(
            ["--input", "in.db", "--project-id", "1"]
                .into_iter()
                .map(str::to_string),
        )
        .expect("should parse");
        assert_eq!(args.target_level, NamingProgressTier::Full);
    }
}
