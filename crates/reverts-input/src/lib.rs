use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use reverts_ir::{ModuleId, ModuleKind, is_valid_package_name, split_bare_specifier};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInput {
    pub id: u32,
    pub name: String,
}

impl ProjectInput {
    #[must_use]
    pub fn new(id: u32, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFileInput {
    pub id: u32,
    pub path: String,
    pub source: Option<String>,
}

impl SourceFileInput {
    #[must_use]
    pub fn new(id: u32, path: impl Into<String>, source: Option<String>) -> Self {
        Self {
            id,
            path: path.into(),
            source,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleInput {
    pub id: ModuleId,
    pub kind: ModuleKind,
    pub original_name: String,
    pub semantic_path: String,
    pub source_file_id: Option<u32>,
    pub package_name: Option<String>,
    pub package_version: Option<String>,
}

impl ModuleInput {
    #[must_use]
    pub fn application(
        id: ModuleId,
        original_name: impl Into<String>,
        semantic_path: impl Into<String>,
    ) -> Self {
        Self {
            id,
            kind: ModuleKind::Application,
            original_name: original_name.into(),
            semantic_path: semantic_path.into(),
            source_file_id: None,
            package_name: None,
            package_version: None,
        }
    }

    #[must_use]
    pub fn package(
        id: ModuleId,
        original_name: impl Into<String>,
        semantic_path: impl Into<String>,
        package_name: impl Into<String>,
        package_version: Option<String>,
    ) -> Self {
        Self {
            id,
            kind: ModuleKind::Package,
            original_name: original_name.into(),
            semantic_path: semantic_path.into(),
            source_file_id: None,
            package_name: Some(package_name.into()),
            package_version,
        }
    }

    #[must_use]
    pub fn with_source_file(mut self, source_file_id: u32) -> Self {
        self.source_file_id = Some(source_file_id);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolInput {
    pub module_id: ModuleId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleDependencyInput {
    pub from_module_id: ModuleId,
    pub target: ModuleDependencyTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleDependencyTarget {
    Module(ModuleId),
    Package { specifier: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageEmissionMode {
    ExternalImport,
    VendoredAsset,
    ApplicationSource,
    RuntimeGlue,
}

impl PackageEmissionMode {
    #[must_use]
    pub const fn requires_runtime_dependency(self) -> bool {
        matches!(self, Self::ExternalImport)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageAttributionStatus {
    Proposed,
    Accepted,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageAttributionInput {
    pub module_id: ModuleId,
    pub package_name: String,
    pub package_version: Option<String>,
    pub subpath: Option<String>,
    pub export_specifier: Option<String>,
    pub emission_mode: PackageEmissionMode,
    pub status: PackageAttributionStatus,
    pub rejection_reason: Option<String>,
}

impl PackageAttributionInput {
    #[must_use]
    pub fn proposed(
        module_id: ModuleId,
        package_name: impl Into<String>,
        package_version: Option<String>,
        emission_mode: PackageEmissionMode,
    ) -> Self {
        Self {
            module_id,
            package_name: package_name.into(),
            package_version,
            subpath: None,
            export_specifier: None,
            emission_mode,
            status: PackageAttributionStatus::Proposed,
            rejection_reason: None,
        }
    }

    #[must_use]
    pub fn accepted_external(
        module_id: ModuleId,
        package_name: impl Into<String>,
        package_version: impl Into<String>,
        export_specifier: impl Into<String>,
    ) -> Self {
        Self {
            module_id,
            package_name: package_name.into(),
            package_version: Some(package_version.into()),
            subpath: None,
            export_specifier: Some(export_specifier.into()),
            emission_mode: PackageEmissionMode::ExternalImport,
            status: PackageAttributionStatus::Accepted,
            rejection_reason: None,
        }
    }

    #[must_use]
    pub fn with_subpath(mut self, subpath: impl Into<String>) -> Self {
        self.subpath = normalize_optional(Some(subpath.into()));
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputBundle {
    pub project: ProjectInput,
    pub source_files: Vec<SourceFileInput>,
    pub modules: Vec<ModuleInput>,
    pub symbols: Vec<SymbolInput>,
    pub dependencies: Vec<ModuleDependencyInput>,
    pub package_attributions: Vec<PackageAttributionInput>,
}

impl InputBundle {
    pub fn from_rows(rows: InputRows) -> Result<Self, InputBundleError> {
        validate_project(&rows.project)?;
        let source_file_ids = collect_source_file_ids(&rows.source_files)?;
        let module_ids = collect_module_ids(&rows.modules)?;
        validate_modules(&rows.modules, &source_file_ids)?;
        validate_symbols(&rows.symbols, &module_ids)?;
        validate_dependencies(&rows.dependencies, &module_ids)?;
        validate_package_attributions(&rows.modules, &rows.package_attributions, &module_ids)?;

        Ok(Self {
            project: rows.project,
            source_files: rows.source_files,
            modules: rows.modules,
            symbols: rows.symbols,
            dependencies: rows.dependencies,
            package_attributions: rows.package_attributions,
        })
    }

    pub fn from_database_rows(rows: DatabaseRows) -> Result<Self, InputBundleError> {
        Self::from_rows(InputRows::from_database_rows(rows)?)
    }

    #[must_use]
    pub fn module_ids(&self) -> BTreeSet<ModuleId> {
        self.modules.iter().map(|module| module.id).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputRows {
    pub project: ProjectInput,
    pub source_files: Vec<SourceFileInput>,
    pub modules: Vec<ModuleInput>,
    pub symbols: Vec<SymbolInput>,
    pub dependencies: Vec<ModuleDependencyInput>,
    pub package_attributions: Vec<PackageAttributionInput>,
}

impl InputRows {
    #[must_use]
    pub fn new(project: ProjectInput) -> Self {
        Self {
            project,
            source_files: Vec::new(),
            modules: Vec::new(),
            symbols: Vec::new(),
            dependencies: Vec::new(),
            package_attributions: Vec::new(),
        }
    }

    pub fn from_database_rows(rows: DatabaseRows) -> Result<Self, InputBundleError> {
        let project_id = checked_u32_id(rows.project.id, "project.id")?;
        let project = ProjectInput::new(project_id, non_empty(rows.project.name, "project.name")?);

        let mut source_files = Vec::with_capacity(rows.source_files.len());
        for row in rows.source_files {
            let id = checked_u32_id(row.id, "source_file.id")?;
            let row_project_id = checked_u32_id(row.project_id, "source_file.project_id")?;
            if row_project_id != project_id {
                return Err(InputBundleError::ProjectMismatch {
                    source_file_id: id,
                    project_id: row_project_id,
                    expected_project_id: project_id,
                });
            }
            source_files.push(SourceFileInput::new(
                id,
                non_empty(row.path, "source_file.path")?,
                row.source,
            ));
        }

        let mut modules = Vec::with_capacity(rows.modules.len());
        for row in rows.modules {
            let id = ModuleId(checked_u32_id(row.id, "module.id")?);
            let original_name = non_empty(row.original_name, "module.original_name")?;
            let semantic_path = non_empty_optional(row.semantic_path, "module.semantic_path")?;
            let source_file_id = row
                .source_file_id
                .map(|value| checked_u32_id(value, "module.source_file_id"))
                .transpose()?;
            let kind = row
                .kind
                .to_module_kind()
                .ok_or(InputBundleError::UnsupportedModuleKind { module_id: id })?;
            modules.push(ModuleInput {
                id,
                kind,
                original_name,
                semantic_path,
                source_file_id,
                package_name: normalize_optional(row.package_name),
                package_version: normalize_optional(row.package_version),
            });
        }

        let mut symbols = Vec::with_capacity(rows.symbols.len());
        for row in rows.symbols {
            symbols.push(SymbolInput {
                module_id: ModuleId(checked_u32_id(row.module_id, "symbol.module_id")?),
                name: non_empty(row.name, "symbol.name")?,
            });
        }

        let mut dependencies = Vec::with_capacity(rows.dependencies.len());
        for row in rows.dependencies {
            dependencies.push(ModuleDependencyInput {
                from_module_id: ModuleId(checked_u32_id(
                    row.from_module_id,
                    "dependency.from_module_id",
                )?),
                target: match row.target {
                    ModuleDependencyRowTarget::Module { module_id } => {
                        ModuleDependencyTarget::Module(ModuleId(checked_u32_id(
                            module_id,
                            "dependency.target_module_id",
                        )?))
                    }
                    ModuleDependencyRowTarget::Package { specifier } => {
                        ModuleDependencyTarget::Package {
                            specifier: non_empty(specifier, "dependency.package_specifier")?,
                        }
                    }
                },
            });
        }

        let mut package_attributions = Vec::with_capacity(rows.package_attributions.len());
        for row in rows.package_attributions {
            package_attributions.push(PackageAttributionInput {
                module_id: ModuleId(checked_u32_id(
                    row.module_id,
                    "package_attribution.module_id",
                )?),
                package_name: non_empty(row.package_name, "package_attribution.package_name")?,
                package_version: normalize_optional(row.package_version),
                subpath: normalize_optional(row.subpath),
                export_specifier: normalize_optional(row.export_specifier),
                emission_mode: row.emission_mode,
                status: row.status,
                rejection_reason: normalize_optional(row.rejection_reason),
            });
        }

        Ok(Self {
            project,
            source_files,
            modules,
            symbols,
            dependencies,
            package_attributions,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseRows {
    pub project: ProjectRow,
    pub source_files: Vec<SourceFileRow>,
    pub modules: Vec<ModuleRow>,
    pub symbols: Vec<SymbolRow>,
    pub dependencies: Vec<ModuleDependencyRow>,
    pub package_attributions: Vec<PackageAttributionRow>,
}

impl DatabaseRows {
    #[must_use]
    pub fn new(project: ProjectRow) -> Self {
        Self {
            project,
            source_files: Vec::new(),
            modules: Vec::new(),
            symbols: Vec::new(),
            dependencies: Vec::new(),
            package_attributions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRow {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFileRow {
    pub id: i64,
    pub project_id: i64,
    pub path: String,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredModuleKind {
    Application,
    Package,
    Builtin,
    Unknown,
}

impl StoredModuleKind {
    const fn to_module_kind(self) -> Option<ModuleKind> {
        match self {
            Self::Application => Some(ModuleKind::Application),
            Self::Package => Some(ModuleKind::Package),
            Self::Builtin => Some(ModuleKind::Builtin),
            Self::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleRow {
    pub id: i64,
    pub source_file_id: Option<i64>,
    pub original_name: String,
    pub semantic_path: Option<String>,
    pub kind: StoredModuleKind,
    pub package_name: Option<String>,
    pub package_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRow {
    pub module_id: i64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleDependencyRow {
    pub from_module_id: i64,
    pub target: ModuleDependencyRowTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleDependencyRowTarget {
    Module { module_id: i64 },
    Package { specifier: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageAttributionRow {
    pub module_id: i64,
    pub package_name: String,
    pub package_version: Option<String>,
    pub subpath: Option<String>,
    pub export_specifier: Option<String>,
    pub emission_mode: PackageEmissionMode,
    pub status: PackageAttributionStatus,
    pub rejection_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputBundleError {
    EmptyField(&'static str),
    InvalidId {
        field: &'static str,
        value: i64,
    },
    ProjectMismatch {
        source_file_id: u32,
        project_id: u32,
        expected_project_id: u32,
    },
    DuplicateSourceFileId(u32),
    DuplicateModuleId(ModuleId),
    UnknownSourceFile {
        source_file_id: u32,
        owner: &'static str,
    },
    UnsupportedModuleKind {
        module_id: ModuleId,
    },
    UnknownModule {
        module_id: ModuleId,
        owner: &'static str,
    },
    InvalidPackageName(String),
    InvalidPackageSpecifier(String),
    MissingPackageAttribution {
        module_id: ModuleId,
    },
    MissingPackageVersion {
        module_id: ModuleId,
        package_name: String,
    },
    MissingExportSpecifier {
        module_id: ModuleId,
        package_name: String,
    },
    MissingRejectionReason {
        module_id: ModuleId,
        package_name: String,
    },
}

impl fmt::Display for InputBundleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField(field) => write!(formatter, "{field} must not be empty"),
            Self::InvalidId { field, value } => {
                write!(
                    formatter,
                    "{field} must be a positive u32-compatible id, got {value}"
                )
            }
            Self::ProjectMismatch {
                source_file_id,
                project_id,
                expected_project_id,
            } => write!(
                formatter,
                "source file {source_file_id} belongs to project {project_id}, expected {expected_project_id}"
            ),
            Self::DuplicateSourceFileId(id) => write!(formatter, "duplicate source file id {id}"),
            Self::DuplicateModuleId(module_id) => {
                write!(formatter, "duplicate module id {}", module_id.0)
            }
            Self::UnknownSourceFile {
                source_file_id,
                owner,
            } => write!(
                formatter,
                "{owner} references unknown source file {source_file_id}"
            ),
            Self::UnsupportedModuleKind { module_id } => {
                write!(
                    formatter,
                    "module {} has unsupported module kind",
                    module_id.0
                )
            }
            Self::UnknownModule { module_id, owner } => {
                write!(
                    formatter,
                    "{owner} references unknown module {}",
                    module_id.0
                )
            }
            Self::InvalidPackageName(package_name) => {
                write!(formatter, "invalid package name {package_name}")
            }
            Self::InvalidPackageSpecifier(specifier) => {
                write!(formatter, "invalid package specifier {specifier}")
            }
            Self::MissingPackageAttribution { module_id } => {
                write!(
                    formatter,
                    "package module {} has no package attribution",
                    module_id.0
                )
            }
            Self::MissingPackageVersion {
                module_id,
                package_name,
            } => write!(
                formatter,
                "accepted package attribution for module {} package {package_name} has no version",
                module_id.0
            ),
            Self::MissingExportSpecifier {
                module_id,
                package_name,
            } => write!(
                formatter,
                "accepted external package attribution for module {} package {package_name} has no export specifier",
                module_id.0
            ),
            Self::MissingRejectionReason {
                module_id,
                package_name,
            } => write!(
                formatter,
                "rejected package attribution for module {} package {package_name} has no rejection reason",
                module_id.0
            ),
        }
    }
}

impl Error for InputBundleError {}

fn validate_project(project: &ProjectInput) -> Result<(), InputBundleError> {
    ensure_non_empty(project.name.as_str(), "project.name")
}

fn collect_source_file_ids(
    source_files: &[SourceFileInput],
) -> Result<BTreeSet<u32>, InputBundleError> {
    let mut ids = BTreeSet::new();
    for source_file in source_files {
        ensure_non_empty(source_file.path.as_str(), "source_file.path")?;
        if !ids.insert(source_file.id) {
            return Err(InputBundleError::DuplicateSourceFileId(source_file.id));
        }
    }
    Ok(ids)
}

fn collect_module_ids(modules: &[ModuleInput]) -> Result<BTreeSet<ModuleId>, InputBundleError> {
    let mut ids = BTreeSet::new();
    for module in modules {
        ensure_non_empty(module.original_name.as_str(), "module.original_name")?;
        ensure_non_empty(module.semantic_path.as_str(), "module.semantic_path")?;
        if !ids.insert(module.id) {
            return Err(InputBundleError::DuplicateModuleId(module.id));
        }
    }
    Ok(ids)
}

fn validate_modules(
    modules: &[ModuleInput],
    source_file_ids: &BTreeSet<u32>,
) -> Result<(), InputBundleError> {
    for module in modules {
        if let Some(source_file_id) = module.source_file_id
            && !source_file_ids.contains(&source_file_id)
        {
            return Err(InputBundleError::UnknownSourceFile {
                source_file_id,
                owner: "module",
            });
        }
    }
    Ok(())
}

fn validate_symbols(
    symbols: &[SymbolInput],
    module_ids: &BTreeSet<ModuleId>,
) -> Result<(), InputBundleError> {
    for symbol in symbols {
        ensure_module_exists(symbol.module_id, module_ids, "symbol")?;
        ensure_non_empty(symbol.name.as_str(), "symbol.name")?;
    }
    Ok(())
}

fn validate_dependencies(
    dependencies: &[ModuleDependencyInput],
    module_ids: &BTreeSet<ModuleId>,
) -> Result<(), InputBundleError> {
    for dependency in dependencies {
        ensure_module_exists(dependency.from_module_id, module_ids, "dependency")?;
        match &dependency.target {
            ModuleDependencyTarget::Module(target_module_id) => {
                ensure_module_exists(*target_module_id, module_ids, "dependency target")?;
            }
            ModuleDependencyTarget::Package { specifier } => {
                validate_package_specifier(specifier)?;
            }
        }
    }
    Ok(())
}

fn validate_package_attributions(
    modules: &[ModuleInput],
    attributions: &[PackageAttributionInput],
    module_ids: &BTreeSet<ModuleId>,
) -> Result<(), InputBundleError> {
    let mut attributions_by_module = BTreeMap::new();
    for attribution in attributions {
        ensure_module_exists(attribution.module_id, module_ids, "package attribution")?;
        validate_package_attribution(attribution)?;
        attributions_by_module.insert(attribution.module_id, attribution);
    }

    for module in modules {
        if module.kind == ModuleKind::Package && !attributions_by_module.contains_key(&module.id) {
            return Err(InputBundleError::MissingPackageAttribution {
                module_id: module.id,
            });
        }
    }

    Ok(())
}

fn validate_package_attribution(
    attribution: &PackageAttributionInput,
) -> Result<(), InputBundleError> {
    if !is_valid_package_name(&attribution.package_name) {
        return Err(InputBundleError::InvalidPackageName(
            attribution.package_name.clone(),
        ));
    }

    match attribution.status {
        PackageAttributionStatus::Accepted => {
            if attribution
                .package_version
                .as_deref()
                .is_none_or(str::is_empty)
            {
                return Err(InputBundleError::MissingPackageVersion {
                    module_id: attribution.module_id,
                    package_name: attribution.package_name.clone(),
                });
            }
            if attribution.emission_mode == PackageEmissionMode::ExternalImport
                && attribution
                    .export_specifier
                    .as_deref()
                    .is_none_or(str::is_empty)
            {
                return Err(InputBundleError::MissingExportSpecifier {
                    module_id: attribution.module_id,
                    package_name: attribution.package_name.clone(),
                });
            }
        }
        PackageAttributionStatus::Rejected => {
            if attribution
                .rejection_reason
                .as_deref()
                .is_none_or(str::is_empty)
            {
                return Err(InputBundleError::MissingRejectionReason {
                    module_id: attribution.module_id,
                    package_name: attribution.package_name.clone(),
                });
            }
        }
        PackageAttributionStatus::Proposed => {}
    }

    Ok(())
}

fn validate_package_specifier(specifier: &str) -> Result<(), InputBundleError> {
    let Some((package_name, _subpath)) = split_bare_specifier(specifier) else {
        return Err(InputBundleError::InvalidPackageSpecifier(
            specifier.to_string(),
        ));
    };

    if is_valid_package_name(&package_name) {
        Ok(())
    } else {
        Err(InputBundleError::InvalidPackageSpecifier(
            specifier.to_string(),
        ))
    }
}

fn ensure_module_exists(
    module_id: ModuleId,
    module_ids: &BTreeSet<ModuleId>,
    owner: &'static str,
) -> Result<(), InputBundleError> {
    if module_ids.contains(&module_id) {
        Ok(())
    } else {
        Err(InputBundleError::UnknownModule { module_id, owner })
    }
}

fn checked_u32_id(value: i64, field: &'static str) -> Result<u32, InputBundleError> {
    if value <= 0 {
        return Err(InputBundleError::InvalidId { field, value });
    }
    u32::try_from(value).map_err(|_error| InputBundleError::InvalidId { field, value })
}

fn non_empty(value: String, field: &'static str) -> Result<String, InputBundleError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        Err(InputBundleError::EmptyField(field))
    } else {
        Ok(trimmed.to_string())
    }
}

fn non_empty_optional(
    value: Option<String>,
    field: &'static str,
) -> Result<String, InputBundleError> {
    value.map_or(Err(InputBundleError::EmptyField(field)), |value| {
        non_empty(value, field)
    })
}

fn normalize_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn ensure_non_empty(value: &str, field: &'static str) -> Result<(), InputBundleError> {
    if value.trim().is_empty() {
        Err(InputBundleError::EmptyField(field))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use reverts_ir::{ModuleId, ModuleKind};

    use super::{
        DatabaseRows, InputBundle, InputBundleError, InputRows, ModuleInput, ModuleRow,
        PackageAttributionInput, PackageAttributionRow, PackageAttributionStatus,
        PackageEmissionMode, ProjectInput, ProjectRow, SourceFileRow, StoredModuleKind,
        SymbolInput, SymbolRow,
    };

    #[test]
    fn rows_build_a_self_contained_input_bundle() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::application(
            ModuleId(10),
            "m10",
            "src/index.ts",
        ));
        rows.symbols.push(SymbolInput {
            module_id: ModuleId(10),
            name: "main".to_string(),
        });

        let bundle = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        assert_eq!(bundle.project.name, "fixture");
        assert!(bundle.module_ids().contains(&ModuleId(10)));
    }

    #[test]
    fn database_rows_build_a_complete_input_bundle_without_sqlite() {
        let mut rows = DatabaseRows::new(ProjectRow {
            id: 7,
            name: "fixture".to_string(),
        });
        rows.source_files.push(SourceFileRow {
            id: 11,
            project_id: 7,
            path: "bundle.js".to_string(),
            source: Some("const answer = 42;".to_string()),
        });
        rows.modules.push(ModuleRow {
            id: 17,
            source_file_id: Some(11),
            original_name: "m17".to_string(),
            semantic_path: Some("src/index.ts".to_string()),
            kind: StoredModuleKind::Application,
            package_name: None,
            package_version: None,
        });
        rows.symbols.push(SymbolRow {
            module_id: 17,
            name: "answer".to_string(),
        });

        let bundle = InputBundle::from_database_rows(rows).expect("database rows should convert");

        assert_eq!(bundle.project.id, 7);
        assert_eq!(bundle.source_files[0].id, 11);
        assert_eq!(bundle.modules[0].original_name, "m17");
        assert_eq!(bundle.modules[0].source_file_id, Some(11));
        assert_eq!(bundle.symbols[0].name, "answer");
    }

    #[test]
    fn package_module_requires_attribution_contract() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "pkg_mod",
            "lodash/map",
            "lodash",
            Some("4.17.21".to_string()),
        ));

        let error = InputBundle::from_rows(rows);

        assert!(matches!(
            error,
            Err(InputBundleError::MissingPackageAttribution {
                module_id: ModuleId(10)
            })
        ));
    }

    #[test]
    fn invalid_package_attribution_is_rejected_before_planning() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "pkg_mod",
            "node_modules/@smithy/XY7/index.js",
            "@smithy/XY7",
            None,
        ));
        rows.package_attributions
            .push(PackageAttributionInput::proposed(
                ModuleId(10),
                "@smithy/XY7",
                None,
                PackageEmissionMode::ExternalImport,
            ));

        let error = InputBundle::from_rows(rows);

        assert!(matches!(
            error,
            Err(InputBundleError::InvalidPackageName(package_name))
                if package_name == "@smithy/XY7"
        ));
    }

    #[test]
    fn accepted_external_attribution_requires_export_specifier() {
        let mut rows = DatabaseRows::new(ProjectRow {
            id: 1,
            name: "fixture".to_string(),
        });
        rows.modules.push(ModuleRow {
            id: 10,
            source_file_id: None,
            original_name: "axios_module".to_string(),
            semantic_path: Some("axios".to_string()),
            kind: StoredModuleKind::Package,
            package_name: Some("axios".to_string()),
            package_version: Some("1.6.0".to_string()),
        });
        rows.package_attributions.push(PackageAttributionRow {
            module_id: 10,
            package_name: "axios".to_string(),
            package_version: Some("1.6.0".to_string()),
            subpath: None,
            export_specifier: None,
            emission_mode: PackageEmissionMode::ExternalImport,
            status: PackageAttributionStatus::Accepted,
            rejection_reason: None,
        });

        let error = InputBundle::from_database_rows(rows);

        assert!(matches!(
            error,
            Err(InputBundleError::MissingExportSpecifier {
                module_id: ModuleId(10),
                package_name
            }) if package_name == "axios"
        ));
    }

    #[test]
    fn module_source_file_reference_must_exist() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(
            ModuleInput::application(ModuleId(10), "m10", "src/index.ts").with_source_file(99),
        );

        let error = InputBundle::from_rows(rows);

        assert!(matches!(
            error,
            Err(InputBundleError::UnknownSourceFile {
                source_file_id: 99,
                owner: "module"
            })
        ));
    }

    #[test]
    fn unknown_database_module_kind_is_rejected() {
        let mut rows = DatabaseRows::new(ProjectRow {
            id: 1,
            name: "fixture".to_string(),
        });
        rows.modules.push(ModuleRow {
            id: 10,
            source_file_id: None,
            original_name: "m10".to_string(),
            semantic_path: Some("src/index.ts".to_string()),
            kind: StoredModuleKind::Unknown,
            package_name: None,
            package_version: None,
        });

        let error = InputBundle::from_database_rows(rows);

        assert!(matches!(
            error,
            Err(InputBundleError::UnsupportedModuleKind {
                module_id: ModuleId(10)
            })
        ));
    }

    #[test]
    fn accepted_non_external_attribution_does_not_require_export_specifier() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput {
            id: ModuleId(10),
            kind: ModuleKind::Package,
            original_name: "asset".to_string(),
            semantic_path: "assets/package-file.js".to_string(),
            source_file_id: None,
            package_name: Some("lodash".to_string()),
            package_version: Some("4.17.21".to_string()),
        });
        rows.package_attributions.push(PackageAttributionInput {
            module_id: ModuleId(10),
            package_name: "lodash".to_string(),
            package_version: Some("4.17.21".to_string()),
            subpath: Some("fp.js".to_string()),
            export_specifier: None,
            emission_mode: PackageEmissionMode::VendoredAsset,
            status: PackageAttributionStatus::Accepted,
            rejection_reason: None,
        });

        let bundle = InputBundle::from_rows(rows).expect("vendored attribution should be valid");

        assert_eq!(
            bundle.package_attributions[0].emission_mode,
            PackageEmissionMode::VendoredAsset
        );
    }
}
