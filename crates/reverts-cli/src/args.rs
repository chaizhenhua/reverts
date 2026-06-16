//! Argument structs + parsers for every reverts-cli subcommand.
//!
//! Lives in its own module so the giant `lib.rs` does not have to be the
//! authoritative home for both behavior and argument syntax. Helpers
//! (`next_path`, `next_value`, `parse_project_id`, …) are `pub(crate)`
//! because the command runners back in `lib.rs` also use them.

use std::path::PathBuf;

use crate::errors::CliError;
use crate::help;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchPackagesArgs {
    pub input: PathBuf,
    pub project_id: u32,
    pub apply: bool,
    pub package_names: Vec<String>,
    pub package_source_roots: Vec<PathBuf>,
    pub materialize_package_sources: bool,
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

// Shared parser helpers used by every Args::parse above and by other CLI
// modules that consume positional flags.

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
