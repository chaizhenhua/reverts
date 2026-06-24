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
    WriteSyntheticSource {
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
    ReadPackageSurfaceDecisionBatch(io::Error),
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
            Self::WriteSyntheticSource { path, source } => {
                write!(
                    formatter,
                    "failed to write synthetic source {}: {source}",
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
            Self::ReadPackageSurfaceDecisionBatch(source) => {
                write!(
                    formatter,
                    "failed to read package surface decision batch: {source}"
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
            Self::ReadPackageSurfaceDecisionBatch(source) => Some(source),
            Self::NormalizePackageSource { source, .. } => Some(source),
            Self::ReadPackageSourceRoot { source, .. }
            | Self::WriteSyntheticSource { source, .. } => Some(source),
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
    Classification(ModuleClassifyError),
    ReadSymbolIndex(io::Error),
    /// `--gate` was requested and the target tier is not fully named.
    GateUnmet {
        tier: String,
        named: usize,
        universe: usize,
    },
}

impl fmt::Display for NamingProgressError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LoadInput(source) => write!(formatter, "{source}"),
            Self::Pipeline(source) => write!(formatter, "{source}"),
            Self::Classification(source) => write!(formatter, "{source}"),
            Self::ReadSymbolIndex(source) => {
                write!(formatter, "failed to read symbol index: {source}")
            }
            Self::GateUnmet {
                tier,
                named,
                universe,
            } => write!(
                formatter,
                "naming gate unmet: tier '{tier}' is {named}/{universe} named \
                 ({} symbol(s) still need a semantic name)",
                universe.saturating_sub(*named),
            ),
        }
    }
}

impl Error for NamingProgressError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::LoadInput(source) => Some(source),
            Self::Pipeline(source) => Some(source),
            Self::Classification(source) => Some(source),
            Self::ReadSymbolIndex(source) => Some(source),
            Self::GateUnmet { .. } => None,
        }
    }
}

#[derive(Debug)]
pub enum ModuleClassifyError {
    OpenDatabase {
        path: PathBuf,
        source: rusqlite::Error,
    },
    ConfigureDatabase(rusqlite::Error),
    QueryClassification(rusqlite::Error),
    WriteClassification(rusqlite::Error),
    ReadBatch(io::Error),
    InvalidBatchLine {
        line: usize,
        message: String,
    },
    ProjectNotFound {
        project_id: u32,
    },
    InvalidDatabaseId {
        owner: &'static str,
        value: i64,
    },
}

impl fmt::Display for ModuleClassifyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenDatabase { path, source } => {
                write!(formatter, "failed to open {}: {source}", path.display())
            }
            Self::ConfigureDatabase(source) => {
                write!(formatter, "failed to configure SQLite: {source}")
            }
            Self::QueryClassification(source) => {
                write!(formatter, "failed to query module_classification: {source}")
            }
            Self::WriteClassification(source) => {
                write!(formatter, "failed to write module_classification: {source}")
            }
            Self::ReadBatch(source) => write!(formatter, "failed to read batch: {source}"),
            Self::InvalidBatchLine { line, message } => {
                write!(formatter, "invalid batch line {line}: {message}")
            }
            Self::ProjectNotFound { project_id } => {
                write!(
                    formatter,
                    "project {project_id} was not found in SQLite database"
                )
            }
            Self::InvalidDatabaseId { owner, value } => {
                write!(formatter, "invalid {owner} value {value}")
            }
        }
    }
}

impl Error for ModuleClassifyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::OpenDatabase { source, .. }
            | Self::ConfigureDatabase(source)
            | Self::QueryClassification(source)
            | Self::WriteClassification(source) => Some(source),
            Self::ReadBatch(source) => Some(source),
            Self::InvalidBatchLine { .. }
            | Self::ProjectNotFound { .. }
            | Self::InvalidDatabaseId { .. } => None,
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
    PlaceholderSemanticName {
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
    NamingGate {
        message: String,
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
            Self::PlaceholderSemanticName { semantic_name } => write!(
                formatter,
                "semantic name {semantic_name} is a generated placeholder, not an accepted semantic name"
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
            Self::NamingGate { message } => write!(formatter, "{message}"),
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
            | Self::PlaceholderSemanticName { .. }
            | Self::ConflictingOperation { .. }
            | Self::NameCollision { .. }
            | Self::InvalidBatchLine { .. }
            | Self::InvalidDatabaseId { .. }
            | Self::NamingGate { .. } => None,
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
    ModuleClassify(ModuleClassifyError),
    SymbolNames(SymbolNamesError),
    FullInventory(String),
    CoverageLedger(String),
    IdentifierInventory(String),
    GenerateProject(String),
    BindingNames(String),
    ModuleNames(String),
    ClusterNames(String),
    IslandPackageCandidates(String),
    ReferenceSourceNames(String),
    AuditRejected(String),
    UnsafeOutputPath(PathBuf),
    WriteOutput { path: PathBuf, source: io::Error },
    MatchModulesRecall(String),
}

impl CliRunError {
    /// A concrete next step for every failure, so the CLI never dead-ends: the
    /// operator/Agent always sees how to recover or what to try next. Printed by
    /// `main` after the error message.
    #[must_use]
    pub fn next_step(&self) -> &'static str {
        match self {
            Self::Args(_) => {
                "next: invalid arguments. Run `reverts-cli <command> --help` for the exact usage and flags."
            }
            Self::LoadInput(_) => {
                "next: the SQLite DB or a referenced source file is missing/unreadable. Source BYTES live on \
                 disk at `source_files.file_path` (the DB stores only paths). If a working dir was deleted \
                 (/tmp, .reverts-synthetic-sources, .reverts-import-sources), restore it from the generated \
                 `<output>/sources/<original-path>` mirror, then retry."
            }
            Self::Pipeline(_) => {
                "next: a recovery/emit invariant failed. Re-run `generate`; if it is a parse/audit \
                 issue, inspect the printed audit warnings and fix the pipeline cause — never hand-edit output."
            }
            Self::MatchPackages(_) => {
                "next: package matching failed. Confirm the version exists (`npm view <pkg> versions`), check \
                 network access, and re-propose inlined-library candidates with valid versions via \
                 `island-package-candidates --accept <pkg> --version <v> --apply`, then re-run match-packages."
            }
            Self::ImportUnpacked(_) => {
                "next: import failed — the reverts.import_evidence.v1 manifest must cover EVERY input file with \
                 matching recorded size/hash. Regenerate the manifest for the extracted app root and retry."
            }
            Self::ExtractAssets(_) => {
                "next: asset extraction failed. Check the output directory exists and is writable, and that the \
                 source assets are present, then retry."
            }
            Self::RuntimeInventory(_) | Self::FullInventory(_) | Self::IdentifierInventory(_) => {
                "next: inventory could not be computed. Ensure the DB loads (`naming-progress` should succeed) \
                 and that any `--symbol-index` path exists; regenerate the project to refresh `.reverts/`."
            }
            // The `--gate` failure is not a compute error — coverage was computed
            // fine, the tier simply is not 100% named. Point at the naming loop.
            Self::NamingProgress(NamingProgressError::GateUnmet { .. }) => {
                "next: naming gate failed — the tier is not 100% named. Pull the residue with \
                 `naming-plan --target-level <tier>`, accept names (`symbol-names`/`binding-names`), \
                 regenerate, then re-run `naming-progress --gate` until it exits 0."
            }
            Self::NamingProgress(_) | Self::CoverageLedger(_) => {
                "next: could not compute naming coverage. Ensure the DB loads and the `--symbol-index` file (if \
                 passed) exists; otherwise omit it so the command regenerates the index from the DB."
            }
            Self::ModuleClassify(_) => {
                "next: classification spec rejected. Run `module-classify --list` to inspect current state and \
                 check the `--batch` rows (MODULE_ID<TAB>classification<TAB>evidence); `--help` for the format."
            }
            Self::SymbolNames(_) => {
                "next: symbol-name spec rejected. Verify the DB module id space (symbol-names is module_id-keyed) \
                 and the accept/batch format; `symbol-names --list` shows current names, `--help` the usage."
            }
            Self::BindingNames(_) => {
                "next: binding-name spec rejected. binding-names is file-path-keyed (island bindings). Check the \
                 `file_path<TAB>original<TAB>semantic<TAB>evidence` rows; `--origin human` is evidence-exempt."
            }
            Self::ModuleNames(_) => {
                "next: module-name spec rejected. module-names is emitted-id-keyed; verify the id and the \
                 `ID=path` form, then re-run with `--apply`. `--list` shows current overrides."
            }
            Self::ClusterNames(_) => {
                "next: cluster-name spec rejected. cluster-names is content-fingerprint-keyed; verify the \
                 `<fingerprint>=vendor/<name>` form. Use this to relocate un-externalizable inlined libraries \
                 under `vendor/`. `--list` shows current names."
            }
            Self::IslandPackageCandidates(_) => {
                "next: island-package candidate rejected. Check the `accept|reject<TAB>pkg<TAB>version|-<TAB>evidence` \
                 rows; a wrong version is harmless (it just won't anchor). `--list` shows accepted candidates."
            }
            Self::ReferenceSourceNames(_) => {
                "next: reference-source naming failed. Confirm `--reference-source-root` points at a readable \
                 first-party source tree and the project loads; `--help` for usage."
            }
            Self::AuditRejected(_) => {
                "next: the generated project failed structural audit. Read each listed finding and fix its \
                 pipeline cause (wire-rename, dangling import, missing asset, …) — do NOT hand-edit the output. \
                 Re-run after the fix."
            }
            Self::UnsafeOutputPath(_) => {
                "next: an emitted path escaped the output root — a pipeline bug, not a usage error. Report the \
                 offending module/binding; do not work around it by relocating files manually."
            }
            Self::WriteOutput { .. } => {
                "next: could not write output. Check free disk space (`df -h`) and write permissions on the \
                 output directory, then retry."
            }
            Self::GenerateProject(_) => {
                "next: project generation failed. Re-run `generate`; if a source file is missing, \
                 restore it from `<output>/sources/` (see LoadInput), and check the output dir is writable."
            }
            Self::MatchModulesRecall(_) => {
                "next: recall measurement failed. Confirm both the project and the ground-truth project load; \
                 `--help` for usage."
            }
        }
    }
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
            Self::ModuleClassify(source) => write!(formatter, "{source}"),
            Self::SymbolNames(source) => write!(formatter, "{source}"),
            Self::FullInventory(message) => write!(formatter, "full-inventory: {message}"),
            Self::CoverageLedger(message) => write!(formatter, "coverage-ledger: {message}"),
            Self::IdentifierInventory(message) => {
                write!(formatter, "identifier-inventory: {message}")
            }
            Self::GenerateProject(message) => write!(formatter, "generate: {message}"),
            Self::BindingNames(message) => write!(formatter, "binding-names: {message}"),
            Self::ModuleNames(message) => write!(formatter, "module-names: {message}"),
            Self::ClusterNames(message) => write!(formatter, "cluster-names: {message}"),
            Self::IslandPackageCandidates(message) => {
                write!(formatter, "island-package-candidates: {message}")
            }
            Self::ReferenceSourceNames(message) => {
                write!(formatter, "reference-source-names: {message}")
            }
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
            Self::ModuleClassify(source) => Some(source),
            Self::SymbolNames(source) => Some(source),
            Self::WriteOutput { source, .. } => Some(source),
            Self::AuditRejected(_)
            | Self::UnsafeOutputPath(_)
            | Self::MatchModulesRecall(_)
            | Self::FullInventory(_)
            | Self::CoverageLedger(_)
            | Self::GenerateProject(_)
            | Self::BindingNames(_)
            | Self::ModuleNames(_)
            | Self::ClusterNames(_)
            | Self::IslandPackageCandidates(_)
            | Self::ReferenceSourceNames(_)
            | Self::IdentifierInventory(_) => None,
        }
    }
}
