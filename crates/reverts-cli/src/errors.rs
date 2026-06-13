//! Error types surfaced by the CLI. Split out so that adding a new error
//! variant to a subcommand does not require touching the rest of the CLI
//! module tree.

use std::error::Error;
use std::fmt;
use std::io;
use std::path::PathBuf;

use reverts_input::sqlite::SqliteInputError;
use reverts_ir::ModuleId;
use reverts_pipeline::PipelineError;

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

#[derive(Debug)]
pub enum MatchPackagesError {
    OpenDatabase {
        path: PathBuf,
        source: rusqlite::Error,
    },
    ConfigureDatabase(rusqlite::Error),
    LoadInput(SqliteInputError),
    QueryPackageSources(rusqlite::Error),
    ReadPackageSourceRoot {
        path: PathBuf,
        source: io::Error,
    },
    WriteAttribution(rusqlite::Error),
    WritePackageSurface(rusqlite::Error),
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
    InvalidPackageSurface {
        export_specifier: String,
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
            Self::ReadPackageSourceRoot { path, source } => {
                write!(
                    formatter,
                    "failed to read package source root {}: {source}",
                    path.display()
                )
            }
            Self::WriteAttribution(source) => {
                write!(formatter, "failed to write package attribution: {source}")
            }
            Self::WritePackageSurface(source) => {
                write!(formatter, "failed to write package surface: {source}")
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
            Self::InvalidPackageSurface {
                export_specifier,
                message,
            } => {
                write!(
                    formatter,
                    "invalid package surface {export_specifier}: {message}"
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
            | Self::WriteAttribution(source)
            | Self::WritePackageSurface(source) => Some(source),
            Self::ReadPackageSourceRoot { source, .. } => Some(source),
            Self::LoadInput(source) => Some(source),
            Self::MissingTable(_)
            | Self::MissingMatchEvidence { .. }
            | Self::MissingModuleForAttribution { .. }
            | Self::InvalidAttribution { .. }
            | Self::InvalidPackageSurface { .. } => None,
        }
    }
}

#[derive(Debug)]
pub enum ExtractAssetsError {
    OpenDatabase {
        path: PathBuf,
        source: rusqlite::Error,
    },
    ConfigureDatabase(rusqlite::Error),
    LoadInput(SqliteInputError),
    CannotInferAssetRoot {
        project_id: u32,
    },
    ReadAsset {
        path: PathBuf,
        source: io::Error,
    },
    WriteMaterializedAsset {
        path: PathBuf,
        source: io::Error,
    },
    InvalidAssetPath {
        logical_path: String,
    },
    AmbiguousAsset {
        logical_path: String,
        candidates: Vec<String>,
    },
    WriteAsset(rusqlite::Error),
}

impl fmt::Display for ExtractAssetsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenDatabase { path, source } => {
                write!(formatter, "failed to open {}: {source}", path.display())
            }
            Self::ConfigureDatabase(source) => {
                write!(formatter, "failed to configure SQLite: {source}")
            }
            Self::LoadInput(source) => write!(formatter, "{source}"),
            Self::CannotInferAssetRoot { project_id } => {
                write!(
                    formatter,
                    "cannot infer asset root for project {project_id} without source files"
                )
            }
            Self::ReadAsset { path, source } => {
                write!(
                    formatter,
                    "failed to read asset {}: {source}",
                    path.display()
                )
            }
            Self::WriteMaterializedAsset { path, source } => {
                write!(
                    formatter,
                    "failed to materialize asset {}: {source}",
                    path.display()
                )
            }
            Self::InvalidAssetPath { logical_path } => {
                write!(formatter, "invalid asset path {logical_path}")
            }
            Self::AmbiguousAsset {
                logical_path,
                candidates,
            } => {
                write!(
                    formatter,
                    "asset {logical_path} matched multiple roots: {}",
                    candidates.join(", ")
                )
            }
            Self::WriteAsset(source) => {
                write!(formatter, "failed to write project asset: {source}")
            }
        }
    }
}

impl Error for ExtractAssetsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::OpenDatabase { source, .. }
            | Self::ConfigureDatabase(source)
            | Self::WriteAsset(source) => Some(source),
            Self::ReadAsset { source, .. } | Self::WriteMaterializedAsset { source, .. } => {
                Some(source)
            }
            Self::LoadInput(source) => Some(source),
            Self::CannotInferAssetRoot { .. }
            | Self::InvalidAssetPath { .. }
            | Self::AmbiguousAsset { .. } => None,
        }
    }
}

#[derive(Debug)]
pub enum CliRunError {
    Args(CliError),
    LoadInput(SqliteInputError),
    Pipeline(PipelineError),
    MatchPackages(MatchPackagesError),
    ExtractAssets(ExtractAssetsError),
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
            Self::ExtractAssets(source) => write!(formatter, "{source}"),
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
            Self::ExtractAssets(source) => Some(source),
            Self::WriteOutput { source, .. } => Some(source),
            Self::AuditRejected(_) | Self::UnsafeOutputPath(_) => None,
        }
    }
}
