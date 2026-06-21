use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::path::{Component, Path};

use reverts_ir::{
    AxisKind, ByteRange, MatchTier, ModuleId, ModuleKind, NormalizationPassId,
    is_valid_package_name, split_bare_specifier,
};

pub mod sqlite;

/// Current safety policy version for accepted external package imports.
///
/// Any row in `package_attributions` whose `status='accepted'` and
/// `emission_mode='external_import'` MUST stamp this exact value into the
/// `external_import_policy_version` column. The sqlite loader at
/// `crates/reverts-input/src/sqlite.rs` performs a defense-in-depth check on
/// load: rows whose stored version does not equal this constant are silently
/// downgraded to `status='rejected'`/`emission_mode='application_source'` with
/// a "rerun match-packages to revalidate" rejection reason — they are
/// invisible to the rest of the pipeline. This protects against stale rows
/// (older matcher, older oracle) keeping unsafe external wiring alive after
/// the matcher tightens its proof requirements.
///
/// **Lifecycle**: bump this constant only when the externalization safety
/// proof changes in a way that invalidates previously-written attributions.
/// Any tool that writes `accepted/external_import` rows (matcher,
/// `reverts-rollup-apply`, future integrations) must reference this constant
/// directly — never hard-code an integer.
pub const PACKAGE_ATTRIBUTION_EXTERNAL_IMPORT_POLICY_VERSION: i64 = 1;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetKind {
    Wasm,
    NativeNode,
    Executable,
    Data,
    Image,
    Font,
    Css,
    Html,
    Unknown,
}

impl AssetKind {
    #[must_use]
    pub fn from_storage(value: &str) -> Option<Self> {
        let normalized = value.trim().to_ascii_lowercase().replace('-', "_");
        match normalized.as_str() {
            "wasm" | "webassembly" => Some(Self::Wasm),
            "native_node" | "node" | "node_native" => Some(Self::NativeNode),
            "executable" | "bin" | "binary" => Some(Self::Executable),
            "data" => Some(Self::Data),
            "image" => Some(Self::Image),
            "font" => Some(Self::Font),
            "css" | "stylesheet" => Some(Self::Css),
            "html" => Some(Self::Html),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Wasm => "wasm",
            Self::NativeNode => "native_node",
            Self::Executable => "executable",
            Self::Data => "data",
            Self::Image => "image",
            Self::Font => "font",
            Self::Css => "css",
            Self::Html => "html",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetInput {
    pub id: u32,
    pub logical_path: String,
    pub output_path: String,
    pub source_path: Option<String>,
    pub bytes: Vec<u8>,
    pub kind: AssetKind,
    pub executable: bool,
    pub platform: Option<String>,
    pub arch: Option<String>,
}

impl AssetInput {
    #[must_use]
    pub fn new(
        id: u32,
        logical_path: impl Into<String>,
        output_path: impl Into<String>,
        bytes: Vec<u8>,
        kind: AssetKind,
        executable: bool,
    ) -> Self {
        Self {
            id,
            logical_path: logical_path.into(),
            output_path: output_path.into(),
            source_path: None,
            bytes,
            kind,
            executable,
            platform: None,
            arch: None,
        }
    }

    #[must_use]
    pub fn with_source_path(mut self, source_path: impl Into<String>) -> Self {
        self.source_path = Some(source_path.into());
        self
    }

    #[must_use]
    pub fn with_platform(mut self, platform: impl Into<String>) -> Self {
        self.platform = Some(platform.into());
        self
    }

    #[must_use]
    pub fn with_arch(mut self, arch: impl Into<String>) -> Self {
        self.arch = Some(arch.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleInput {
    pub id: ModuleId,
    pub kind: ModuleKind,
    pub original_name: String,
    pub semantic_path: String,
    pub source_file_id: Option<u32>,
    pub source_span: Option<SourceSpan>,
    pub package_name: Option<String>,
    pub package_version: Option<String>,
}

/// Byte range of a module inside its backing source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceSpan {
    /// Inclusive UTF-8 byte offset where the module body starts.
    pub byte_start: u32,
    /// Exclusive UTF-8 byte offset where the module body ends.
    pub byte_end: u32,
}

/// Resolved source body for one module.
///
/// The slice is the only supported way for downstream pipelines to read module
/// source. A module with a byte span reads that range from its backing file; a
/// module without a span can use the whole file only when it is the sole module
/// attached to that file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleSourceSlice<'a> {
    /// Module that owns this source slice.
    pub module_id: ModuleId,
    /// Source file backing this slice.
    pub source_file_id: u32,
    /// Original source file path.
    pub source_file_path: &'a str,
    /// Source text that belongs to the module.
    pub source: &'a str,
    /// Optional byte span used to extract the slice.
    pub span: Option<SourceSpan>,
}

impl SourceSpan {
    /// Creates a byte range for a module source slice.
    #[must_use]
    pub const fn new(byte_start: u32, byte_end: u32) -> Self {
        Self {
            byte_start,
            byte_end,
        }
    }
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
            source_span: None,
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
            source_span: None,
            package_name: Some(package_name.into()),
            package_version,
        }
    }

    #[must_use]
    pub fn with_source_file(mut self, source_file_id: u32) -> Self {
        self.source_file_id = Some(source_file_id);
        self
    }

    #[must_use]
    pub fn with_source_span(mut self, source_span: SourceSpan) -> Self {
        self.source_span = Some(source_span);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolInput {
    pub module_id: ModuleId,
    /// Source-backed binding identity as it appears in the module body.
    pub name: String,
    /// Optional readability hint. This is not a declaration identity unless an
    /// AST rewrite pass later proves that every use was renamed consistently.
    pub semantic_name: Option<String>,
    /// Optional export-name hint from the source database.
    pub export_name: Option<String>,
    /// Symbol-table scope as recovered by ingestion. Module symbols are part of
    /// the normal source surface; global symbols are only used by conservative
    /// owner reconstruction passes and must not drive readability renames.
    pub scope: SymbolScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolScope {
    Module,
    Global,
}

impl SymbolInput {
    #[must_use]
    pub fn new(module_id: ModuleId, name: impl Into<String>) -> Self {
        Self {
            module_id,
            name: name.into(),
            semantic_name: None,
            export_name: None,
            scope: SymbolScope::Module,
        }
    }

    #[must_use]
    pub fn with_semantic_name(mut self, semantic_name: impl Into<String>) -> Self {
        self.semantic_name = Some(semantic_name.into());
        self
    }

    #[must_use]
    pub fn with_export_name(mut self, export_name: impl Into<String>) -> Self {
        self.export_name = Some(export_name.into());
        self
    }

    #[must_use]
    pub const fn with_scope(mut self, scope: SymbolScope) -> Self {
        self.scope = scope;
        self
    }
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

#[derive(Debug, Clone, PartialEq)]
pub struct AttributionConfidence {
    pub tier: MatchTier,
    pub matched_axes: Vec<AxisKind>,
    pub matched_alternate: Option<NormalizationPassId>,
    pub top_score: f64,
    pub runner_up_score: f64,
    pub margin: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PackageAttributionInput {
    pub module_id: ModuleId,
    pub package_name: String,
    pub package_version: Option<String>,
    pub subpath: Option<String>,
    /// Physical/package-cache source path that proved this attribution.
    ///
    /// Generation normally only needs the public `export_specifier`, but some
    /// safely externalizable assets (notably JSON package entry points) also
    /// need import attributes. Keeping the resolved source path on the
    /// generation input lets the package resolver recover those import-shape
    /// details without re-reading the package source cache.
    pub resolved_file: Option<String>,
    pub export_specifier: Option<String>,
    pub emission_mode: PackageEmissionMode,
    pub status: PackageAttributionStatus,
    pub rejection_reason: Option<String>,
    pub function_span: Option<ByteRange>,
    pub confidence: Option<AttributionConfidence>,
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
            resolved_file: None,
            export_specifier: None,
            emission_mode,
            status: PackageAttributionStatus::Proposed,
            rejection_reason: None,
            function_span: None,
            confidence: None,
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
            resolved_file: None,
            export_specifier: Some(export_specifier.into()),
            emission_mode: PackageEmissionMode::ExternalImport,
            status: PackageAttributionStatus::Accepted,
            rejection_reason: None,
            function_span: None,
            confidence: None,
        }
    }

    #[must_use]
    pub fn rejected_source(
        module_id: ModuleId,
        package_name: impl Into<String>,
        rejection_reason: impl Into<String>,
    ) -> Self {
        Self {
            module_id,
            package_name: package_name.into(),
            package_version: None,
            subpath: None,
            resolved_file: None,
            export_specifier: None,
            emission_mode: PackageEmissionMode::ApplicationSource,
            status: PackageAttributionStatus::Rejected,
            rejection_reason: Some(rejection_reason.into()),
            function_span: None,
            confidence: None,
        }
    }

    #[must_use]
    pub fn with_subpath(mut self, subpath: impl Into<String>) -> Self {
        self.subpath = normalize_optional(Some(subpath.into()));
        self
    }

    #[must_use]
    pub fn with_resolved_file(mut self, resolved_file: impl Into<String>) -> Self {
        self.resolved_file = normalize_optional(Some(resolved_file.into()));
        self
    }

    #[must_use]
    pub fn with_function_span(mut self, span: ByteRange) -> Self {
        self.function_span = Some(span);
        self
    }

    #[must_use]
    pub fn with_confidence(mut self, conf: AttributionConfidence) -> Self {
        self.confidence = Some(conf);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageSurfaceInput {
    pub package_name: String,
    pub package_version: Option<String>,
    pub export_specifier: String,
    pub status: PackageAttributionStatus,
    pub evidence: Option<String>,
}

impl PackageSurfaceInput {
    #[must_use]
    pub fn accepted_external(
        package_name: impl Into<String>,
        package_version: impl Into<String>,
        export_specifier: impl Into<String>,
    ) -> Self {
        Self {
            package_name: package_name.into(),
            package_version: Some(package_version.into()),
            export_specifier: export_specifier.into(),
            status: PackageAttributionStatus::Accepted,
            evidence: None,
        }
    }

    #[must_use]
    pub fn with_evidence(mut self, evidence: impl Into<String>) -> Self {
        self.evidence = normalize_optional(Some(evidence.into()));
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct InputBundle {
    pub project: ProjectInput,
    pub source_files: Vec<SourceFileInput>,
    pub assets: Vec<AssetInput>,
    pub modules: Vec<ModuleInput>,
    pub symbols: Vec<SymbolInput>,
    pub dependencies: Vec<ModuleDependencyInput>,
    pub package_attributions: Vec<PackageAttributionInput>,
    pub package_surfaces: Vec<PackageSurfaceInput>,
}

impl InputBundle {
    /// Remove module-dependency edges that leak across bundle boundaries: a
    /// scope-hoisted bundle module (its source file holds MANY sliced modules)
    /// wired to a DIFFERENT source file. esbuild emits a separate scope-hoisted
    /// bundle per output (Electron main vs each renderer), and a free reference
    /// inside one bundle resolves WITHIN that bundle's file — an edge to another
    /// file is a cross-bundle leak (e.g. a main module pulling a renderer
    /// `ion-dist` chunk that uses `document` into the Node main process). A
    /// single-module file is a standalone module whose cross-file edges are
    /// genuine explicit imports, so it is left untouched.
    ///
    /// Must run BEFORE graph construction (the ImportExport graph records module
    /// imports from these edges), so the planner never resolves a binding to a
    /// foreign bundle.
    pub fn strip_cross_bundle_dependencies(&mut self) {
        use std::collections::BTreeMap;
        let file_of: BTreeMap<ModuleId, Option<u32>> = self
            .modules
            .iter()
            .map(|module| (module.id, module.source_file_id))
            .collect();
        let mut modules_per_file: BTreeMap<u32, usize> = BTreeMap::new();
        for file_id in file_of.values().flatten() {
            *modules_per_file.entry(*file_id).or_insert(0) += 1;
        }
        self.dependencies.retain(|dependency| {
            let ModuleDependencyTarget::Module(target) = dependency.target else {
                return true;
            };
            let Some(from_file) = file_of.get(&dependency.from_module_id).copied().flatten() else {
                return true;
            };
            if modules_per_file.get(&from_file).copied().unwrap_or(0) <= 1 {
                return true;
            }
            file_of.get(&target).copied().flatten() == Some(from_file)
        });
    }

    pub fn from_rows(rows: InputRows) -> Result<Self, InputBundleError> {
        validate_project(&rows.project)?;
        let source_file_ids = collect_source_file_ids(&rows.source_files)?;
        let module_ids = collect_module_ids(&rows.modules)?;
        validate_assets(&rows.assets)?;
        validate_modules(&rows.modules, &rows.source_files, &source_file_ids)?;
        validate_symbols(&rows.symbols, &module_ids)?;
        validate_dependencies(&rows.dependencies, &module_ids)?;
        validate_package_attributions(&rows.modules, &rows.package_attributions, &module_ids)?;
        validate_package_surfaces(&rows.package_surfaces)?;

        Ok(Self {
            project: rows.project,
            source_files: rows.source_files,
            assets: rows.assets,
            modules: rows.modules,
            symbols: rows.symbols,
            dependencies: rows.dependencies,
            package_attributions: rows.package_attributions,
            package_surfaces: rows.package_surfaces,
        })
    }

    pub fn from_database_rows(rows: DatabaseRows) -> Result<Self, InputBundleError> {
        Self::from_rows(InputRows::from_database_rows(rows)?)
    }

    #[must_use]
    pub fn module_source_slice(&self, module_id: ModuleId) -> Option<ModuleSourceSlice<'_>> {
        module_source_slice_from_parts(&self.modules, &self.source_files, module_id)
    }

    #[must_use]
    pub fn module_ids(&self) -> BTreeSet<ModuleId> {
        self.modules.iter().map(|module| module.id).collect()
    }

    /// Append additional modules to an already-validated bundle without
    /// re-running validation on the existing rows. Validates only the new
    /// modules: id uniqueness against the existing set, FK to a known
    /// `source_file_id` (when set), and `source_span` consistency.
    pub fn with_appended_modules(
        mut self,
        modules: Vec<ModuleInput>,
    ) -> Result<Self, InputBundleError> {
        if modules.is_empty() {
            return Ok(self);
        }
        let mut existing_ids: BTreeSet<ModuleId> = self.modules.iter().map(|m| m.id).collect();
        let source_file_ids: BTreeSet<u32> = self.source_files.iter().map(|sf| sf.id).collect();
        for module in &modules {
            ensure_non_empty(module.original_name.as_str(), "module.original_name")?;
            ensure_non_empty(module.semantic_path.as_str(), "module.semantic_path")?;
            if !existing_ids.insert(module.id) {
                return Err(InputBundleError::DuplicateModuleId(module.id));
            }
            if let Some(source_file_id) = module.source_file_id
                && !source_file_ids.contains(&source_file_id)
            {
                return Err(InputBundleError::UnknownSourceFile {
                    source_file_id,
                    owner: "module",
                });
            }
            if let Some(source_span) = module.source_span {
                if source_span.byte_end <= source_span.byte_start {
                    return Err(invalid_source_span(module.id, source_span));
                }
                let Some(source_file_id) = module.source_file_id else {
                    return Err(invalid_source_span(module.id, source_span));
                };
                if let Some(source_file) =
                    self.source_files.iter().find(|sf| sf.id == source_file_id)
                {
                    let Some(source) = source_file.source.as_deref() else {
                        return Err(invalid_source_span(module.id, source_span));
                    };
                    if source
                        .get(source_span.byte_start as usize..source_span.byte_end as usize)
                        .is_none()
                    {
                        return Err(invalid_source_span(module.id, source_span));
                    }
                }
            }
        }
        self.modules.extend(modules);
        Ok(self)
    }

    /// Append additional source files to an already-validated bundle without
    /// re-running validation on existing rows. Used for in-memory SYNTHETIC
    /// source files — reconstructed esbuild multi-handle modules whose content
    /// is generated rather than read from disk (they carry `source: Some(_)`).
    /// Validates only id uniqueness against the existing set; a module that
    /// later spans a content-less source file is still rejected by
    /// `with_appended_modules`' span check.
    pub fn with_appended_source_files(
        mut self,
        source_files: Vec<SourceFileInput>,
    ) -> Result<Self, InputBundleError> {
        if source_files.is_empty() {
            return Ok(self);
        }
        let mut existing_ids: BTreeSet<u32> = self
            .source_files
            .iter()
            .map(|source_file| source_file.id)
            .collect();
        for source_file in &source_files {
            if !existing_ids.insert(source_file.id) {
                return Err(InputBundleError::DuplicateSourceFileId(source_file.id));
            }
        }
        self.source_files.extend(source_files);
        Ok(self)
    }

    #[must_use]
    pub fn into_rows(self) -> InputRows {
        InputRows {
            project: self.project,
            source_files: self.source_files,
            assets: self.assets,
            modules: self.modules,
            symbols: self.symbols,
            dependencies: self.dependencies,
            package_attributions: self.package_attributions,
            package_surfaces: self.package_surfaces,
        }
    }
}

impl From<InputBundle> for InputRows {
    fn from(bundle: InputBundle) -> Self {
        bundle.into_rows()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct InputRows {
    pub project: ProjectInput,
    pub source_files: Vec<SourceFileInput>,
    pub assets: Vec<AssetInput>,
    pub modules: Vec<ModuleInput>,
    pub symbols: Vec<SymbolInput>,
    pub dependencies: Vec<ModuleDependencyInput>,
    pub package_attributions: Vec<PackageAttributionInput>,
    pub package_surfaces: Vec<PackageSurfaceInput>,
}

impl InputRows {
    #[must_use]
    pub fn new(project: ProjectInput) -> Self {
        Self {
            project,
            source_files: Vec::new(),
            assets: Vec::new(),
            modules: Vec::new(),
            symbols: Vec::new(),
            dependencies: Vec::new(),
            package_attributions: Vec::new(),
            package_surfaces: Vec::new(),
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

        let mut assets = Vec::with_capacity(rows.assets.len());
        for row in rows.assets {
            let id = checked_u32_id(row.id, "asset.id")?;
            let row_project_id = checked_u32_id(row.project_id, "asset.project_id")?;
            if row_project_id != project_id {
                return Err(InputBundleError::AssetProjectMismatch {
                    asset_id: id,
                    project_id: row_project_id,
                    expected_project_id: project_id,
                });
            }
            let kind = AssetKind::from_storage(row.kind.as_str()).ok_or(
                InputBundleError::InvalidAssetKind {
                    asset_id: id,
                    kind: row.kind,
                },
            )?;
            assets.push(AssetInput {
                id,
                logical_path: non_empty(row.logical_path, "asset.logical_path")?,
                output_path: non_empty(row.output_path, "asset.output_path")?,
                source_path: normalize_optional(row.source_path),
                bytes: row.bytes,
                kind,
                executable: row.executable,
                platform: normalize_optional(row.platform),
                arch: normalize_optional(row.arch),
            });
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
            let source_span = source_span_from_row(row.byte_start, row.byte_end, id)?;
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
                source_span,
                package_name: normalize_optional(row.package_name),
                package_version: normalize_optional(row.package_version),
            });
        }

        let mut symbols = Vec::with_capacity(rows.symbols.len());
        for row in rows.symbols {
            symbols.push(SymbolInput {
                module_id: ModuleId(checked_u32_id(row.module_id, "symbol.module_id")?),
                name: non_empty(row.name, "symbol.name")?,
                semantic_name: normalize_optional(row.semantic_name),
                export_name: normalize_optional(row.export_name),
                scope: symbol_scope_from_storage(row.scope_level.as_deref()),
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
                resolved_file: normalize_optional(row.resolved_file),
                export_specifier: normalize_optional(row.export_specifier),
                emission_mode: row.emission_mode,
                status: row.status,
                rejection_reason: normalize_optional(row.rejection_reason),
                function_span: None,
                confidence: None,
            });
        }

        let mut package_surfaces = Vec::with_capacity(rows.package_surfaces.len());
        for row in rows.package_surfaces {
            let row_project_id = checked_u32_id(row.project_id, "package_surface.project_id")?;
            if row_project_id != project_id {
                return Err(InputBundleError::PackageSurfaceProjectMismatch {
                    project_id: row_project_id,
                    expected_project_id: project_id,
                    export_specifier: row.export_specifier,
                });
            }
            package_surfaces.push(PackageSurfaceInput {
                package_name: non_empty(row.package_name, "package_surface.package_name")?,
                package_version: normalize_optional(row.package_version),
                export_specifier: non_empty(
                    row.export_specifier,
                    "package_surface.export_specifier",
                )?,
                status: row.status,
                evidence: normalize_optional(row.evidence),
            });
        }

        Ok(Self {
            project,
            source_files,
            assets,
            modules,
            symbols,
            dependencies,
            package_attributions,
            package_surfaces,
        })
    }

    #[must_use]
    pub fn module_source_slice(&self, module_id: ModuleId) -> Option<ModuleSourceSlice<'_>> {
        module_source_slice_from_parts(&self.modules, &self.source_files, module_id)
    }
}

#[must_use]
pub fn module_source_slice_from_parts<'a>(
    modules: &'a [ModuleInput],
    source_files: &'a [SourceFileInput],
    module_id: ModuleId,
) -> Option<ModuleSourceSlice<'a>> {
    let module = modules.iter().find(|module| module.id == module_id)?;
    let source_file_id = module.source_file_id?;
    let source_file = source_files
        .iter()
        .find(|source_file| source_file.id == source_file_id)?;
    let source = source_file.source.as_deref()?;

    let (source, span) = if let Some(span) = module.source_span {
        (
            source.get(span.byte_start as usize..span.byte_end as usize)?,
            Some(span),
        )
    } else {
        let module_count_for_source = modules
            .iter()
            .filter(|candidate| candidate.source_file_id == Some(source_file_id))
            .count();
        if module_count_for_source != 1 {
            return None;
        }
        (source, None)
    };

    Some(ModuleSourceSlice {
        module_id,
        source_file_id,
        source_file_path: source_file.path.as_str(),
        source,
        span,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseRows {
    pub project: ProjectRow,
    pub source_files: Vec<SourceFileRow>,
    pub assets: Vec<AssetRow>,
    pub modules: Vec<ModuleRow>,
    pub symbols: Vec<SymbolRow>,
    pub dependencies: Vec<ModuleDependencyRow>,
    pub package_attributions: Vec<PackageAttributionRow>,
    pub package_surfaces: Vec<PackageSurfaceRow>,
}

impl DatabaseRows {
    #[must_use]
    pub fn new(project: ProjectRow) -> Self {
        Self {
            project,
            source_files: Vec::new(),
            assets: Vec::new(),
            modules: Vec::new(),
            symbols: Vec::new(),
            dependencies: Vec::new(),
            package_attributions: Vec::new(),
            package_surfaces: Vec::new(),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetRow {
    pub id: i64,
    pub project_id: i64,
    pub logical_path: String,
    pub output_path: String,
    pub source_path: Option<String>,
    pub bytes: Vec<u8>,
    pub kind: String,
    pub executable: bool,
    pub platform: Option<String>,
    pub arch: Option<String>,
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

fn symbol_scope_from_storage(value: Option<&str>) -> SymbolScope {
    match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("global") => SymbolScope::Global,
        _ => SymbolScope::Module,
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
    pub byte_start: Option<i64>,
    pub byte_end: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRow {
    pub module_id: i64,
    pub name: String,
    pub semantic_name: Option<String>,
    pub export_name: Option<String>,
    pub scope_level: Option<String>,
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
    pub resolved_file: Option<String>,
    pub export_specifier: Option<String>,
    pub emission_mode: PackageEmissionMode,
    pub status: PackageAttributionStatus,
    pub rejection_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageSurfaceRow {
    pub project_id: i64,
    pub package_name: String,
    pub package_version: Option<String>,
    pub export_specifier: String,
    pub status: PackageAttributionStatus,
    pub evidence: Option<String>,
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
    AssetProjectMismatch {
        asset_id: u32,
        project_id: u32,
        expected_project_id: u32,
    },
    DuplicateAssetId(u32),
    DuplicateAssetLogicalPath(String),
    DuplicateAssetOutputPath(String),
    InvalidAssetKind {
        asset_id: u32,
        kind: String,
    },
    UnsafeAssetOutputPath {
        asset_id: u32,
        output_path: String,
    },
    DuplicateModuleId(ModuleId),
    UnknownSourceFile {
        source_file_id: u32,
        owner: &'static str,
    },
    UnsupportedModuleKind {
        module_id: ModuleId,
    },
    InvalidSourceSpan {
        module_id: ModuleId,
        byte_start: i64,
        byte_end: i64,
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
    ProposedPackageAttribution {
        module_id: ModuleId,
        package_name: String,
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
    PackageSurfaceProjectMismatch {
        project_id: u32,
        expected_project_id: u32,
        export_specifier: String,
    },
    InvalidPackageSurfaceSpecifier {
        package_name: String,
        export_specifier: String,
    },
    MissingPackageSurfaceVersion {
        package_name: String,
        export_specifier: String,
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
            Self::AssetProjectMismatch {
                asset_id,
                project_id,
                expected_project_id,
            } => write!(
                formatter,
                "asset {asset_id} belongs to project {project_id}, expected {expected_project_id}"
            ),
            Self::DuplicateAssetId(id) => write!(formatter, "duplicate asset id {id}"),
            Self::DuplicateAssetLogicalPath(path) => {
                write!(formatter, "duplicate asset logical path {path}")
            }
            Self::DuplicateAssetOutputPath(path) => {
                write!(formatter, "duplicate asset output path {path}")
            }
            Self::InvalidAssetKind { asset_id, kind } => {
                write!(formatter, "asset {asset_id} has invalid kind {kind}")
            }
            Self::UnsafeAssetOutputPath {
                asset_id,
                output_path,
            } => write!(
                formatter,
                "asset {asset_id} output path is not safe: {output_path}"
            ),
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
            Self::InvalidSourceSpan {
                module_id,
                byte_start,
                byte_end,
            } => write!(
                formatter,
                "module {} has invalid source span {byte_start}..{byte_end}",
                module_id.0
            ),
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
            Self::ProposedPackageAttribution {
                module_id,
                package_name,
            } => write!(
                formatter,
                "package attribution for module {} package {package_name} is still proposed",
                module_id.0
            ),
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
            Self::PackageSurfaceProjectMismatch {
                project_id,
                expected_project_id,
                export_specifier,
            } => write!(
                formatter,
                "package surface {export_specifier} belongs to project {project_id}, expected {expected_project_id}"
            ),
            Self::InvalidPackageSurfaceSpecifier {
                package_name,
                export_specifier,
            } => write!(
                formatter,
                "package surface {export_specifier} does not belong to package {package_name}"
            ),
            Self::MissingPackageSurfaceVersion {
                package_name,
                export_specifier,
            } => write!(
                formatter,
                "accepted package surface {export_specifier} for package {package_name} has no version"
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

fn validate_assets(assets: &[AssetInput]) -> Result<(), InputBundleError> {
    let mut ids = BTreeSet::new();
    let mut logical_paths = BTreeSet::new();
    let mut output_paths = BTreeSet::new();
    for asset in assets {
        ensure_non_empty(asset.logical_path.as_str(), "asset.logical_path")?;
        ensure_non_empty(asset.output_path.as_str(), "asset.output_path")?;
        if let Some(source_path) = &asset.source_path {
            ensure_non_empty(source_path.as_str(), "asset.source_path")?;
        }
        if let Some(platform) = &asset.platform {
            ensure_non_empty(platform.as_str(), "asset.platform")?;
        }
        if let Some(arch) = &asset.arch {
            ensure_non_empty(arch.as_str(), "asset.arch")?;
        }
        if !ids.insert(asset.id) {
            return Err(InputBundleError::DuplicateAssetId(asset.id));
        }
        if !logical_paths.insert(asset.logical_path.clone()) {
            return Err(InputBundleError::DuplicateAssetLogicalPath(
                asset.logical_path.clone(),
            ));
        }
        if !output_paths.insert(asset.output_path.clone()) {
            return Err(InputBundleError::DuplicateAssetOutputPath(
                asset.output_path.clone(),
            ));
        }
        if !is_safe_relative_path(asset.output_path.as_str()) {
            return Err(InputBundleError::UnsafeAssetOutputPath {
                asset_id: asset.id,
                output_path: asset.output_path.clone(),
            });
        }
    }
    Ok(())
}

fn is_safe_relative_path(value: &str) -> bool {
    let path = Path::new(value);
    if path.as_os_str().is_empty() || path.is_absolute() {
        return false;
    }

    let mut has_normal_component = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => has_normal_component = true,
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    has_normal_component
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
    source_files: &[SourceFileInput],
    source_file_ids: &BTreeSet<u32>,
) -> Result<(), InputBundleError> {
    let source_files_by_id: BTreeMap<u32, &SourceFileInput> = source_files
        .iter()
        .map(|source_file| (source_file.id, source_file))
        .collect();

    for module in modules {
        if let Some(source_file_id) = module.source_file_id
            && !source_file_ids.contains(&source_file_id)
        {
            return Err(InputBundleError::UnknownSourceFile {
                source_file_id,
                owner: "module",
            });
        }

        let Some(source_span) = module.source_span else {
            continue;
        };
        if source_span.byte_end <= source_span.byte_start {
            return Err(invalid_source_span(module.id, source_span));
        }
        let Some(source_file_id) = module.source_file_id else {
            return Err(invalid_source_span(module.id, source_span));
        };
        let Some(source_file) = source_files_by_id.get(&source_file_id) else {
            continue;
        };
        let Some(source) = source_file.source.as_deref() else {
            return Err(invalid_source_span(module.id, source_span));
        };
        if source
            .get(source_span.byte_start as usize..source_span.byte_end as usize)
            .is_none()
        {
            return Err(invalid_source_span(module.id, source_span));
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
        if let Some(semantic_name) = &symbol.semantic_name {
            ensure_non_empty(semantic_name.as_str(), "symbol.semantic_name")?;
        }
        if let Some(export_name) = &symbol.export_name {
            ensure_non_empty(export_name.as_str(), "symbol.export_name")?;
        }
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
        PackageAttributionStatus::Proposed => {
            return Err(InputBundleError::ProposedPackageAttribution {
                module_id: attribution.module_id,
                package_name: attribution.package_name.clone(),
            });
        }
    }

    Ok(())
}

fn validate_package_surfaces(surfaces: &[PackageSurfaceInput]) -> Result<(), InputBundleError> {
    for surface in surfaces {
        validate_package_surface(surface)?;
    }
    Ok(())
}

fn validate_package_surface(surface: &PackageSurfaceInput) -> Result<(), InputBundleError> {
    if !is_valid_package_name(&surface.package_name) {
        return Err(InputBundleError::InvalidPackageName(
            surface.package_name.clone(),
        ));
    }

    let Some((resolved_package, _subpath)) = split_bare_specifier(&surface.export_specifier) else {
        return Err(InputBundleError::InvalidPackageSpecifier(
            surface.export_specifier.clone(),
        ));
    };
    if resolved_package != surface.package_name {
        return Err(InputBundleError::InvalidPackageSurfaceSpecifier {
            package_name: surface.package_name.clone(),
            export_specifier: surface.export_specifier.clone(),
        });
    }

    if surface.status == PackageAttributionStatus::Accepted
        && surface.package_version.as_deref().is_none_or(str::is_empty)
    {
        return Err(InputBundleError::MissingPackageSurfaceVersion {
            package_name: surface.package_name.clone(),
            export_specifier: surface.export_specifier.clone(),
        });
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

fn source_span_from_row(
    byte_start: Option<i64>,
    byte_end: Option<i64>,
    module_id: ModuleId,
) -> Result<Option<SourceSpan>, InputBundleError> {
    match (byte_start, byte_end) {
        (Some(start), Some(end)) if start == 0 && end == 0 => Ok(None),
        (Some(start), Some(end)) if start >= 0 && end > start => {
            let byte_start =
                u32::try_from(start).map_err(|_error| InputBundleError::InvalidSourceSpan {
                    module_id,
                    byte_start: start,
                    byte_end: end,
                })?;
            let byte_end =
                u32::try_from(end).map_err(|_error| InputBundleError::InvalidSourceSpan {
                    module_id,
                    byte_start: start,
                    byte_end: end,
                })?;
            Ok(Some(SourceSpan::new(byte_start, byte_end)))
        }
        (None, None) => Ok(None),
        (Some(start), Some(end)) => Err(InputBundleError::InvalidSourceSpan {
            module_id,
            byte_start: start,
            byte_end: end,
        }),
        (Some(start), None) => Err(InputBundleError::InvalidSourceSpan {
            module_id,
            byte_start: start,
            byte_end: -1,
        }),
        (None, Some(end)) => Err(InputBundleError::InvalidSourceSpan {
            module_id,
            byte_start: -1,
            byte_end: end,
        }),
    }
}

fn invalid_source_span(module_id: ModuleId, source_span: SourceSpan) -> InputBundleError {
    InputBundleError::InvalidSourceSpan {
        module_id,
        byte_start: i64::from(source_span.byte_start),
        byte_end: i64::from(source_span.byte_end),
    }
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
        AssetInput, AssetKind, AssetRow, DatabaseRows, InputBundle, InputBundleError, InputRows,
        ModuleInput, ModuleRow, PackageAttributionInput, PackageAttributionRow,
        PackageAttributionStatus, PackageEmissionMode, PackageSurfaceInput, PackageSurfaceRow,
        ProjectInput, ProjectRow, SourceFileInput, SourceFileRow, SourceSpan, StoredModuleKind,
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
        rows.symbols.push(SymbolInput::new(ModuleId(10), "main"));

        let bundle = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        assert_eq!(bundle.project.name, "fixture");
        assert!(bundle.module_ids().contains(&ModuleId(10)));
    }

    #[test]
    fn rows_preserve_binary_assets_in_input_bundle() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::application(
            ModuleId(10),
            "m10",
            "src/index.ts",
        ));
        rows.assets.push(
            AssetInput::new(
                100,
                "/$bunfs/root/vendor/rg",
                "modules/entry/vendor/rg",
                b"rg-binary".to_vec(),
                AssetKind::Executable,
                true,
            )
            .with_source_path("/tmp/rg")
            .with_platform("linux")
            .with_arch("x64"),
        );

        let bundle = InputBundle::from_rows(rows).expect("asset rows should be valid");

        assert_eq!(bundle.assets.len(), 1);
        assert_eq!(bundle.assets[0].logical_path, "/$bunfs/root/vendor/rg");
        assert_eq!(bundle.assets[0].output_path, "modules/entry/vendor/rg");
        assert_eq!(bundle.assets[0].bytes, b"rg-binary");
        assert_eq!(bundle.assets[0].kind, AssetKind::Executable);
        assert!(bundle.assets[0].executable);
        assert_eq!(bundle.assets[0].platform.as_deref(), Some("linux"));
        assert_eq!(bundle.assets[0].arch.as_deref(), Some("x64"));
    }

    #[test]
    fn absolute_asset_output_path_is_rejected_before_writing() {
        // The existing `unsafe_asset_output_path_is_rejected_before_writing`
        // test covers `../` escapes. An absolute path is just as dangerous:
        // it would write outside the project root. The validator must also
        // reject that case so the writer cannot be tricked into clobbering
        // arbitrary filesystem locations.
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::application(
            ModuleId(10),
            "m10",
            "src/index.ts",
        ));
        rows.assets.push(AssetInput::new(
            100,
            "vendor/rg",
            "/etc/passwd",
            Vec::new(),
            AssetKind::Executable,
            false,
        ));

        let error = InputBundle::from_rows(rows);

        assert!(
            matches!(error, Err(InputBundleError::UnsafeAssetOutputPath { .. })),
            "absolute output paths must be rejected; got: {error:?}",
        );
    }

    #[test]
    fn unsafe_asset_output_path_is_rejected_before_writing() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::application(
            ModuleId(10),
            "m10",
            "src/index.ts",
        ));
        rows.assets.push(AssetInput::new(
            100,
            "vendor/rg",
            "../escape/rg",
            Vec::new(),
            AssetKind::Executable,
            true,
        ));

        let error = InputBundle::from_rows(rows);

        assert!(matches!(
            error,
            Err(InputBundleError::UnsafeAssetOutputPath {
                asset_id: 100,
                output_path,
            }) if output_path == "../escape/rg"
        ));
    }

    #[test]
    fn duplicate_asset_logical_path_is_rejected_as_ambiguous() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::application(
            ModuleId(10),
            "m10",
            "src/index.ts",
        ));
        rows.assets.push(AssetInput::new(
            100,
            "/$bunfs/root/addon.node",
            "assets/addon-linux.node",
            b"native-a".to_vec(),
            AssetKind::NativeNode,
            false,
        ));
        rows.assets.push(AssetInput::new(
            101,
            "/$bunfs/root/addon.node",
            "assets/addon-darwin.node",
            b"native-b".to_vec(),
            AssetKind::NativeNode,
            false,
        ));

        let error = InputBundle::from_rows(rows);

        assert!(matches!(
            error,
            Err(InputBundleError::DuplicateAssetLogicalPath(path))
                if path == "/$bunfs/root/addon.node"
        ));
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
            byte_start: Some(0),
            byte_end: Some(18),
        });
        rows.symbols.push(SymbolRow {
            module_id: 17,
            name: "answer".to_string(),
            semantic_name: Some("readableAnswer".to_string()),
            export_name: None,
            scope_level: Some("module".to_string()),
        });
        rows.assets.push(AssetRow {
            id: 31,
            project_id: 7,
            logical_path: "vendor/addon.node".to_string(),
            output_path: "assets/addon.node".to_string(),
            source_path: Some("/tmp/addon.node".to_string()),
            bytes: b"native".to_vec(),
            kind: "native_node".to_string(),
            executable: false,
            platform: Some("linux".to_string()),
            arch: Some("x64".to_string()),
        });

        let bundle = InputBundle::from_database_rows(rows).expect("database rows should convert");

        assert_eq!(bundle.project.id, 7);
        assert_eq!(bundle.source_files[0].id, 11);
        assert_eq!(bundle.modules[0].original_name, "m17");
        assert_eq!(bundle.modules[0].source_file_id, Some(11));
        assert_eq!(bundle.modules[0].source_span, Some(SourceSpan::new(0, 18)));
        assert_eq!(bundle.symbols[0].name, "answer");
        assert_eq!(
            bundle.symbols[0].semantic_name.as_deref(),
            Some("readableAnswer")
        );
        assert_eq!(bundle.assets[0].id, 31);
        assert_eq!(bundle.assets[0].kind, AssetKind::NativeNode);
        assert_eq!(bundle.assets[0].bytes, b"native");
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
    fn proposed_package_attribution_is_not_generation_ready() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::package(
            ModuleId(10),
            "pkg_mod",
            "node_modules/pkg/add.js",
            "pkg",
            None,
        ));
        rows.package_attributions
            .push(PackageAttributionInput::proposed(
                ModuleId(10),
                "pkg",
                Some("1.2.3".to_string()),
                PackageEmissionMode::ExternalImport,
            ));

        let error = InputBundle::from_rows(rows);

        assert!(matches!(
            error,
            Err(InputBundleError::ProposedPackageAttribution {
                module_id: ModuleId(10),
                package_name
            }) if package_name == "pkg"
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
            byte_start: None,
            byte_end: None,
        });
        rows.package_attributions.push(PackageAttributionRow {
            module_id: 10,
            package_name: "axios".to_string(),
            package_version: Some("1.6.0".to_string()),
            subpath: None,
            resolved_file: None,
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
    fn accepted_package_surface_resolves_without_package_module_attribution() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::application(
            ModuleId(10),
            "m10",
            "src/index.ts",
        ));
        rows.package_surfaces
            .push(PackageSurfaceInput::accepted_external(
                "undici", "2.2.1", "undici",
            ));

        let bundle = InputBundle::from_rows(rows).expect("surface should be valid input");

        assert_eq!(bundle.package_surfaces.len(), 1);
        assert_eq!(bundle.package_surfaces[0].package_name, "undici");
        assert_eq!(
            bundle.package_surfaces[0].package_version.as_deref(),
            Some("2.2.1")
        );
    }

    #[test]
    fn accepted_package_surface_requires_version_and_matching_specifier() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::application(
            ModuleId(10),
            "m10",
            "src/index.ts",
        ));
        rows.package_surfaces.push(PackageSurfaceInput {
            package_name: "undici".to_string(),
            package_version: None,
            export_specifier: "undici".to_string(),
            status: PackageAttributionStatus::Accepted,
            evidence: None,
        });

        let error = InputBundle::from_rows(rows);

        assert!(matches!(
            error,
            Err(InputBundleError::MissingPackageSurfaceVersion {
                package_name,
                export_specifier,
            }) if package_name == "undici" && export_specifier == "undici"
        ));

        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::application(
            ModuleId(10),
            "m10",
            "src/index.ts",
        ));
        rows.package_surfaces.push(PackageSurfaceInput {
            package_name: "undici".to_string(),
            package_version: Some("2.2.1".to_string()),
            export_specifier: "ws".to_string(),
            status: PackageAttributionStatus::Accepted,
            evidence: None,
        });

        let error = InputBundle::from_rows(rows);

        assert!(matches!(
            error,
            Err(InputBundleError::InvalidPackageSurfaceSpecifier {
                package_name,
                export_specifier,
            }) if package_name == "undici" && export_specifier == "ws"
        ));
    }

    #[test]
    fn package_surface_database_project_must_match() {
        let mut rows = DatabaseRows::new(ProjectRow {
            id: 1,
            name: "fixture".to_string(),
        });
        rows.modules.push(ModuleRow {
            id: 10,
            source_file_id: None,
            original_name: "m10".to_string(),
            semantic_path: Some("src/index.ts".to_string()),
            kind: StoredModuleKind::Application,
            package_name: None,
            package_version: None,
            byte_start: None,
            byte_end: None,
        });
        rows.package_surfaces.push(PackageSurfaceRow {
            project_id: 2,
            package_name: "undici".to_string(),
            package_version: Some("2.2.1".to_string()),
            export_specifier: "undici".to_string(),
            status: PackageAttributionStatus::Accepted,
            evidence: None,
        });

        let error = InputBundle::from_database_rows(rows);

        assert!(matches!(
            error,
            Err(InputBundleError::PackageSurfaceProjectMismatch {
                project_id: 2,
                expected_project_id: 1,
                export_specifier,
            }) if export_specifier == "undici"
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
    fn module_source_span_requires_backing_source_file() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(
            ModuleInput::application(ModuleId(10), "m10", "src/index.ts")
                .with_source_span(SourceSpan::new(0, 5)),
        );

        let error = InputBundle::from_rows(rows);

        assert!(matches!(
            error,
            Err(InputBundleError::InvalidSourceSpan {
                module_id: ModuleId(10),
                byte_start: 0,
                byte_end: 5
            })
        ));
    }

    #[test]
    fn module_source_span_must_resolve_to_real_source_slice() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("export const value = 1;".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(10), "m10", "src/index.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(0, 200)),
        );

        let error = InputBundle::from_rows(rows);

        assert!(matches!(
            error,
            Err(InputBundleError::InvalidSourceSpan {
                module_id: ModuleId(10),
                byte_start: 0,
                byte_end: 200
            })
        ));
    }

    #[test]
    fn module_source_slice_uses_shared_bundle_span() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("export const one = 1;\nexport const two = 2;".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(10), "m10", "src/one.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(0, 21)),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(11), "m11", "src/two.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(22, 43)),
        );

        let slice = rows
            .module_source_slice(ModuleId(11))
            .expect("span should select module source");

        assert_eq!(slice.source_file_path, "bundle.js");
        assert_eq!(slice.source, "export const two = 2;");
        assert_eq!(slice.span, Some(SourceSpan::new(22, 43)));
    }

    #[test]
    fn appended_synthetic_source_file_resolves_module_slice() {
        // Reconstructed multi-handle esbuild modules carry SYNTHETIC source
        // (not a slice of the original bundle file). It lives in-memory only:
        // a synthetic source file is appended to the bundle, and the
        // per-handle module's span points into it. `module_source_slice` must
        // return that synthetic text so the planner lowers it like any
        // single-handle module.
        let synthetic = "var a, X = st(() => { a = 1; });";
        let bundle = InputBundle::from_rows(InputRows::new(ProjectInput::new(1, "fixture")))
            .expect("empty bundle is valid")
            .with_appended_source_files(vec![SourceFileInput::new(
                900,
                "__reverts_synthetic__/esbuild-X.js",
                Some(synthetic.to_string()),
            )])
            .expect("synthetic source file appends")
            .with_appended_modules(vec![
                ModuleInput::application(ModuleId(5000), "esbuild:X", "esbuild:X")
                    .with_source_file(900)
                    .with_source_span(SourceSpan::new(0, synthetic.len() as u32)),
            ])
            .expect("module into synthetic file appends");

        let slice = bundle
            .module_source_slice(ModuleId(5000))
            .expect("synthetic module resolves");
        assert_eq!(slice.source, synthetic);
    }

    #[test]
    fn shared_source_without_span_has_no_module_slice() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("let x = 1;".to_string()),
        ));
        rows.modules
            .push(ModuleInput::application(ModuleId(10), "m10", "src/one.ts").with_source_file(1));
        rows.modules
            .push(ModuleInput::application(ModuleId(11), "m11", "src/two.ts").with_source_file(1));

        assert!(rows.module_source_slice(ModuleId(10)).is_none());
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
            byte_start: None,
            byte_end: None,
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
            source_span: None,
            package_name: Some("lodash".to_string()),
            package_version: Some("4.17.21".to_string()),
        });
        rows.package_attributions.push(PackageAttributionInput {
            module_id: ModuleId(10),
            package_name: "lodash".to_string(),
            package_version: Some("4.17.21".to_string()),
            subpath: Some("fp.js".to_string()),
            resolved_file: None,
            export_specifier: None,
            emission_mode: PackageEmissionMode::VendoredAsset,
            status: PackageAttributionStatus::Accepted,
            rejection_reason: None,
            function_span: None,
            confidence: None,
        });

        let bundle = InputBundle::from_rows(rows).expect("vendored attribution should be valid");

        assert_eq!(
            bundle.package_attributions[0].emission_mode,
            PackageEmissionMode::VendoredAsset
        );
    }

    #[test]
    fn package_attribution_function_span_defaults_to_none() {
        let attr = PackageAttributionInput::accepted_external(
            reverts_ir::ModuleId(1),
            "pkg",
            "1.0",
            "pkg",
        );
        assert!(attr.function_span.is_none());
        assert!(attr.confidence.is_none());
    }

    #[test]
    fn package_attribution_with_function_span_round_trips() {
        let attr = PackageAttributionInput::accepted_external(
            reverts_ir::ModuleId(1),
            "pkg",
            "1.0",
            "pkg",
        )
        .with_function_span(reverts_ir::ByteRange::new(10, 30));
        assert_eq!(attr.function_span, Some(reverts_ir::ByteRange::new(10, 30)));
    }
}
