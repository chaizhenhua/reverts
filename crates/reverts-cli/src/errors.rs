//! Error types surfaced by the CLI. Split out so that adding a new error
//! variant to a subcommand does not require touching the rest of the CLI
//! module tree.

use std::error::Error;
use std::fmt;
use std::io;
use std::path::PathBuf;

use reverts_input::sqlite::SqliteInputError;
use reverts_ir::ModuleId;
use reverts_js::JsError;
use reverts_pipeline::PipelineError;

pub use crate::commands::import_unpacked::ImportUnpackedError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    MissingCommand,
    MissingArgument(&'static str),
    InvalidProjectId(String),
    InvalidLimit(String),
    InvalidByteLimit(String),
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
            Self::InvalidLimit(value) => write!(formatter, "invalid limit {value}"),
            Self::InvalidByteLimit(value) => write!(formatter, "invalid byte limit {value}"),
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
    InvalidPackageMetadata {
        path: PathBuf,
        source: serde_json::Error,
    },
    MaterializePackageSource {
        package_name: String,
        package_version: String,
        message: String,
    },
    RegistryRequest {
        url: String,
        message: String,
    },
    ParsePackument {
        package_name: String,
        message: String,
    },
    PackageSourceIntegrity {
        package_name: String,
        package_version: String,
        message: String,
    },
    ExtractPackageSource {
        package_name: String,
        package_version: String,
        message: String,
    },
    ResolveCacheDir {
        message: String,
    },
    InvalidPackageSourceVersion {
        package_name: String,
        package_version: String,
        source_path: String,
    },
    NormalizePackageSource {
        package_name: String,
        package_version: Option<String>,
        source_path: String,
        source: JsError,
    },
    WritePackageSourceCache(rusqlite::Error),
    WritePackageExternalizationHints(rusqlite::Error),
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
            Self::InvalidPackageMetadata { path, source } => {
                write!(
                    formatter,
                    "failed to parse package metadata {}: {source}",
                    path.display()
                )
            }
            Self::MaterializePackageSource {
                package_name,
                package_version,
                message,
            } => {
                write!(
                    formatter,
                    "failed to materialize {package_name}@{package_version}: {message}"
                )
            }
            Self::RegistryRequest { url, message } => {
                write!(formatter, "registry request to {url} failed: {message}")
            }
            Self::ParsePackument {
                package_name,
                message,
            } => write!(
                formatter,
                "failed to parse packument for {package_name}: {message}"
            ),
            Self::PackageSourceIntegrity {
                package_name,
                package_version,
                message,
            } => write!(
                formatter,
                "integrity check failed for {package_name}@{package_version}: {message}"
            ),
            Self::ExtractPackageSource {
                package_name,
                package_version,
                message,
            } => write!(
                formatter,
                "failed to extract {package_name}@{package_version}: {message}"
            ),
            Self::ResolveCacheDir { message } => {
                write!(formatter, "failed to resolve package cache dir: {message}")
            }
            Self::InvalidPackageSourceVersion {
                package_name,
                package_version,
                source_path,
            } => {
                write!(
                    formatter,
                    "invalid package source version {package_name}@{package_version} {source_path}"
                )
            }
            Self::NormalizePackageSource {
                package_name,
                package_version,
                source_path,
                source,
            } => {
                if let Some(package_version) = package_version {
                    write!(
                        formatter,
                        "failed to normalize package source {package_name}@{package_version} {source_path}: {source}"
                    )
                } else {
                    write!(
                        formatter,
                        "failed to normalize package source {package_name} {source_path}: {source}"
                    )
                }
            }
            Self::WriteAttribution(source) => {
                write!(formatter, "failed to write package attribution: {source}")
            }
            Self::WritePackageSourceCache(source) => {
                write!(formatter, "failed to write package source cache: {source}")
            }
            Self::WritePackageExternalizationHints(source) => {
                write!(
                    formatter,
                    "failed to write package externalization hints: {source}"
                )
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
            | Self::WritePackageSourceCache(source)
            | Self::WritePackageExternalizationHints(source)
            | Self::WriteAttribution(source)
            | Self::WritePackageSurface(source) => Some(source),
            Self::NormalizePackageSource { source, .. } => Some(source),
            Self::ReadPackageSourceRoot { source, .. } => Some(source),
            Self::InvalidPackageMetadata { source, .. } => Some(source),
            Self::LoadInput(source) => Some(source),
            Self::MissingTable(_)
            | Self::MaterializePackageSource { .. }
            | Self::RegistryRequest { .. }
            | Self::ParsePackument { .. }
            | Self::PackageSourceIntegrity { .. }
            | Self::ExtractPackageSource { .. }
            | Self::ResolveCacheDir { .. }
            | Self::InvalidPackageSourceVersion { .. }
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
pub enum RuntimeInventoryError {
    OpenDatabase {
        path: PathBuf,
        source: rusqlite::Error,
    },
    QueryProjects(rusqlite::Error),
    LoadInput(SqliteInputError),
    Pipeline(PipelineError),
}

impl fmt::Display for RuntimeInventoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenDatabase { path, source } => {
                write!(formatter, "failed to open {}: {source}", path.display())
            }
            Self::QueryProjects(source) => {
                write!(formatter, "failed to query project ids: {source}")
            }
            Self::LoadInput(source) => write!(formatter, "{source}"),
            Self::Pipeline(source) => write!(formatter, "{source}"),
        }
    }
}

impl Error for RuntimeInventoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::OpenDatabase { source, .. } | Self::QueryProjects(source) => Some(source),
            Self::LoadInput(source) => Some(source),
            Self::Pipeline(source) => Some(source),
        }
    }
}

#[derive(Debug)]
pub enum NamingProgressError {
    LoadInput(SqliteInputError),
    Pipeline(PipelineError),
}

impl fmt::Display for NamingProgressError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LoadInput(source) => write!(formatter, "{source}"),
            Self::Pipeline(source) => write!(formatter, "{source}"),
        }
    }
}

impl Error for NamingProgressError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::LoadInput(source) => Some(source),
            Self::Pipeline(source) => Some(source),
        }
    }
}

#[derive(Debug)]
pub enum SymbolNamesError {
    OpenDatabase {
        path: PathBuf,
        source: rusqlite::Error,
    },
    ConfigureDatabase(rusqlite::Error),
    QuerySymbolNames(rusqlite::Error),
    WriteSymbolName(rusqlite::Error),
    ReadBatch(io::Error),
    ProjectNotFound {
        project_id: u32,
    },
    UnknownModule {
        project_id: u32,
        module_id: u32,
    },
    UnknownSymbol {
        module_id: u32,
        original_name: String,
    },
    InvalidSemanticName {
        semantic_name: String,
    },
    ConflictingOperation {
        module_id: u32,
        original_name: String,
    },
    NameCollision {
        module_id: u32,
        semantic_name: String,
        existing_original_name: String,
    },
    InvalidBatchLine {
        line: usize,
        message: String,
    },
    InvalidDatabaseId {
        owner: &'static str,
        value: i64,
    },
}

impl fmt::Display for SymbolNamesError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenDatabase { path, source } => {
                write!(formatter, "failed to open {}: {source}", path.display())
            }
            Self::ConfigureDatabase(source) => {
                write!(formatter, "failed to configure SQLite: {source}")
            }
            Self::QuerySymbolNames(source) => {
                write!(formatter, "failed to query symbol names: {source}")
            }
            Self::WriteSymbolName(source) => {
                write!(formatter, "failed to write symbol name: {source}")
            }
            Self::ReadBatch(source) => {
                write!(formatter, "failed to read symbol-name batch: {source}")
            }
            Self::ProjectNotFound { project_id } => {
                write!(formatter, "project {project_id} was not found")
            }
            Self::UnknownModule {
                project_id,
                module_id,
            } => write!(
                formatter,
                "module {module_id} does not belong to project {project_id}"
            ),
            Self::UnknownSymbol {
                module_id,
                original_name,
            } => write!(
                formatter,
                "module {module_id} has no module-scope symbol named {original_name}"
            ),
            Self::InvalidSemanticName { semantic_name } => write!(
                formatter,
                "semantic name {semantic_name} is not a valid JavaScript identifier"
            ),
            Self::ConflictingOperation {
                module_id,
                original_name,
            } => write!(
                formatter,
                "multiple operations target module {module_id} symbol {original_name}"
            ),
            Self::NameCollision {
                module_id,
                semantic_name,
                existing_original_name,
            } => write!(
                formatter,
                "module {module_id} semantic name {semantic_name} collides with symbol {existing_original_name}"
            ),
            Self::InvalidBatchLine { line, message } => {
                write!(
                    formatter,
                    "invalid symbol-name batch line {line}: {message}"
                )
            }
            Self::InvalidDatabaseId { owner, value } => {
                write!(formatter, "invalid database id {value} for {owner}")
            }
        }
    }
}

impl Error for SymbolNamesError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::OpenDatabase { source, .. }
            | Self::ConfigureDatabase(source)
            | Self::QuerySymbolNames(source)
            | Self::WriteSymbolName(source) => Some(source),
            Self::ReadBatch(source) => Some(source),
            Self::ProjectNotFound { .. }
            | Self::UnknownModule { .. }
            | Self::UnknownSymbol { .. }
            | Self::InvalidSemanticName { .. }
            | Self::ConflictingOperation { .. }
            | Self::NameCollision { .. }
            | Self::InvalidBatchLine { .. }
            | Self::InvalidDatabaseId { .. } => None,
        }
    }
}

#[derive(Debug)]
pub enum CliRunError {
    Args(CliError),
    LoadInput(SqliteInputError),
    Pipeline(PipelineError),
    MatchPackages(MatchPackagesError),
    ImportUnpacked(ImportUnpackedError),
    ExtractAssets(ExtractAssetsError),
    RuntimeInventory(RuntimeInventoryError),
    NamingProgress(NamingProgressError),
    SymbolNames(SymbolNamesError),
    AuditRejected(String),
    UnsafeOutputPath(PathBuf),
    WriteOutput { path: PathBuf, source: io::Error },
    MatchModulesRecall(String),
}

impl fmt::Display for CliRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Args(source) => write!(formatter, "{source}"),
            Self::LoadInput(source) => write!(formatter, "{source}"),
            Self::Pipeline(source) => write!(formatter, "{source}"),
            Self::MatchPackages(source) => write!(formatter, "{source}"),
            Self::ImportUnpacked(source) => write!(formatter, "{source}"),
            Self::ExtractAssets(source) => write!(formatter, "{source}"),
            Self::RuntimeInventory(source) => write!(formatter, "{source}"),
            Self::NamingProgress(source) => write!(formatter, "{source}"),
            Self::SymbolNames(source) => write!(formatter, "{source}"),
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
            Self::MatchModulesRecall(message) => {
                write!(formatter, "match-modules-recall: {message}")
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
            Self::ImportUnpacked(source) => Some(source),
            Self::ExtractAssets(source) => Some(source),
            Self::RuntimeInventory(source) => Some(source),
            Self::NamingProgress(source) => Some(source),
            Self::SymbolNames(source) => Some(source),
            Self::WriteOutput { source, .. } => Some(source),
            Self::AuditRejected(_) | Self::UnsafeOutputPath(_) | Self::MatchModulesRecall(_) => {
                None
            }
        }
    }
}
