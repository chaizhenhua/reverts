use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use reverts_graph::{
    RevertsGraph, RuntimeEntrypoint, RuntimeNamespaceExport, RuntimePrelude,
    RuntimePreludeBindingKind, RuntimePreludeImport,
};
use reverts_input::{
    ModuleDependencyTarget, PackageAttributionInput, PackageAttributionStatus, PackageEmissionMode,
};
use reverts_ir::{BindingName, BindingShape, ModuleId, ModuleKind};
use reverts_js::{
    ImportUsageScope, ParseGoal, classify_import_usage_scope, collect_identifier_read_facts,
    format_source_pretty, is_ascii_identifier_continue as is_identifier_continue,
    is_ascii_identifier_start as is_identifier_start, is_js_keyword, parse_error_message,
    sanitize_identifier, skip_block_comment, skip_line_comment,
    verify_only_immediate_call_references,
};
use reverts_model::{CompilerEvidence, CompilerKind, EnrichedProgram, ModuleCompilerProfile};
use reverts_package::PackageResolution;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EmitPlan {
    pub files: Vec<PlannedFile>,
}

impl EmitPlan {
    pub fn push_file(&mut self, mut file: PlannedFile) {
        file.coalesce_consecutive_uninitialized_var_declarations();
        file.coalesce_generated_named_imports();
        file.coalesce_generated_named_exports();
        self.files.push(file);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub path: String,
    pub imports: Vec<PlannedImport>,
    pub bindings: Vec<PlannedBinding>,
    pub exports: Vec<PlannedExport>,
    /// Late, readability-only binding renames. These are applied by the
    /// emitter after all source recovery/lowering is complete but before
    /// final codegen and parse audit, so graph/planner facts stay keyed by
    /// original recovered names.
    pub readability_renames: Vec<PlannedRename>,
    pub body: Vec<String>,
    pub compiler_recovery: CompilerRecoveryDecision,
}

impl PlannedFile {
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            imports: Vec::new(),
            bindings: Vec::new(),
            exports: Vec::new(),
            readability_renames: Vec::new(),
            body: Vec::new(),
            compiler_recovery: CompilerRecoveryDecision::default(),
        }
    }

    pub fn add_import(&mut self, import: PlannedImport) {
        self.imports.push(import);
    }

    pub fn add_binding(&mut self, binding: PlannedBinding) {
        self.bindings.push(binding);
    }

    pub fn add_export(&mut self, binding: BindingName) {
        self.add_export_with_source_backed(binding, false);
    }

    pub fn add_export_with_source_backed(&mut self, binding: BindingName, source_backed: bool) {
        self.exports.push(PlannedExport {
            binding,
            source_backed,
        });
    }

    pub fn push_source(&mut self, source: impl Into<String>) {
        self.body.push(source.into());
    }

    pub fn add_readability_rename(&mut self, rename: PlannedRename) {
        self.readability_renames.push(rename);
    }

    pub fn set_compiler_recovery(&mut self, compiler_recovery: CompilerRecoveryDecision) {
        self.compiler_recovery = compiler_recovery;
    }

    fn coalesce_generated_named_imports(&mut self) {
        let mut imports_by_specifier = BTreeMap::<String, BTreeSet<BindingName>>::new();
        let mut first_index_by_specifier = BTreeMap::<String, usize>::new();
        let mut duplicate_indices = BTreeSet::<usize>::new();
        for (index, source) in self.body.iter().enumerate() {
            let Some((bindings, specifier)) = parse_generated_named_import_statement(source) else {
                continue;
            };
            imports_by_specifier
                .entry(specifier.clone())
                .or_default()
                .extend(bindings);
            use std::collections::btree_map::Entry;
            match first_index_by_specifier.entry(specifier) {
                Entry::Vacant(entry) => {
                    entry.insert(index);
                }
                Entry::Occupied(_) => {
                    duplicate_indices.insert(index);
                }
            }
        }
        if duplicate_indices.is_empty() {
            return;
        }
        let mut replacements = BTreeMap::<usize, String>::new();
        for (specifier, index) in first_index_by_specifier {
            let Some(bindings) = imports_by_specifier.get(&specifier) else {
                continue;
            };
            replacements.insert(
                index,
                named_import_statement(bindings.iter(), specifier.as_str()),
            );
        }
        let mut merged =
            Vec::with_capacity(self.body.len().saturating_sub(duplicate_indices.len()));
        for (index, source) in self.body.iter().enumerate() {
            if duplicate_indices.contains(&index) {
                continue;
            }
            if let Some(replacement) = replacements.get(&index) {
                merged.push(replacement.clone());
            } else {
                merged.push(source.clone());
            }
        }
        self.body = merged;
    }

    fn coalesce_generated_named_exports(&mut self) {
        let mut exported_bindings = BTreeSet::<BindingName>::new();
        let mut first_index = None::<usize>;
        let mut duplicate_indices = BTreeSet::<usize>::new();
        for (index, source) in self.body.iter().enumerate() {
            let Some(bindings) = parse_generated_named_export_statement(source) else {
                continue;
            };
            exported_bindings.extend(bindings);
            if first_index.is_none() {
                first_index = Some(index);
            } else {
                duplicate_indices.insert(index);
            }
        }
        if duplicate_indices.is_empty() {
            return;
        }
        let Some(first_index) = first_index else {
            return;
        };
        let replacement = named_export_statement(exported_bindings.iter());
        let mut merged =
            Vec::with_capacity(self.body.len().saturating_sub(duplicate_indices.len()));
        for (index, source) in self.body.iter().enumerate() {
            if duplicate_indices.contains(&index) {
                continue;
            }
            if index == first_index {
                merged.push(replacement.clone());
            } else {
                merged.push(source.clone());
            }
        }
        self.body = merged;
    }

    fn coalesce_consecutive_uninitialized_var_declarations(&mut self) {
        for source in &mut self.body {
            *source = coalesce_consecutive_uninitialized_var_declarations(source);
        }
    }

    #[must_use]
    pub const fn source_strategy(&self) -> SourceCompilerStrategy {
        self.compiler_recovery.strategy
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedImport {
    pub namespace: BindingName,
    pub resolution: PackageResolution,
    pub source_backed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedRename {
    pub original: BindingName,
    pub renamed: BindingName,
}

impl PlannedRename {
    #[must_use]
    pub fn new(original: BindingName, renamed: BindingName) -> Self {
        Self { original, renamed }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedBinding {
    pub original: BindingName,
    pub emitted: BindingName,
    pub shape: BindingShape,
    pub source_backed: bool,
    /// Paper #7 downstream: property names observed on this binding when
    /// shape is `NamespaceObject`. Empty for every other shape and for
    /// namespaces whose members the solver could not see.
    pub known_members: BTreeSet<BindingName>,
}

impl PlannedBinding {
    #[must_use]
    pub fn new(
        original: BindingName,
        emitted: BindingName,
        shape: BindingShape,
        source_backed: bool,
    ) -> Self {
        Self {
            original,
            emitted,
            shape,
            source_backed,
            known_members: BTreeSet::new(),
        }
    }

    #[must_use]
    pub fn with_known_members(mut self, known_members: BTreeSet<BindingName>) -> Self {
        self.known_members = known_members;
        self
    }
}

/// Build a `PlannedBinding` whose `shape` and `known_members` are derived
/// from the enriched program. Centralising this keeps every planner site
/// that emits a real user binding consistent — the shape comes from the
/// solver, and `known_members` are attached only for `NamespaceObject`
/// (paper #7 downstream). Sites that synthesise runtime helpers bypass
/// this and use `PlannedBinding::new` directly with their known shape.
fn plan_binding_from_program(
    program: &EnrichedProgram,
    module_id: ModuleId,
    original: BindingName,
    emitted: BindingName,
    source_backed: bool,
    shape_override: Option<BindingShape>,
) -> PlannedBinding {
    // When delazify or namespace decomposition has reshaped a binding's
    // emitted RHS, the IR-derived shape (computed against the pre-lowering
    // lazy thunk) no longer matches reality — pass `Unknown` so the audit
    // doesn't fire on a stale "Callable" classification.
    let shape =
        shape_override.unwrap_or_else(|| program.binding_shape(module_id, original.as_str()));
    let known_members = if shape == BindingShape::NamespaceObject {
        program.known_members(module_id, original.as_str())
    } else {
        BTreeSet::new()
    };
    PlannedBinding::new(original, emitted, shape, source_backed).with_known_members(known_members)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedExport {
    pub binding: BindingName,
    pub source_backed: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum SourceCompilerStrategy {
    #[default]
    DirectSource,
    WebpackRuntime,
    EsbuildHelpers,
    RollupFacade,
    BabelTranspiled,
    TerserMinified,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CompilerRecoveryAction {
    #[default]
    DirectModuleSource,
    PreserveWebpackRuntime,
    PreserveEsbuildHelpers,
    PreserveRollupFacade,
    PreserveBabelTranspiledOutput,
    PreserveTerserMinifiedOutput,
}

impl CompilerRecoveryAction {
    #[must_use]
    pub const fn from_compiler(compiler: CompilerKind) -> Self {
        match compiler {
            CompilerKind::Unknown => Self::DirectModuleSource,
            CompilerKind::Webpack => Self::PreserveWebpackRuntime,
            CompilerKind::Esbuild => Self::PreserveEsbuildHelpers,
            CompilerKind::Rollup => Self::PreserveRollupFacade,
            CompilerKind::Babel => Self::PreserveBabelTranspiledOutput,
            CompilerKind::Terser => Self::PreserveTerserMinifiedOutput,
        }
    }

    /// Short banner text that surfaces the recovery decision in the emitted
    /// source. Returns `None` for `DirectModuleSource` so untransformed user
    /// code stays banner-free.
    #[must_use]
    pub const fn recovery_banner(self) -> Option<&'static str> {
        match self {
            Self::DirectModuleSource => None,
            Self::PreserveWebpackRuntime => Some("reverts-recovery: webpack"),
            Self::PreserveEsbuildHelpers => Some("reverts-recovery: esbuild"),
            Self::PreserveRollupFacade => Some("reverts-recovery: rollup"),
            Self::PreserveBabelTranspiledOutput => Some("reverts-recovery: babel"),
            Self::PreserveTerserMinifiedOutput => Some("reverts-recovery: terser"),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CompilerRecoveryDecision {
    pub strategy: SourceCompilerStrategy,
    pub action: CompilerRecoveryAction,
    pub minified: bool,
    pub evidence: Vec<CompilerEvidence>,
}

impl CompilerRecoveryDecision {
    #[must_use]
    pub fn from_profile(profile: &ModuleCompilerProfile) -> Self {
        Self {
            strategy: SourceCompilerStrategy::from_profile(profile),
            action: CompilerRecoveryAction::from_compiler(profile.compiler),
            minified: profile.minified,
            evidence: profile.evidence.clone(),
        }
    }
}

impl SourceCompilerStrategy {
    #[must_use]
    pub fn from_profile(profile: &ModuleCompilerProfile) -> Self {
        match profile.compiler {
            CompilerKind::Unknown => Self::DirectSource,
            CompilerKind::Webpack => Self::WebpackRuntime,
            CompilerKind::Esbuild => Self::EsbuildHelpers,
            CompilerKind::Rollup => Self::RollupFacade,
            CompilerKind::Babel => Self::BabelTranspiled,
            CompilerKind::Terser => Self::TerserMinified,
        }
    }

    #[must_use]
    pub const fn parse_goal(self) -> ParseGoal {
        match self {
            Self::DirectSource => ParseGoal::TypeScript,
            Self::WebpackRuntime
            | Self::EsbuildHelpers
            | Self::RollupFacade
            | Self::BabelTranspiled
            | Self::TerserMinified => ParseGoal::JavaScript,
        }
    }

    #[must_use]
    pub fn path_hint(self, path: &str) -> Option<&std::path::Path> {
        match self {
            Self::DirectSource => Some(std::path::Path::new(path)),
            Self::WebpackRuntime
            | Self::EsbuildHelpers
            | Self::RollupFacade
            | Self::BabelTranspiled
            | Self::TerserMinified => None,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ImportExportPlanner;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RuntimeSetterMigrationBlockerReport {
    pub total_bindings: usize,
    pub accepted_bindings: usize,
    pub blocked_bindings: usize,
    pub reasons: BTreeMap<RuntimeSetterMigrationBlockerReason, usize>,
    pub binding_statuses:
        BTreeMap<RuntimeSetterMigrationBindingKey, RuntimeSetterMigrationBindingStatus>,
}

impl RuntimeSetterMigrationBlockerReport {
    pub fn add_accepted(&mut self, source_file_id: u32, binding: BindingName) {
        self.remove_existing_status(source_file_id, &binding);
        self.accepted_bindings += 1;
        self.binding_statuses.insert(
            RuntimeSetterMigrationBindingKey {
                source_file_id,
                binding,
            },
            RuntimeSetterMigrationBindingStatus::Accepted,
        );
    }

    pub fn add_reason(
        &mut self,
        source_file_id: u32,
        binding: BindingName,
        reason: RuntimeSetterMigrationBlockerReason,
    ) {
        self.remove_existing_status(source_file_id, &binding);
        self.blocked_bindings += 1;
        *self.reasons.entry(reason).or_default() += 1;
        self.binding_statuses.insert(
            RuntimeSetterMigrationBindingKey {
                source_file_id,
                binding,
            },
            RuntimeSetterMigrationBindingStatus::Blocked(reason),
        );
    }

    pub fn add(&mut self, other: &Self) {
        self.total_bindings += other.total_bindings;
        self.accepted_bindings += other.accepted_bindings;
        self.blocked_bindings += other.blocked_bindings;
        self.binding_statuses.extend(
            other
                .binding_statuses
                .iter()
                .map(|(key, status)| (key.clone(), *status)),
        );
        for (reason, count) in &other.reasons {
            *self.reasons.entry(*reason).or_default() += count;
        }
    }

    fn remove_existing_status(&mut self, source_file_id: u32, binding: &BindingName) {
        let key = RuntimeSetterMigrationBindingKey {
            source_file_id,
            binding: binding.clone(),
        };
        let Some(previous) = self.binding_statuses.remove(&key) else {
            return;
        };
        match previous {
            RuntimeSetterMigrationBindingStatus::Accepted => {
                self.accepted_bindings = self.accepted_bindings.saturating_sub(1);
            }
            RuntimeSetterMigrationBindingStatus::Blocked(reason) => {
                self.blocked_bindings = self.blocked_bindings.saturating_sub(1);
                if let Some(count) = self.reasons.get_mut(&reason) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        self.reasons.remove(&reason);
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RuntimeSetterMigrationBindingKey {
    pub source_file_id: u32,
    pub binding: BindingName,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeSetterMigrationBindingStatus {
    Accepted,
    Blocked(RuntimeSetterMigrationBlockerReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuntimeSetterMigrationBlockerReason {
    MultipleEligibleWriters,
    FoldedWriterOnly,
    ExternalizedPackageWriterOnly,
    NoEligibleWriter,
    MissingRuntimePrelude,
    InitializerNotMigratable,
    RuntimeNonSnippetRead,
    RuntimeNamespaceExportHelper,
    RuntimeNamespaceObjectBinding,
    ReaderNonSnippetUse,
    ReaderSnippetMissing,
    ReaderNotMovableShape,
    ReaderWritesRuntimeBinding,
    ReaderClosureEscapes,
    ReaderFreeBindingIndexMissing,
    ReaderReadsOtherMovableBinding,
    ReaderReadsNonRuntimeBinding,
    NamespaceTargetDifferentWriter,
    OwnerSourceMissing,
    OwnerNameConflict,
    ReaderClusterOverlapsMigratedBinding,
    NoDiagnosticStatus,
}

impl RuntimeSetterMigrationBlockerReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MultipleEligibleWriters => "multiple_eligible_writers",
            Self::FoldedWriterOnly => "folded_writer_only",
            Self::ExternalizedPackageWriterOnly => "externalized_package_writer_only",
            Self::NoEligibleWriter => "no_eligible_writer",
            Self::MissingRuntimePrelude => "missing_runtime_prelude",
            Self::InitializerNotMigratable => "initializer_not_migratable",
            Self::RuntimeNonSnippetRead => "runtime_non_snippet_read",
            Self::RuntimeNamespaceExportHelper => "runtime_namespace_export_helper",
            Self::RuntimeNamespaceObjectBinding => "runtime_namespace_object_binding",
            Self::ReaderNonSnippetUse => "reader_non_snippet_use",
            Self::ReaderSnippetMissing => "reader_snippet_missing",
            Self::ReaderNotMovableShape => "reader_not_movable_shape",
            Self::ReaderWritesRuntimeBinding => "reader_writes_runtime_binding",
            Self::ReaderClosureEscapes => "reader_closure_escapes",
            Self::ReaderFreeBindingIndexMissing => "reader_free_binding_index_missing",
            Self::ReaderReadsOtherMovableBinding => "reader_reads_other_movable_binding",
            Self::ReaderReadsNonRuntimeBinding => "reader_reads_non_runtime_binding",
            Self::NamespaceTargetDifferentWriter => "namespace_target_different_writer",
            Self::OwnerSourceMissing => "owner_source_missing",
            Self::OwnerNameConflict => "owner_name_conflict",
            Self::ReaderClusterOverlapsMigratedBinding => {
                "reader_cluster_overlaps_migrated_binding"
            }
            Self::NoDiagnosticStatus => "no_diagnostic_status",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceModuleFacts {
    candidate_reads_by_module: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    exportable_bindings_by_module: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    definition_modules_all: BTreeMap<BindingName, Option<ModuleId>>,
}

impl SourceModuleFacts {
    fn from_program(program: &EnrichedProgram) -> Self {
        let mut definition_bindings_by_module = BTreeMap::new();
        let mut exportable_bindings_by_module = BTreeMap::new();
        for module in program.model().modules() {
            let definitions = source_definition_bindings(program, module.id);
            let mut exportable = definitions.clone();
            exportable.extend(program.model().graph().ast_imports_for(module.id));
            if let Some(source) = program.model().input().module_source_slice(module.id) {
                exportable.extend(named_reexported_bindings(source.source));
            }
            definition_bindings_by_module.insert(module.id, definitions);
            exportable_bindings_by_module.insert(module.id, exportable);
        }
        let candidate_reads_by_module = candidate_source_reads_by_module_with_exportable(
            program,
            &exportable_bindings_by_module,
        );
        let definition_modules_all = unique_source_definition_modules_from_bindings(
            program,
            &BTreeSet::new(),
            &definition_bindings_by_module,
        );

        Self {
            candidate_reads_by_module,
            exportable_bindings_by_module,
            definition_modules_all,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlannerAnalysis {
    source_required_packages: BTreeSet<ModuleId>,
    externalized_packages: BTreeSet<ModuleId>,
    source_facts: SourceModuleFacts,
    source_module_wiring: SourceModuleWiring,
    lowered_runtime_sources: BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
    runtime_lazy_folds: RuntimeLazyFoldPlan,
}

impl PlannerAnalysis {
    fn from_program(program: &EnrichedProgram) -> Self {
        let source_facts = SourceModuleFacts::from_program(program);
        let accepted_externalized_packages = externalized_package_modules(program);
        let source_required_packages = source_required_package_modules(
            program,
            &accepted_externalized_packages,
            &source_facts,
        );
        let externalized_packages = accepted_externalized_packages
            .difference(&source_required_packages)
            .copied()
            .collect::<BTreeSet<_>>();
        let source_module_wiring =
            source_module_wiring(program, &externalized_packages, &source_facts);
        let eager_safe_analysis = if should_compute_cross_module_eager_safe_analysis(program) {
            compute_eager_safe_analysis(program, &source_module_wiring)
        } else {
            EagerSafeAnalysis::default()
        };
        let lowered_runtime_sources = lowered_runtime_sources(
            program,
            &source_module_wiring,
            &eager_safe_analysis,
            &externalized_packages,
        );
        let runtime_lazy_folds =
            runtime_lazy_fold_plan(program, &source_module_wiring, &lowered_runtime_sources);

        Self {
            source_required_packages,
            externalized_packages,
            source_facts,
            source_module_wiring,
            lowered_runtime_sources,
            runtime_lazy_folds,
        }
    }
}

impl ImportExportPlanner {
    #[must_use]
    pub fn runtime_setter_migration_blocker_report(
        self,
        program: &EnrichedProgram,
    ) -> RuntimeSetterMigrationBlockerReport {
        let analysis = PlannerAnalysis::from_program(program);
        runtime_setter_migration_blocker_report(
            program,
            &analysis.source_module_wiring,
            &analysis.lowered_runtime_sources,
            &analysis.runtime_lazy_folds,
            &analysis.externalized_packages,
        )
    }

    pub fn plan_enriched_program(self, program: &EnrichedProgram) -> Result<EmitPlan, PlanError> {
        let mut plan = EmitPlan::default();
        let mut used_runtime_helper_files = BTreeMap::<u32, BTreeSet<BindingName>>::new();
        let mut exported_runtime_helper_bindings = BTreeMap::<u32, BTreeSet<BindingName>>::new();
        let mut required_runtime_helper_bindings = BTreeMap::<u32, BTreeSet<BindingName>>::new();
        let mut used_runtime_helper_setters = BTreeMap::<u32, BTreeSet<BindingName>>::new();
        let mut used_lazy_module = BTreeSet::<u32>::new();
        let mut used_lazy_value = BTreeSet::<u32>::new();
        let mut exported_lazy_module = BTreeSet::<u32>::new();
        let mut exported_lazy_value = BTreeSet::<u32>::new();
        let analysis = PlannerAnalysis::from_program(program);
        let source_required_packages = &analysis.source_required_packages;
        let externalized_packages = &analysis.externalized_packages;
        let source_facts = &analysis.source_facts;
        let source_module_wiring = &analysis.source_module_wiring;
        let lowered_runtime_sources = &analysis.lowered_runtime_sources;
        let runtime_lazy_folds = &analysis.runtime_lazy_folds;
        let omitted_folded_stub_modules =
            folded_stub_modules_with_internal_consumers(runtime_lazy_folds, source_module_wiring);
        let pure_reexport_bypasses =
            pure_reexport_bypass_plan(program, source_module_wiring, externalized_packages);
        let runtime_var_migrations = compute_runtime_var_migration_plan(
            program,
            source_module_wiring,
            lowered_runtime_sources,
            runtime_lazy_folds,
            externalized_packages,
        );
        let runtime_prelude_direct_imports = runtime_prelude_direct_imports(program);
        let runtime_edge_direct_prelude_imports = runtime_edge_direct_prelude_imports(
            program,
            lowered_runtime_sources,
            runtime_lazy_folds,
            &runtime_prelude_direct_imports,
        );

        // When a lazy binding is folded into its helper module, the consumer no
        // longer carries the `lazyValue(...)`/`lazyModule(...)` call — but the
        // helper file does. Detect that case here so the helper file declares
        // and exports `lazyValue`/`lazyModule` even though no consumer needs to
        // import them.
        for (source_file_id, chunks) in &runtime_lazy_folds.chunks_by_source_file {
            for chunk in chunks {
                if chunk.source.contains("lazyModule(") {
                    used_lazy_module.insert(*source_file_id);
                }
                if chunk.source.contains("lazyValue(") {
                    used_lazy_value.insert(*source_file_id);
                }
            }
        }

        for module in program.model().modules() {
            if module.kind == ModuleKind::Package && externalized_packages.contains(&module.id) {
                continue;
            }

            let path = program
                .semantic_names()
                .module_path(module.id)
                .unwrap_or(module.semantic_path.as_str());
            let mut file = PlannedFile::new(path);
            let compiler_profile = program.compiler_profile().module(module.id);
            let compiler_recovery = CompilerRecoveryDecision::from_profile(&compiler_profile);
            file.set_compiler_recovery(compiler_recovery);
            let mut planned_bindings = BTreeSet::<BindingName>::new();

            if module.kind == ModuleKind::Package
                && source_required_packages.contains(&module.id)
                && let Some(attribution) = accepted_external_package_attribution(program, module.id)
            {
                let adapter_bindings =
                    package_adapter_export_bindings(program, module.id, source_facts);
                if !adapter_bindings.is_empty()
                    && let Some(adapter_kind) =
                        external_package_adapter_kind(program, module.id, &adapter_bindings)
                {
                    populate_external_package_adapter_file(
                        &mut file,
                        attribution,
                        &adapter_bindings,
                        adapter_kind,
                    );
                    plan.push_file(file);
                    continue;
                }
            }

            if pure_reexport_bypasses.omitted_modules.contains(&module.id) {
                continue;
            }

            if let Some(folded) = runtime_lazy_folds.modules.get(&module.id) {
                required_runtime_helper_bindings
                    .entry(folded.source_file_id)
                    .or_default()
                    .extend(folded.required_bindings.iter().cloned());
                // Phase 13: a folded module whose only in-project purpose is
                // `export { X } from runtime` is a pure forwarding shim. When
                // we can rewrite every internal consumer to import the folded
                // binding directly from the runtime helper, omit that shim
                // entirely. Modules with no internal consumers keep their stub
                // file so an explicit/source export surface is still visible.
                if omitted_folded_stub_modules.contains(&module.id) {
                    continue;
                }
                used_runtime_helper_files
                    .entry(folded.source_file_id)
                    .or_default()
                    .extend(folded.stub_exports.iter().cloned());
                exported_runtime_helper_bindings
                    .entry(folded.source_file_id)
                    .or_default()
                    .extend(folded.stub_exports.iter().cloned());
                let specifier = relative_import_specifier(
                    path,
                    runtime_helpers_path(folded.source_file_id).as_str(),
                );
                file.push_source(named_reexport_statement(
                    folded.stub_exports.iter(),
                    specifier.as_str(),
                ));
                for export in &folded.stub_exports {
                    file.add_binding(PlannedBinding::new(
                        export.clone(),
                        export.clone(),
                        BindingShape::Unknown,
                        true,
                    ));
                    file.add_export_with_source_backed(export.clone(), true);
                }
                plan.push_file(file);
                continue;
            }

            for decision in program.package_imports_for(module.id) {
                file.add_import(PlannedImport {
                    namespace: decision.namespace_binding.clone(),
                    resolution: decision.resolution.clone(),
                    source_backed: decision.source_backed,
                });
            }

            let mut has_runtime_edge_before_lazy_helpers = false;
            if let Some(module_imports) = source_module_wiring.imports_by_module.get(&module.id) {
                for (target_module_id, bindings) in module_imports {
                    let mut bindings_by_target = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
                    if let Some(redirects) = pure_reexport_bypasses.redirects.get(target_module_id)
                    {
                        for binding in bindings {
                            let effective_target =
                                redirects.get(binding).copied().unwrap_or(*target_module_id);
                            bindings_by_target
                                .entry(effective_target)
                                .or_default()
                                .insert(binding.clone());
                        }
                    } else {
                        bindings_by_target.insert(*target_module_id, bindings.clone());
                    }
                    for (effective_target_module_id, effective_bindings) in bindings_by_target {
                        if effective_target_module_id == module.id {
                            continue;
                        }
                        if let Some(folded) =
                            runtime_lazy_folds.modules.get(&effective_target_module_id)
                            && omitted_folded_stub_modules.contains(&effective_target_module_id)
                        {
                            has_runtime_edge_before_lazy_helpers = true;
                            used_runtime_helper_files
                                .entry(folded.source_file_id)
                                .or_default()
                                .extend(effective_bindings.iter().cloned());
                            exported_runtime_helper_bindings
                                .entry(folded.source_file_id)
                                .or_default()
                                .extend(effective_bindings.iter().cloned());
                            let specifier = relative_import_specifier(
                                path,
                                runtime_helpers_path(folded.source_file_id).as_str(),
                            );
                            file.push_source(named_import_statement(
                                effective_bindings.iter(),
                                specifier.as_str(),
                            ));
                            for binding in effective_bindings {
                                planned_bindings.insert(binding.clone());
                                file.add_binding(plan_binding_from_program(
                                    program,
                                    module.id,
                                    binding.clone(),
                                    binding,
                                    true,
                                    None,
                                ));
                            }
                            continue;
                        }
                        let Some(target_path) =
                            module_output_path(program, effective_target_module_id)
                        else {
                            continue;
                        };
                        let specifier = relative_import_specifier(path, target_path.as_str());
                        file.push_source(named_import_statement(
                            effective_bindings.iter(),
                            specifier.as_str(),
                        ));
                        for binding in effective_bindings {
                            planned_bindings.insert(binding.clone());
                            file.add_binding(plan_binding_from_program(
                                program,
                                module.id,
                                binding.clone(),
                                binding,
                                true,
                                None,
                            ));
                        }
                    }
                }
            }

            let runtime_imports = program.model().graph().runtime_imports_for(module.id);
            let runtime_import_groups = group_runtime_imports(runtime_imports);
            let lowered_source = lowered_runtime_sources.get(&module.id);
            let lowered_helpers = lowered_source
                .map(|source| source.lowered_helpers.clone())
                .unwrap_or_default();
            let local_source_definitions = lowered_source
                .as_ref()
                .map(|source| source.local_definitions.clone())
                .unwrap_or_default();
            let local_source_writes = lowered_source
                .as_ref()
                .map(|source| source.local_writes.clone())
                .unwrap_or_default();
            let source_imports = program.model().graph().ast_imports_for(module.id);
            let remaining_runtime_helpers = lowered_source
                .map(|source| source.remaining_helpers.clone())
                .unwrap_or_default();
            let written_runtime_helpers = lowered_source
                .map(|source| source.written_helpers.clone())
                .unwrap_or_default();
            // Phase 10b: bindings the migration plan reassigned to this
            // module. They're declared locally below (rather than imported
            // from the runtime), and their `X = value` writes stay as
            // direct same-module assignments instead of being rewritten
            // to setter calls.
            let migrated_locally: BTreeSet<BindingName> = runtime_var_migrations
                .migrations_by_owner
                .get(&module.id)
                .cloned()
                .unwrap_or_default();
            let migrated_extra_snippets =
                runtime_var_migrations.extra_snippets_for_owner(module.id);
            let migrated_extra_namespace_exports =
                runtime_var_migrations.extra_namespace_exports_for_owner(module.id);
            let migrated_extra_namespace_bindings = migrated_extra_namespace_exports
                .iter()
                .map(|(_, binding)| binding.clone())
                .collect::<BTreeSet<_>>();
            let migrated_extra_runtime_deps =
                runtime_var_migrations.extra_runtime_deps_for_owner(module.id);
            let migrated_extra_source_deps =
                runtime_var_migrations.extra_source_deps_for_owner(module.id);
            let migrated_local_bindings =
                runtime_var_migrations.local_bindings_for_owner(module.id);
            let remaining_runtime_helpers: BTreeSet<BindingName> = remaining_runtime_helpers
                .union(&migrated_extra_runtime_deps)
                .cloned()
                .collect::<BTreeSet<_>>()
                .difference(&migrated_local_bindings)
                .cloned()
                .collect();
            let written_runtime_helpers: BTreeSet<BindingName> = written_runtime_helpers
                .difference(&migrated_locally)
                .cloned()
                .collect();
            let namespace_member_rewrite = lowered_source.and_then(|source| {
                let mut reserved_bindings = local_source_definitions.clone();
                reserved_bindings.extend(source_imports.iter().cloned());
                reserved_bindings.extend(planned_bindings.iter().cloned());
                rewrite_runtime_namespace_member_accesses(
                    source.source.as_str(),
                    &runtime_import_groups,
                    program.model().graph(),
                    &reserved_bindings,
                )
            });
            let source_runtime_refs = lowered_source
                .map(|source| {
                    let source_text = namespace_member_rewrite
                        .as_ref()
                        .map(|rewrite| rewrite.source.as_str())
                        .unwrap_or(source.source.as_str());
                    let mut refs = runtime_import_identifiers_in_source(source_text)
                        .into_iter()
                        .map(BindingName::new)
                        .collect::<BTreeSet<_>>();
                    refs.extend(
                        program
                            .model()
                            .graph()
                            .import_export()
                            .exports_for(module.id)
                            .into_iter(),
                    );
                    if let Some(exports) = source_module_wiring.exports_by_module.get(&module.id) {
                        refs.extend(exports.iter().cloned());
                    }
                    if let Some((_stripped, exports)) = strip_top_level_named_exports(source_text) {
                        refs.extend(exports);
                    }
                    refs
                })
                .unwrap_or_default();
            let remaining_runtime_helpers: BTreeSet<BindingName> = if let Some(lowered_source) =
                lowered_source
                && !written_runtime_helpers.is_empty()
            {
                let source_text = namespace_member_rewrite
                    .as_ref()
                    .map(|rewrite| rewrite.source.as_str())
                    .unwrap_or(lowered_source.source.as_str());
                let rewritten =
                    rewrite_runtime_helper_writes(source_text, &written_runtime_helpers);
                let mut refs_after_write_rewrite = runtime_import_identifiers_in_source(&rewritten)
                    .into_iter()
                    .map(BindingName::new)
                    .collect::<BTreeSet<_>>();
                refs_after_write_rewrite.extend(
                    program
                        .model()
                        .graph()
                        .import_export()
                        .exports_for(module.id)
                        .into_iter(),
                );
                if let Some(exports) = source_module_wiring.exports_by_module.get(&module.id) {
                    refs_after_write_rewrite.extend(exports.iter().cloned());
                }
                if let Some((_stripped, exports)) = strip_top_level_named_exports(&rewritten) {
                    refs_after_write_rewrite.extend(exports);
                }
                remaining_runtime_helpers
                    .into_iter()
                    .filter(|binding| {
                        migrated_extra_runtime_deps.contains(binding)
                            || refs_after_write_rewrite.contains(binding)
                    })
                    .collect()
            } else {
                remaining_runtime_helpers
            };
            let namespace_export_helpers_for_source = lowered_source
                .and_then(|source| {
                    program
                        .model()
                        .graph()
                        .runtime_prelude(source.source_file_id)
                })
                .map(|prelude| {
                    prelude
                        .namespace_exports
                        .iter()
                        .map(|export| export.helper.clone())
                        .collect::<BTreeSet<_>>()
                })
                .unwrap_or_default();
            let remaining_runtime_helpers: BTreeSet<BindingName> = remaining_runtime_helpers
                .into_iter()
                .filter(|binding| {
                    !namespace_export_helpers_for_source.contains(binding)
                        || source_runtime_refs.contains(binding)
                })
                .collect();
            let dropped_runtime_namespaces_for_source = lowered_source
                .and_then(|source| {
                    namespace_member_rewrite.as_ref().and_then(|rewrite| {
                        rewrite
                            .dropped_namespaces_by_source
                            .get(&source.source_file_id)
                    })
                })
                .cloned()
                .unwrap_or_default();
            let remaining_runtime_helpers: BTreeSet<BindingName> = remaining_runtime_helpers
                .into_iter()
                .filter(|binding| !dropped_runtime_namespaces_for_source.contains(binding))
                .collect();
            let mut runtime_import_partitions = Vec::<(u32, RuntimeReexportImportPartition)>::new();
            for (source_file_id, bindings) in runtime_import_groups {
                let dropped_runtime_namespaces = namespace_member_rewrite
                    .as_ref()
                    .and_then(|rewrite| rewrite.dropped_namespaces_by_source.get(&source_file_id))
                    .cloned()
                    .unwrap_or_default();
                let namespace_member_imports = namespace_member_rewrite
                    .as_ref()
                    .and_then(|rewrite| rewrite.imports_by_source.get(&source_file_id))
                    .cloned()
                    .unwrap_or_default();
                let namespace_export_helpers = program
                    .model()
                    .graph()
                    .runtime_prelude(source_file_id)
                    .map(|prelude| {
                        prelude
                            .namespace_exports
                            .iter()
                            .map(|export| export.helper.clone())
                            .collect::<BTreeSet<_>>()
                    })
                    .unwrap_or_default();
                let bindings = bindings
                    .into_iter()
                    .filter(|binding| !dropped_runtime_namespaces.contains(binding))
                    .chain(namespace_member_imports.into_iter())
                    // Namespace helper calls such as `__export(ns, {...})`
                    // are planner-lowered to `Object.defineProperties(...)`
                    // when the namespace setup is emitted. If graph recovery
                    // attributed that prelude-only call to a source module,
                    // do not keep a synthetic runtime import/export edge for
                    // the helper unless the module body itself still names it.
                    .filter(|binding| {
                        !namespace_export_helpers.contains(binding)
                            || source_runtime_refs.contains(binding)
                    })
                    .filter(|binding| !lowered_helpers.contains(binding))
                    // Runtime imports recorded by the graph can include the
                    // LHS of cross-module writes. After we rewrite those
                    // writes through `__reverts_set_X(...)`, a write-only `X`
                    // must not stay in the value-import/public-export
                    // surface. Bindings that are both written and read in the
                    // lowered source are handled by `remaining_runtime_helpers`
                    // above, so this only removes setter-only leftovers.
                    .filter(|binding| !written_runtime_helpers.contains(binding))
                    .filter(|binding| !remaining_runtime_helpers.contains(binding))
                    .filter(|binding| !planned_bindings.contains(binding))
                    .filter(|binding| !local_source_definitions.contains(binding))
                    .filter(|binding| !local_source_writes.contains(binding))
                    .collect::<BTreeSet<_>>();
                if bindings.is_empty() {
                    continue;
                }
                let import_partition = partition_runtime_reexport_bindings(
                    &runtime_var_migrations,
                    &runtime_prelude_direct_imports,
                    source_file_id,
                    module.id,
                    bindings,
                );
                if !import_partition.runtime_bindings.is_empty()
                    || !import_partition.direct_imports.is_empty()
                {
                    runtime_import_partitions.push((source_file_id, import_partition));
                }
            }
            let has_runtime_group_imports = runtime_import_partitions
                .iter()
                .any(|(_, partition)| !partition.runtime_bindings.is_empty());
            let mut lowered_import_partition = lowered_source
                .map(|lowered_source| {
                    partition_runtime_reexport_bindings(
                        &runtime_var_migrations,
                        &runtime_prelude_direct_imports,
                        lowered_source.source_file_id,
                        module.id,
                        remaining_runtime_helpers.clone(),
                    )
                })
                .unwrap_or_default();
            let mut lazy_helper_names = lowered_source
                .map(lazy_helper_import_names_for_source)
                .unwrap_or_default();
            let mut localized_lazy_value_source: Option<String> = None;
            if let Some(lowered_source) = lowered_source
                && lowered_source.uses_lazy_value
                && !lowered_source.uses_lazy_module
                && !has_runtime_edge_before_lazy_helpers
                && lowered_import_partition.runtime_bindings.is_empty()
                && written_runtime_helpers.is_empty()
                && !has_runtime_group_imports
            {
                let source_for_lazy = namespace_member_rewrite
                    .as_ref()
                    .map(|rewrite| rewrite.source.as_str())
                    .unwrap_or(lowered_source.source.as_str());
                let (localized, changed) =
                    inline_remaining_lazy_value_wrappers_allowing_assignments(source_for_lazy);
                if changed {
                    localized_lazy_value_source = Some(localized);
                    lazy_helper_names.retain(|name| *name != "lazyValue");
                }
            }
            let mut runtime_sources_for_module = BTreeSet::<u32>::new();
            if let Some(lowered_source) = lowered_source
                && (!lowered_import_partition.runtime_bindings.is_empty()
                    || !written_runtime_helpers.is_empty()
                    || !lazy_helper_names.is_empty())
            {
                runtime_sources_for_module.insert(lowered_source.source_file_id);
            }
            for (source_file_id, partition) in &runtime_import_partitions {
                if !partition.runtime_bindings.is_empty() {
                    runtime_sources_for_module.insert(*source_file_id);
                }
            }
            if let Some(lowered_source) = lowered_source
                && runtime_sources_for_module.contains(&lowered_source.source_file_id)
            {
                lowered_import_partition.route_prelude_imports_through_runtime_except(
                    runtime_edge_direct_prelude_imports.get(&lowered_source.source_file_id),
                );
            }
            for (source_file_id, partition) in &mut runtime_import_partitions {
                if runtime_sources_for_module.contains(source_file_id) {
                    partition.route_prelude_imports_through_runtime_except(
                        runtime_edge_direct_prelude_imports.get(source_file_id),
                    );
                }
            }
            if !migrated_extra_source_deps.is_empty() {
                emit_direct_owner_imports(
                    program,
                    module.id,
                    path,
                    &mut file,
                    &mut planned_bindings,
                    &migrated_extra_source_deps,
                );
            }
            if let Some(lowered_source) = lowered_source
                && (!remaining_runtime_helpers.is_empty()
                    || !written_runtime_helpers.is_empty()
                    || !lazy_helper_names.is_empty())
            {
                emit_direct_owner_imports(
                    program,
                    module.id,
                    path,
                    &mut file,
                    &mut planned_bindings,
                    &lowered_import_partition.direct_imports,
                );
                emit_direct_prelude_imports(
                    &mut file,
                    &mut planned_bindings,
                    &lowered_import_partition.direct_prelude_imports,
                );
                let remaining_runtime_helpers = lowered_import_partition.runtime_bindings.clone();
                if !remaining_runtime_helpers.is_empty()
                    || !written_runtime_helpers.is_empty()
                    || !lazy_helper_names.is_empty()
                {
                    used_runtime_helper_files
                        .entry(lowered_source.source_file_id)
                        .or_default();
                }
                if !remaining_runtime_helpers.is_empty() {
                    used_runtime_helper_files
                        .entry(lowered_source.source_file_id)
                        .or_default()
                        .extend(remaining_runtime_helpers.iter().cloned());
                    exported_runtime_helper_bindings
                        .entry(lowered_source.source_file_id)
                        .or_default()
                        .extend(remaining_runtime_helpers.iter().cloned());
                    required_runtime_helper_bindings
                        .entry(lowered_source.source_file_id)
                        .or_default()
                        .extend(remaining_runtime_helpers.iter().cloned());
                }
                if !written_runtime_helpers.is_empty() {
                    used_runtime_helper_setters
                        .entry(lowered_source.source_file_id)
                        .or_default()
                        .extend(written_runtime_helpers.iter().cloned());
                }
                if lowered_source.uses_lazy_module {
                    used_lazy_module.insert(lowered_source.source_file_id);
                }
                if lazy_helper_names.contains(&"lazyValue") {
                    used_lazy_value.insert(lowered_source.source_file_id);
                }
                if lazy_helper_names.contains(&"lazyModule") {
                    exported_lazy_module.insert(lowered_source.source_file_id);
                }
                if lazy_helper_names.contains(&"lazyValue") {
                    exported_lazy_value.insert(lowered_source.source_file_id);
                }
                let specifier = relative_import_specifier(
                    path,
                    runtime_helpers_path(lowered_source.source_file_id).as_str(),
                );
                if !remaining_runtime_helpers.is_empty()
                    || !written_runtime_helpers.is_empty()
                    || !lazy_helper_names.is_empty()
                {
                    file.push_source(runtime_helper_import_statement(
                        &remaining_runtime_helpers,
                        &written_runtime_helpers,
                        &lazy_helper_names,
                        specifier.as_str(),
                    ));
                    for binding in &remaining_runtime_helpers {
                        if planned_bindings.contains(binding) {
                            continue;
                        }
                        planned_bindings.insert(binding.clone());
                        file.add_binding(plan_binding_from_program(
                            program,
                            module.id,
                            binding.clone(),
                            binding.clone(),
                            true,
                            None,
                        ));
                    }
                }
            }

            for (source_file_id, import_partition) in runtime_import_partitions {
                emit_direct_owner_imports(
                    program,
                    module.id,
                    path,
                    &mut file,
                    &mut planned_bindings,
                    &import_partition.direct_imports,
                );
                emit_direct_prelude_imports(
                    &mut file,
                    &mut planned_bindings,
                    &import_partition.direct_prelude_imports,
                );
                let bindings = import_partition.runtime_bindings;
                if bindings.is_empty() {
                    continue;
                }
                used_runtime_helper_files
                    .entry(source_file_id)
                    .or_default()
                    .extend(bindings.iter().cloned());
                exported_runtime_helper_bindings
                    .entry(source_file_id)
                    .or_default()
                    .extend(bindings.iter().cloned());
                required_runtime_helper_bindings
                    .entry(source_file_id)
                    .or_default()
                    .extend(bindings.iter().cloned());
                let specifier =
                    relative_import_specifier(path, runtime_helpers_path(source_file_id).as_str());
                file.push_source(runtime_helper_import_statement(
                    &bindings,
                    &BTreeSet::new(),
                    &[],
                    specifier.as_str(),
                ));
                for binding in bindings {
                    if planned_bindings.contains(&binding) {
                        continue;
                    }
                    planned_bindings.insert(binding.clone());
                    file.add_binding(plan_binding_from_program(
                        program,
                        module.id,
                        binding.clone(),
                        binding,
                        true,
                        None,
                    ));
                }
            }

            let source_definitions = program.model().graph().ast_definitions_for(module.id);
            let reshaped_bindings = lowered_source
                .map(|src| src.reshaped_bindings.clone())
                .unwrap_or_default();
            for original in program.model().graph().definitions_for(module.id) {
                let source_backed = source_definitions.contains(&original);
                let emitted = program
                    .semantic_names()
                    .binding_name(module.id, original.as_str())
                    .cloned()
                    .unwrap_or_else(|| original.clone());
                if source_backed && emitted != original {
                    file.add_readability_rename(PlannedRename::new(
                        original.clone(),
                        emitted.clone(),
                    ));
                }
                let shape_override = if reshaped_bindings.contains(&original) {
                    Some(BindingShape::Unknown)
                } else {
                    None
                };
                planned_bindings.insert(original.clone());
                file.add_binding(plan_binding_from_program(
                    program,
                    module.id,
                    original,
                    emitted,
                    source_backed,
                    shape_override,
                ));
            }

            for original in &source_imports {
                if planned_bindings.contains(original) {
                    continue;
                }
                let emitted = program
                    .semantic_names()
                    .binding_name(module.id, original.as_str())
                    .cloned()
                    .unwrap_or_else(|| original.clone());
                if emitted != *original {
                    file.add_readability_rename(PlannedRename::new(
                        original.clone(),
                        emitted.clone(),
                    ));
                }
                planned_bindings.insert(original.clone());
                file.add_binding(plan_binding_from_program(
                    program,
                    module.id,
                    original.clone(),
                    emitted,
                    true,
                    None,
                ));
            }

            if let Some(lowered_source) = lowered_source {
                let source_file_path = lowered_source.source_file_path.as_str();
                let mut source = localized_lazy_value_source.clone().unwrap_or_else(|| {
                    namespace_member_rewrite
                        .as_ref()
                        .map(|rewrite| rewrite.source.clone())
                        .unwrap_or_else(|| lowered_source.source.clone())
                });
                if !written_runtime_helpers.is_empty() {
                    source =
                        rewrite_runtime_helper_writes(source.as_str(), &written_runtime_helpers);
                }
                if contains_call_to_identifier(source.as_str(), "require")
                    && !local_source_definitions.contains(&BindingName::new("require"))
                {
                    file.push_source(node_require_prelude_statement());
                    planned_bindings.insert(BindingName::new("require"));
                }
                // Phase 10b: register migrated bindings as planned BEFORE
                // computing implicit globals — otherwise the implicit-
                // globals scan picks the same bindings up as undeclared
                // writes and emits a redundant `var X;` line alongside
                // the migration's own declaration.
                for binding in &migrated_local_bindings {
                    planned_bindings.insert(binding.clone());
                    let binding_shape = if migrated_locally.contains(binding)
                        || migrated_extra_namespace_bindings.contains(binding)
                    {
                        BindingShape::Unknown
                    } else {
                        BindingShape::Callable
                    };
                    file.add_binding(PlannedBinding::new(
                        binding.clone(),
                        binding.clone(),
                        binding_shape,
                        true,
                    ));
                }
                let implicit_globals = implicit_global_declarations_for_module(
                    source.as_str(),
                    &source_definitions,
                    &source_imports,
                    &planned_bindings,
                );
                if !implicit_globals.is_empty() {
                    file.push_source(variable_declaration_statement(implicit_globals.iter()));
                }
                // Declare the bindings the runtime helpers module no
                // longer hosts. Their `X = value` writes (left
                // unrewritten above because we excluded them from
                // `written_runtime_helpers`) now bind to the same-module
                // `var X;` slot. The runtime helpers file re-exports
                // these from this module, so cross-module readers keep
                // resolving them transparently. Bindings that came with
                // a literal initializer in the prelude emit
                // `var X = INIT;` so the writer keeps the original
                // initial value the runtime used to set at load.
                if !migrated_locally.is_empty() {
                    let bare: BTreeSet<&BindingName> = migrated_locally
                        .iter()
                        .filter(|binding| {
                            runtime_var_migrations
                                .migrations_by_binding
                                .get(*binding)
                                .and_then(|m| m.initializer.as_deref())
                                .is_none()
                        })
                        .collect();
                    if !bare.is_empty() {
                        file.push_source(variable_declaration_statement(bare.into_iter()));
                    }
                    for binding in &migrated_locally {
                        let Some(migration) =
                            runtime_var_migrations.migrations_by_binding.get(binding)
                        else {
                            continue;
                        };
                        let Some(initializer) = migration.initializer.as_deref() else {
                            continue;
                        };
                        file.push_source(format!(
                            "var {name} = {initializer};",
                            name = binding.as_str()
                        ));
                    }
                }
                if !migrated_extra_snippets.is_empty()
                    || !migrated_extra_namespace_exports.is_empty()
                {
                    let mut migrated_chunks = Vec::<(u32, u8, String)>::new();
                    for (source_file_id, binding) in &migrated_extra_snippets {
                        let Some(prelude) =
                            program.model().graph().runtime_prelude(*source_file_id)
                        else {
                            continue;
                        };
                        let Some(snippet) = prelude.snippets.get(binding) else {
                            continue;
                        };
                        migrated_chunks.push((snippet.byte_start, 0, snippet.source.clone()));
                    }
                    for (source_file_id, namespace) in &migrated_extra_namespace_exports {
                        let Some(prelude) =
                            program.model().graph().runtime_prelude(*source_file_id)
                        else {
                            continue;
                        };
                        let Some(namespace_export) = prelude
                            .namespace_exports
                            .iter()
                            .find(|export| export.namespace == *namespace)
                        else {
                            continue;
                        };
                        migrated_chunks.push((
                            namespace_export.byte_start,
                            1,
                            runtime_namespace_export_statement(namespace_export),
                        ));
                    }
                    migrated_chunks.sort_by_key(|(byte_start, kind, _source)| (*byte_start, *kind));
                    for (_, _, source) in migrated_chunks {
                        file.push_source(source);
                    }
                }
                let normalized = normalize_source_for_emit(
                    module.id,
                    source_file_path,
                    source.as_str(),
                    file.source_strategy(),
                )?;
                file.push_source(normalized);
            }

            for export in program
                .model()
                .graph()
                .import_export()
                .exports_for(module.id)
            {
                file.add_export_with_source_backed(export, true);
            }
            // Phase 10b: bindings this module now owns must be exported so
            // the runtime helpers file (which re-exports from here) and
            // every consumer importing from runtime continue to resolve
            // through the live binding. Skip any binding that is already
            // exported through the module's regular AST or wiring path
            // to avoid duplicate-export audit failures.
            if !migrated_local_bindings.is_empty() {
                let already_exported: BTreeSet<BindingName> = program
                    .model()
                    .graph()
                    .import_export()
                    .exports_for(module.id)
                    .into_iter()
                    .chain(
                        source_module_wiring
                            .exports_by_module
                            .get(&module.id)
                            .cloned()
                            .unwrap_or_default(),
                    )
                    .collect();
                let new_exports: BTreeSet<BindingName> = migrated_local_bindings
                    .difference(&already_exported)
                    .cloned()
                    .collect();
                if !new_exports.is_empty() {
                    file.push_source(named_export_statement(new_exports.iter()));
                    for binding in &new_exports {
                        file.add_export_with_source_backed(binding.clone(), true);
                    }
                }
            }
            let migration_source_exports =
                runtime_var_migrations.source_dep_exports_for_module(module.id);
            if !migration_source_exports.is_empty() {
                let already_exported = file
                    .exports
                    .iter()
                    .map(|export| export.binding.clone())
                    .collect::<BTreeSet<_>>();
                let new_exports = migration_source_exports
                    .difference(&already_exported)
                    .cloned()
                    .collect::<BTreeSet<_>>();
                if !new_exports.is_empty() {
                    file.push_source(named_export_statement(new_exports.iter()));
                    for binding in &new_exports {
                        file.add_export_with_source_backed(binding.clone(), true);
                    }
                }
            }
            let extra_exports = extra_exports_for_module(
                program,
                module.id,
                [source_module_wiring.exports_by_module.get(&module.id)],
            );
            if !extra_exports.is_empty() {
                let existing_exports = program
                    .model()
                    .graph()
                    .import_export()
                    .exports_for(module.id)
                    .into_iter()
                    .collect::<BTreeSet<_>>();
                let extra_exports = extra_exports
                    .into_iter()
                    .filter(|binding| !existing_exports.contains(binding))
                    .collect::<BTreeSet<_>>();
                if !extra_exports.is_empty() {
                    file.push_source(named_export_statement(extra_exports.iter()));
                    for export in extra_exports {
                        file.add_export_with_source_backed(export, true);
                    }
                }
            }

            plan.push_file(file);
        }
        if let Some((_prelude, entrypoint)) = runtime_entrypoint(program) {
            used_runtime_helper_files
                .entry(entrypoint.source_file_id)
                .or_default()
                .insert(entrypoint.callee.clone());
            exported_runtime_helper_bindings
                .entry(entrypoint.source_file_id)
                .or_default()
                .insert(entrypoint.callee.clone());
            required_runtime_helper_bindings
                .entry(entrypoint.source_file_id)
                .or_default()
                .insert(entrypoint.callee.clone());
        }

        for (source_file_id, helper_bindings) in &used_runtime_helper_files {
            let Some(prelude) = program.model().graph().runtime_prelude(*source_file_id) else {
                continue;
            };
            let mut file = PlannedFile::new(runtime_helpers_path(*source_file_id));
            let mut public_helper_bindings = exported_runtime_helper_bindings
                .get(source_file_id)
                .cloned()
                .unwrap_or_default();
            let migrations_for_source =
                runtime_var_migrations.primary_bindings_for_source(*source_file_id);
            let reexported_bindings_for_source =
                runtime_var_migrations.reexported_bindings_for_source(*source_file_id);
            let runtime_reexports_for_source = reexported_bindings_for_source
                .iter()
                .filter(|(binding, _owner)| public_helper_bindings.contains(*binding))
                .map(|(binding, owner)| (binding.clone(), *owner))
                .collect::<BTreeMap<_, _>>();
            let entrypoint = prelude
                .entrypoint
                .as_ref()
                .filter(|entrypoint| helper_bindings.contains(&entrypoint.callee));
            let entrypoint_callee = entrypoint.map(|entrypoint| entrypoint.callee.clone());
            let consumed_helper_bindings =
                planned_runtime_helper_consumed_bindings(&plan, *source_file_id);
            let namespace_export_helpers = prelude
                .namespace_exports
                .iter()
                .map(|export| export.helper.clone())
                .collect::<BTreeSet<_>>();
            public_helper_bindings.retain(|binding| {
                !namespace_export_helpers.contains(binding)
                    || consumed_helper_bindings.contains(binding)
                    || entrypoint_callee
                        .as_ref()
                        .is_some_and(|callee| callee == binding)
            });
            let mut root_bindings = required_runtime_helper_bindings
                .get(source_file_id)
                .cloned()
                .unwrap_or_else(|| helper_bindings.clone());
            root_bindings.retain(|binding| {
                !namespace_export_helpers.contains(binding)
                    || consumed_helper_bindings.contains(binding)
                    || entrypoint_callee
                        .as_ref()
                        .is_some_and(|callee| callee == binding)
            });
            if let Some(setter_targets) = used_runtime_helper_setters.get(source_file_id) {
                root_bindings.extend(setter_targets.iter().cloned());
            }
            for binding in reexported_bindings_for_source.keys() {
                root_bindings.remove(binding);
            }
            if let Some(entrypoint) = entrypoint {
                root_bindings.extend(runtime_entrypoint_root_bindings(prelude, entrypoint));
            }
            if let Some(folded_chunks) =
                runtime_lazy_folds.chunks_by_source_file.get(source_file_id)
            {
                for chunk in folded_chunks {
                    for identifier in identifiers_in_source(chunk.source.as_str()) {
                        let binding = BindingName::new(identifier);
                        if prelude.defines(&binding) {
                            root_bindings.insert(binding);
                        }
                    }
                }
            }
            for binding in reexported_bindings_for_source.keys() {
                root_bindings.remove(binding);
            }
            let folded_chunks = runtime_lazy_folds
                .chunks_by_source_file
                .get(source_file_id)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let mut helper_closure =
                close_runtime_helper_source(prelude, &root_bindings, entrypoint, folded_chunks);
            // Phase 9a: collapse `__reverts_set_X(arg)` → `(X = arg)`
            // whenever the call sits inside the runtime helpers module
            // itself. Same-module direct assignment is observationally
            // equivalent for single-argument invocations, and the form
            // reads as the underlying bundler intent. The cross-module
            // setter function is still emitted below so consumer modules
            // (which can't legally assign through their read-only ESM
            // imports) keep their existing call form unchanged.
            helper_closure.source = inline_internal_setter_calls(&helper_closure.source);
            // Phase 10b: bindings the migration plan reassigned out of
            // this runtime helpers file no longer have their `var X;`
            // declaration here. The writer module re-declares them
            // locally and exports them; below we emit `export { X } from
            // '<owner>';` re-exports so existing consumer imports keep
            // working unchanged.
            if !migrations_for_source.is_empty() {
                helper_closure.source = strip_runtime_var_declarations(
                    helper_closure.source.as_str(),
                    migrations_for_source.keys(),
                );
            }
            // Phase 11a: some runtime lazyValue thunks are not real lazy
            // module boundaries; they only fill private helper vars with
            // pure values and return `undefined`. Make those assignments
            // direct and leave a no-op initializer behind for existing
            // `init()` call sites. This removes lazy loader wrappers
            // without changing the exported runtime surface.
            helper_closure.source = purify_private_runtime_lazy_initializers(
                helper_closure.source.as_str(),
                &helper_closure.emitted_bindings,
            );
            let helper_imports = runtime_source_module_imports(
                program,
                helper_closure.source.as_str(),
                &helper_closure.emitted_bindings,
                externalized_packages,
            );
            let package_init_shims = externalized_package_init_shims(
                program,
                helper_closure.source.as_str(),
                externalized_packages,
            );
            let mut emitted_runtime_bindings = helper_closure.emitted_bindings.clone();
            emitted_runtime_bindings.extend(package_init_shims.iter().cloned());
            let helper_path = runtime_helpers_path(*source_file_id);
            let unresolved = unresolved_runtime_helper_references(
                prelude,
                helper_closure.source.as_str(),
                &emitted_runtime_bindings,
                &helper_imports,
            );
            if !unresolved.is_empty() {
                return Err(PlanError::UnresolvedRuntimeHelperReferences {
                    path: helper_path,
                    bindings: unresolved.into_iter().collect(),
                });
            }
            for (module_id, bindings) in &helper_imports {
                ensure_planned_module_exports(&mut plan, program, *module_id, bindings);
                let Some(module_path) = module_output_path(program, *module_id) else {
                    continue;
                };
                let specifier =
                    relative_import_specifier(helper_path.as_str(), module_path.as_str());
                file.push_source(named_import_statement(bindings.iter(), specifier.as_str()));
            }
            for binding in &package_init_shims {
                file.push_source(noop_function_statement(binding));
            }
            if !helper_closure.source.trim().is_empty() {
                file.push_source(helper_closure.source);
            }
            let setter_bindings = used_runtime_helper_setters
                .get(source_file_id)
                .cloned()
                .unwrap_or_default();
            // Phase 10b: skip setter functions for migrated bindings;
            // the writer module now mutates them via direct assignment.
            let setter_bindings: BTreeSet<BindingName> = setter_bindings
                .difference(
                    &migrations_for_source
                        .keys()
                        .cloned()
                        .collect::<BTreeSet<_>>(),
                )
                .cloned()
                .collect();
            for binding in &setter_bindings {
                file.push_source(runtime_helper_setter_declaration(binding));
            }
            let emits_lazy_module = used_lazy_module.contains(source_file_id);
            let emits_lazy_value = used_lazy_value.contains(source_file_id);
            let exports_lazy_module = exported_lazy_module.contains(source_file_id);
            let exports_lazy_value = exported_lazy_value.contains(source_file_id);
            if emits_lazy_module {
                file.push_source(lazy_module_helper_source());
            }
            if emits_lazy_value {
                file.push_source(lazy_value_helper_source());
            }
            let mut exported_bindings = public_helper_bindings.clone();
            exported_bindings.extend(
                setter_bindings
                    .iter()
                    .map(|binding| BindingName::new(runtime_helper_setter_name(binding))),
            );
            // Phase 10b: drop migrated bindings from the runtime helper's
            // own named export — they're re-exported below from their
            // new owner module so the consumer's `import { X } from
            // runtime` still resolves to a live binding.
            for binding in runtime_reexports_for_source.keys() {
                exported_bindings.remove(binding);
            }
            if exports_lazy_module {
                exported_bindings.insert(BindingName::new("lazyModule"));
            }
            if exports_lazy_value {
                exported_bindings.insert(BindingName::new("lazyValue"));
            }
            if !exported_bindings.is_empty() {
                file.push_source(named_export_statement(exported_bindings.iter()));
            }
            // Phase 12: only keep migrated-binding re-exports when an
            // internal consumer still imports the compatibility path. Most
            // consumers are rewritten to import the owner module directly, so
            // these statements usually disappear entirely.
            let mut migrations_by_owner_path: BTreeMap<String, BTreeSet<BindingName>> =
                BTreeMap::new();
            for (binding, owner_module) in &runtime_reexports_for_source {
                let Some(owner_path) = module_output_path(program, *owner_module) else {
                    continue;
                };
                let specifier =
                    relative_import_specifier(helper_path.as_str(), owner_path.as_str());
                migrations_by_owner_path
                    .entry(specifier)
                    .or_default()
                    .insert(binding.clone());
            }
            for (specifier, bindings) in &migrations_by_owner_path {
                file.push_source(named_reexport_statement(bindings.iter(), specifier));
                for binding in bindings {
                    file.add_binding(PlannedBinding::new(
                        binding.clone(),
                        binding.clone(),
                        BindingShape::Unknown,
                        true,
                    ));
                    file.add_export_with_source_backed(binding.clone(), true);
                }
            }
            for binding in public_helper_bindings
                .iter()
                .filter(|binding| !reexported_bindings_for_source.contains_key(*binding))
                .cloned()
            {
                file.add_binding(PlannedBinding::new(
                    binding.clone(),
                    binding.clone(),
                    BindingShape::Unknown,
                    true,
                ));
                file.add_export_with_source_backed(binding, true);
            }
            for setter in setter_bindings
                .iter()
                .map(|binding| BindingName::new(runtime_helper_setter_name(binding)))
            {
                file.add_binding(PlannedBinding::new(
                    setter.clone(),
                    setter.clone(),
                    BindingShape::Callable,
                    true,
                ));
                file.add_export_with_source_backed(setter, true);
            }
            for lazy_name in [
                exports_lazy_module.then_some("lazyModule"),
                exports_lazy_value.then_some("lazyValue"),
            ]
            .into_iter()
            .flatten()
            {
                let binding = BindingName::new(lazy_name);
                file.add_binding(PlannedBinding::new(
                    binding.clone(),
                    binding.clone(),
                    BindingShape::Callable,
                    true,
                ));
                file.add_export_with_source_backed(binding, true);
            }
            if file.body.is_empty() {
                continue;
            }
            plan.push_file(file);
        }

        if let Some((_prelude, entrypoint)) = runtime_entrypoint(program) {
            let mut file = PlannedFile::new("cli.ts");
            file.push_source("#!/usr/bin/env node");
            let helper_path = runtime_helpers_path(entrypoint.source_file_id);
            let specifier = relative_import_specifier("cli.ts", helper_path.as_str());
            let entrypoint_imports = BTreeSet::from([entrypoint.callee.clone()]);
            file.push_source(named_import_statement(
                entrypoint_imports.iter(),
                specifier.as_str(),
            ));
            file.push_source(format!("await {}();", entrypoint.callee.as_str()));
            plan.push_file(file);
        }

        Ok(plan)
    }

    #[must_use]
    pub fn plan_module_file(
        self,
        graph: &RevertsGraph,
        module_id: ModuleId,
        path: impl Into<String>,
    ) -> PlannedFile {
        let mut file = PlannedFile::new(path);
        for binding in graph.definitions_for(module_id) {
            file.add_export(binding);
        }
        file
    }
}

fn normalize_source_for_emit(
    module_id: ModuleId,
    path: &str,
    source: &str,
    source_strategy: SourceCompilerStrategy,
) -> Result<String, PlanError> {
    format_source_pretty(
        source,
        source_strategy.path_hint(path),
        source_strategy.parse_goal(),
    )
    .map_err(|error| PlanError::UnparseableSource {
        module_id,
        path: path.to_string(),
        message: parse_error_message(&error, "source could not be parsed"),
    })
}

fn group_runtime_imports(
    imports: Vec<RuntimePreludeImport>,
) -> BTreeMap<u32, BTreeSet<BindingName>> {
    let mut grouped = BTreeMap::<u32, BTreeSet<BindingName>>::new();
    for import in imports {
        grouped
            .entry(import.source_file_id)
            .or_default()
            .insert(import.binding);
    }
    grouped
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct SourceModuleWiring {
    imports_by_module: BTreeMap<ModuleId, BTreeMap<ModuleId, BTreeSet<BindingName>>>,
    exports_by_module: BTreeMap<ModuleId, BTreeSet<BindingName>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LoweredRuntimeModuleSource {
    source_file_id: u32,
    source_file_path: String,
    byte_start: u32,
    source: String,
    lowered_helpers: BTreeSet<BindingName>,
    remaining_helpers: BTreeSet<BindingName>,
    local_definitions: BTreeSet<BindingName>,
    local_writes: BTreeSet<BindingName>,
    written_helpers: BTreeSet<BindingName>,
    uses_lazy_module: bool,
    uses_lazy_value: bool,
    /// Bindings whose shape was rewritten by delazify / namespace decomposition.
    /// The planner must override the IR-derived shape (which assumed the
    /// pre-lowering lazy thunk) to keep the audit consistent with what was
    /// actually emitted.
    reshaped_bindings: BTreeSet<BindingName>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct RuntimeLazyFoldPlan {
    modules: BTreeMap<ModuleId, RuntimeLazyFoldModule>,
    chunks_by_source_file: BTreeMap<u32, Vec<RuntimeFoldedSourceChunk>>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct PureReexportBypassPlan {
    omitted_modules: BTreeSet<ModuleId>,
    redirects: BTreeMap<ModuleId, BTreeMap<BindingName, ModuleId>>,
}

fn folded_stub_modules_with_internal_consumers(
    runtime_lazy_folds: &RuntimeLazyFoldPlan,
    source_module_wiring: &SourceModuleWiring,
) -> BTreeSet<ModuleId> {
    runtime_lazy_folds
        .modules
        .keys()
        .filter(|module_id| {
            source_module_wiring
                .exports_by_module
                .get(module_id)
                .is_some_and(|exports| !exports.is_empty())
        })
        .copied()
        .collect()
}

fn pure_reexport_bypass_plan(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
    externalized_packages: &BTreeSet<ModuleId>,
) -> PureReexportBypassPlan {
    let modules_by_id = program
        .model()
        .modules()
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let explicit_exports_by_module = program
        .model()
        .modules()
        .iter()
        .map(|module| {
            (
                module.id,
                program
                    .model()
                    .graph()
                    .import_export()
                    .exports_for(module.id)
                    .into_iter()
                    .collect::<BTreeSet<_>>(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut plan = PureReexportBypassPlan::default();

    for module in program.model().modules() {
        if module.kind != ModuleKind::Application || externalized_packages.contains(&module.id) {
            continue;
        }
        let Some(source) = program.model().input().module_source_slice(module.id) else {
            continue;
        };
        let Some(reexports) = pure_named_barrel_reexports(source.source) else {
            continue;
        };
        let Some(consumed_reexports) = source_module_wiring.exports_by_module.get(&module.id)
        else {
            continue;
        };
        if reexports.is_empty() || consumed_reexports != &reexports {
            continue;
        }
        let mut redirects = BTreeMap::<BindingName, ModuleId>::new();
        for binding in &reexports {
            let mut owners = BTreeSet::<ModuleId>::new();
            for dependency in &program.model().input().dependencies {
                if dependency.from_module_id != module.id {
                    continue;
                }
                let ModuleDependencyTarget::Module(target_module_id) = dependency.target else {
                    continue;
                };
                let Some(target_module) = modules_by_id.get(&target_module_id) else {
                    continue;
                };
                if target_module.kind == ModuleKind::Package
                    && externalized_packages.contains(&target_module_id)
                {
                    continue;
                }
                if explicit_exports_by_module
                    .get(&target_module_id)
                    .is_some_and(|exports| exports.contains(binding))
                {
                    owners.insert(target_module_id);
                }
            }
            let Some(owner) = owners.iter().next().copied() else {
                continue;
            };
            if owners.len() == 1 {
                redirects.insert(binding.clone(), owner);
            }
        }
        if redirects.len() == reexports.len() {
            plan.omitted_modules.insert(module.id);
            plan.redirects.insert(module.id, redirects);
        }
    }

    plan
}

/// Phase 10: vars currently declared inside `source-<N>-helpers.ts` that
/// can be relocated to their writer module.
///
/// The runtime helper file traditionally holds every cross-module mutable
/// binding plus a `__reverts_set_X(value)` thunk for each, because ESM
/// forbids direct assignment to imported bindings. When a binding's value
/// is only WRITTEN by a single application module AND the runtime body
/// itself never READS that binding, the setter becomes a workaround for a
/// problem that no longer exists — the writer module can declare and
/// export the binding directly.
///
/// `migrations_by_binding` maps each migrated binding to its new owner
/// module. The runtime helper file replaces the binding's declaration
/// and setter with a `export { X } from '<owner>';` re-export so that
/// other consumers' existing `import { X } from runtime` keep working.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct RuntimeVarMigrationPlan {
    /// Binding name → (owner module id, runtime source file id the
    /// binding originally lived in).
    migrations_by_binding: BTreeMap<BindingName, RuntimeVarMigration>,
    /// Reverse index: owner module → set of bindings it now owns.
    /// This index contains primary runtime vars only; extra moved
    /// snippets are derived from `migrations_by_binding`.
    migrations_by_owner: BTreeMap<ModuleId, BTreeSet<BindingName>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeVarMigration {
    owner_module: ModuleId,
    source_file_id: u32,
    /// Additional runtime prelude snippets that must move with the
    /// primary var. The first conservative use is the single reader
    /// function that is the var's only runtime read.
    extra_snippets: BTreeSet<BindingName>,
    /// Runtime namespace export initializers whose namespace object
    /// snippet moved with the primary var. These are emitted in the
    /// writer module as `Object.defineProperties(...)` statements so the
    /// namespace getter no longer reads the migrated var from runtime.
    extra_namespace_exports: BTreeSet<BindingName>,
    /// Runtime helper bindings read by `extra_snippets` after excluding
    /// the migrated primary var. The writer module imports these back
    /// from the helper file.
    extra_runtime_deps: BTreeSet<BindingName>,
    /// Source-module bindings read by `extra_snippets` that are already
    /// represented by a source dependency edge from the writer. The writer
    /// imports these from their source module instead of forcing the reader
    /// cluster to stay in runtime.
    extra_source_deps: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    /// Optional initializer expression preserved from the runtime
    /// declaration. `None` means the original `var X;` had no
    /// initializer; `Some(text)` carries a side-effect-free literal
    /// (`null`, `0`, `!1`, `void 0`, etc.) to be emitted alongside the
    /// writer module's `var X = INIT;`.
    initializer: Option<String>,
}

impl RuntimeVarMigrationPlan {
    fn insert(&mut self, binding: BindingName, migration: RuntimeVarMigration) {
        self.migrations_by_owner
            .entry(migration.owner_module)
            .or_default()
            .insert(binding.clone());
        self.migrations_by_binding.insert(binding, migration);
    }

    fn extra_snippets_for_owner(&self, owner_module: ModuleId) -> BTreeSet<(u32, BindingName)> {
        self.migrations_by_binding
            .values()
            .filter(|migration| migration.owner_module == owner_module)
            .flat_map(|migration| {
                migration
                    .extra_snippets
                    .iter()
                    .cloned()
                    .map(|binding| (migration.source_file_id, binding))
            })
            .collect()
    }

    fn extra_runtime_deps_for_owner(&self, owner_module: ModuleId) -> BTreeSet<BindingName> {
        self.migrations_by_binding
            .values()
            .filter(|migration| migration.owner_module == owner_module)
            .flat_map(|migration| migration.extra_runtime_deps.iter().cloned())
            .collect()
    }

    fn extra_source_deps_for_owner(
        &self,
        owner_module: ModuleId,
    ) -> BTreeMap<ModuleId, BTreeSet<BindingName>> {
        let mut deps = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
        for migration in self
            .migrations_by_binding
            .values()
            .filter(|migration| migration.owner_module == owner_module)
        {
            for (module_id, bindings) in &migration.extra_source_deps {
                deps.entry(*module_id)
                    .or_default()
                    .extend(bindings.iter().cloned());
            }
        }
        deps
    }

    fn source_dep_exports_for_module(&self, module_id: ModuleId) -> BTreeSet<BindingName> {
        self.migrations_by_binding
            .values()
            .flat_map(|migration| {
                migration
                    .extra_source_deps
                    .get(&module_id)
                    .into_iter()
                    .flatten()
                    .cloned()
            })
            .collect()
    }

    fn extra_namespace_exports_for_owner(
        &self,
        owner_module: ModuleId,
    ) -> BTreeSet<(u32, BindingName)> {
        self.migrations_by_binding
            .values()
            .filter(|migration| migration.owner_module == owner_module)
            .flat_map(|migration| {
                migration
                    .extra_namespace_exports
                    .iter()
                    .cloned()
                    .map(|binding| (migration.source_file_id, binding))
            })
            .collect()
    }

    fn local_bindings_for_owner(&self, owner_module: ModuleId) -> BTreeSet<BindingName> {
        let mut bindings = self
            .migrations_by_owner
            .get(&owner_module)
            .cloned()
            .unwrap_or_default();
        bindings.extend(
            self.extra_snippets_for_owner(owner_module)
                .into_iter()
                .map(|(_, binding)| binding),
        );
        bindings
    }

    fn primary_bindings_for_source(&self, source_file_id: u32) -> BTreeMap<BindingName, ModuleId> {
        self.migrations_by_binding
            .iter()
            .filter(|(_, migration)| migration.source_file_id == source_file_id)
            .map(|(binding, migration)| (binding.clone(), migration.owner_module))
            .collect()
    }

    fn reexported_bindings_for_source(
        &self,
        source_file_id: u32,
    ) -> BTreeMap<BindingName, ModuleId> {
        let mut bindings = BTreeMap::new();
        for (binding, migration) in &self.migrations_by_binding {
            if migration.source_file_id != source_file_id {
                continue;
            }
            bindings.insert(binding.clone(), migration.owner_module);
            for extra in &migration.extra_snippets {
                bindings.insert(extra.clone(), migration.owner_module);
            }
        }
        bindings
    }

    fn reexport_owner(&self, source_file_id: u32, binding: &BindingName) -> Option<ModuleId> {
        for (primary, migration) in &self.migrations_by_binding {
            if migration.source_file_id != source_file_id {
                continue;
            }
            if primary == binding || migration.extra_snippets.contains(binding) {
                return Some(migration.owner_module);
            }
        }
        None
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct RuntimeReexportImportPartition {
    runtime_bindings: BTreeSet<BindingName>,
    direct_imports: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    direct_prelude_imports: BTreeMap<BindingName, RuntimePreludeDirectImport>,
}

impl RuntimeReexportImportPartition {
    fn route_prelude_imports_through_runtime_except(
        &mut self,
        keep_direct: Option<&BTreeSet<BindingName>>,
    ) {
        let direct_prelude_imports = std::mem::take(&mut self.direct_prelude_imports);
        for (binding, import) in direct_prelude_imports {
            if keep_direct.is_some_and(|bindings| bindings.contains(&binding)) {
                self.direct_prelude_imports.insert(binding, import);
            } else {
                self.runtime_bindings.insert(binding);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimePreludeDirectImport {
    source: String,
    snippet_source: String,
    snippet_byte_start: u32,
    kind: RuntimePreludeDirectImportKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RuntimePreludeDirectImportKind {
    Default,
    Namespace,
    Named { imported: String },
}

fn partition_runtime_reexport_bindings(
    migrations: &RuntimeVarMigrationPlan,
    direct_imports: &BTreeMap<u32, BTreeMap<BindingName, RuntimePreludeDirectImport>>,
    source_file_id: u32,
    current_module: ModuleId,
    bindings: BTreeSet<BindingName>,
) -> RuntimeReexportImportPartition {
    let mut partition = RuntimeReexportImportPartition::default();
    let direct_imports_for_source = direct_imports.get(&source_file_id);
    for binding in bindings {
        match migrations.reexport_owner(source_file_id, &binding) {
            Some(owner) if owner != current_module => {
                partition
                    .direct_imports
                    .entry(owner)
                    .or_default()
                    .insert(binding);
            }
            Some(_owner_is_current_module) => {}
            None => {
                if let Some(import) =
                    direct_imports_for_source.and_then(|imports| imports.get(&binding))
                {
                    partition
                        .direct_prelude_imports
                        .insert(binding, import.clone());
                } else {
                    partition.runtime_bindings.insert(binding);
                }
            }
        }
    }
    partition
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeNamespaceMemberAccessRewrite {
    source: String,
    imports_by_source: BTreeMap<u32, BTreeSet<BindingName>>,
    dropped_namespaces_by_source: BTreeMap<u32, BTreeSet<BindingName>>,
}

fn rewrite_runtime_namespace_member_accesses(
    source: &str,
    runtime_import_groups: &BTreeMap<u32, BTreeSet<BindingName>>,
    graph: &RevertsGraph,
    reserved_bindings: &BTreeSet<BindingName>,
) -> Option<RuntimeNamespaceMemberAccessRewrite> {
    let mut edits = Vec::<(usize, usize, String)>::new();
    let mut imports_by_source = BTreeMap::<u32, BTreeSet<BindingName>>::new();
    let mut dropped_namespaces_by_source = BTreeMap::<u32, BTreeSet<BindingName>>::new();
    let mut introduced = BTreeSet::<BindingName>::new();

    for (source_file_id, imported_bindings) in runtime_import_groups {
        let Some(prelude) = graph.runtime_prelude(*source_file_id) else {
            continue;
        };
        for namespace_export in &prelude.namespace_exports {
            if !imported_bindings.contains(&namespace_export.namespace) {
                continue;
            }
            let properties = namespace_export
                .exports
                .iter()
                .map(|(key, target)| (key.clone(), target.as_str().to_string()))
                .collect::<Vec<_>>();
            let Some(access_sites) = collect_member_access_only(
                source,
                namespace_export.namespace.as_str(),
                (0, 0),
                &properties,
            ) else {
                continue;
            };
            if access_sites.is_empty() {
                continue;
            }

            let mut targets = BTreeSet::<BindingName>::new();
            let mut namespace_edits = Vec::<(usize, usize, String)>::new();
            let mut safe = true;
            for (start, end, key) in access_sites {
                if !runtime_namespace_member_access_site_is_read_only(source, start, end) {
                    safe = false;
                    break;
                }
                let Some(target) = namespace_export.exports.get(&key) else {
                    safe = false;
                    break;
                };
                if target == &namespace_export.namespace || is_js_keyword(target.as_str()) {
                    safe = false;
                    break;
                }
                if reserved_bindings.contains(target) && !introduced.contains(target) {
                    safe = false;
                    break;
                }
                targets.insert(target.clone());
                namespace_edits.push((start, end, target.as_str().to_string()));
            }
            if !safe || targets.is_empty() {
                continue;
            }

            introduced.extend(targets.iter().cloned());
            edits.extend(namespace_edits);
            imports_by_source
                .entry(*source_file_id)
                .or_default()
                .extend(targets);
            dropped_namespaces_by_source
                .entry(*source_file_id)
                .or_default()
                .insert(namespace_export.namespace.clone());
        }
    }

    if edits.is_empty() {
        return None;
    }
    edits.sort_by_key(|(start, _, _)| *start);
    if edits.windows(2).any(|window| window[0].1 > window[1].0) {
        return None;
    }
    Some(RuntimeNamespaceMemberAccessRewrite {
        source: apply_text_edits(source, &edits),
        imports_by_source,
        dropped_namespaces_by_source,
    })
}

fn runtime_namespace_member_access_site_is_read_only(
    source: &str,
    start: usize,
    end: usize,
) -> bool {
    let bytes = source.as_bytes();
    if let Some(before) = previous_non_ws(bytes, start)
        && before > 0
        && bytes
            .get(before - 1..=before)
            .is_some_and(|operator| operator == b"++" || operator == b"--")
    {
        return false;
    }
    let after = skip_ws(bytes, end);
    match bytes.get(after).copied() {
        Some(b'+') | Some(b'-') => {
            !matches!(bytes.get(after + 1), Some(b'+') | Some(b'-') | Some(b'='))
        }
        Some(b'=') => matches!(bytes.get(after + 1), Some(b'=')),
        Some(b'*') => {
            bytes.get(after + 1) != Some(&b'=')
                && !(bytes.get(after + 1) == Some(&b'*') && bytes.get(after + 2) == Some(&b'='))
        }
        Some(b'/' | b'%' | b'^') => bytes.get(after + 1) != Some(&b'='),
        Some(b'&') => {
            bytes.get(after + 1) != Some(&b'=')
                && !(bytes.get(after + 1) == Some(&b'&') && bytes.get(after + 2) == Some(&b'='))
        }
        Some(b'|') => {
            bytes.get(after + 1) != Some(&b'=')
                && !(bytes.get(after + 1) == Some(&b'|') && bytes.get(after + 2) == Some(&b'='))
        }
        Some(b'<') => !(bytes.get(after + 1) == Some(&b'<') && bytes.get(after + 2) == Some(&b'=')),
        Some(b'>') => {
            !(bytes.get(after + 1) == Some(&b'>')
                && (bytes.get(after + 2) == Some(&b'=')
                    || (bytes.get(after + 2) == Some(&b'>')
                        && bytes.get(after + 3) == Some(&b'='))))
        }
        Some(b'?') => !(bytes.get(after + 1) == Some(&b'?') && bytes.get(after + 2) == Some(&b'=')),
        _ => true,
    }
}

fn planned_runtime_helper_consumed_bindings(
    plan: &EmitPlan,
    source_file_id: u32,
) -> BTreeSet<BindingName> {
    let helper_path = runtime_helpers_path(source_file_id);
    let mut consumed = BTreeSet::<BindingName>::new();
    for file in &plan.files {
        let specifier = relative_import_specifier(file.path.as_str(), helper_path.as_str());
        for source in &file.body {
            if let Some((bindings, import_specifier)) =
                parse_generated_named_import_statement(source)
                && import_specifier == specifier
            {
                consumed.extend(bindings);
                continue;
            }
            if let Some((bindings, reexport_specifier)) =
                parse_generated_named_reexport_statement(source)
                && reexport_specifier == specifier
            {
                consumed.extend(bindings);
            }
        }
    }
    consumed
}

fn emit_direct_owner_imports(
    program: &EnrichedProgram,
    module_id: ModuleId,
    module_path: &str,
    file: &mut PlannedFile,
    planned_bindings: &mut BTreeSet<BindingName>,
    direct_imports: &BTreeMap<ModuleId, BTreeSet<BindingName>>,
) {
    for (owner_module, bindings) in direct_imports {
        let Some(owner_path) = module_output_path(program, *owner_module) else {
            continue;
        };
        let bindings = bindings
            .iter()
            .filter(|binding| !planned_bindings.contains(*binding))
            .cloned()
            .collect::<BTreeSet<_>>();
        if bindings.is_empty() {
            continue;
        }
        let specifier = relative_import_specifier(module_path, owner_path.as_str());
        file.push_source(named_import_statement(bindings.iter(), specifier.as_str()));
        for binding in bindings {
            planned_bindings.insert(binding.clone());
            file.add_binding(plan_binding_from_program(
                program,
                module_id,
                binding.clone(),
                binding,
                true,
                None,
            ));
        }
    }
}

fn emit_direct_prelude_imports(
    file: &mut PlannedFile,
    planned_bindings: &mut BTreeSet<BindingName>,
    direct_imports: &BTreeMap<BindingName, RuntimePreludeDirectImport>,
) {
    if direct_imports.is_empty() {
        return;
    }

    let mut named_by_source = BTreeMap::<String, BTreeSet<(String, BindingName)>>::new();
    for (binding, import) in direct_imports {
        if planned_bindings.contains(binding) {
            continue;
        }
        match &import.kind {
            RuntimePreludeDirectImportKind::Default => {
                file.push_source(default_import_statement(binding, import.source.as_str()));
            }
            RuntimePreludeDirectImportKind::Namespace => {
                file.push_source(namespace_import_statement(binding, import.source.as_str()));
            }
            RuntimePreludeDirectImportKind::Named { imported } => {
                named_by_source
                    .entry(import.source.clone())
                    .or_default()
                    .insert((imported.clone(), binding.clone()));
            }
        }
        planned_bindings.insert(binding.clone());
        file.add_binding(PlannedBinding::new(
            binding.clone(),
            binding.clone(),
            BindingShape::Unknown,
            true,
        ));
    }

    for (source, specifiers) in named_by_source {
        file.push_source(named_import_alias_statement(
            specifiers
                .iter()
                .map(|(imported, local)| (imported.as_str(), local)),
            source.as_str(),
        ));
    }
}

fn runtime_prelude_direct_imports(
    program: &EnrichedProgram,
) -> BTreeMap<u32, BTreeMap<BindingName, RuntimePreludeDirectImport>> {
    program
        .model()
        .graph()
        .runtime_preludes()
        .iter()
        .filter_map(|(source_file_id, prelude)| {
            let imports = runtime_prelude_direct_imports_for_prelude(prelude);
            (!imports.is_empty()).then_some((*source_file_id, imports))
        })
        .collect()
}

fn runtime_prelude_direct_imports_for_prelude(
    prelude: &RuntimePrelude,
) -> BTreeMap<BindingName, RuntimePreludeDirectImport> {
    let mut imports = BTreeMap::<BindingName, RuntimePreludeDirectImport>::new();
    for (binding, snippet) in &prelude.snippets {
        let Some(mut import) =
            parse_runtime_prelude_direct_import(snippet.source.as_str(), binding)
        else {
            continue;
        };
        import.snippet_source = snippet.source.clone();
        import.snippet_byte_start = snippet.byte_start;
        imports.entry(binding.clone()).or_insert(import);
    }
    imports
}

fn runtime_edge_direct_prelude_imports(
    program: &EnrichedProgram,
    lowered_runtime_sources: &BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
    runtime_lazy_folds: &RuntimeLazyFoldPlan,
    direct_imports: &BTreeMap<u32, BTreeMap<BindingName, RuntimePreludeDirectImport>>,
) -> BTreeMap<u32, BTreeSet<BindingName>> {
    let consumers = runtime_prelude_direct_import_consumers(program, lowered_runtime_sources);
    let mut allowed = BTreeMap::<u32, BTreeSet<BindingName>>::new();

    for (source_file_id, imports) in direct_imports {
        let Some(prelude) = program.model().graph().runtime_prelude(*source_file_id) else {
            continue;
        };
        let mut imports_by_snippet = BTreeMap::<u32, BTreeSet<BindingName>>::new();
        let mut snippets_by_import_source = BTreeMap::<String, BTreeSet<u32>>::new();
        for (binding, import) in imports {
            imports_by_snippet
                .entry(import.snippet_byte_start)
                .or_default()
                .insert(binding.clone());
            snippets_by_import_source
                .entry(import.source.clone())
                .or_default()
                .insert(import.snippet_byte_start);
        }

        for bindings in imports_by_snippet.values() {
            let Some(first_binding) = bindings.first() else {
                continue;
            };
            let Some(first_import) = imports.get(first_binding) else {
                continue;
            };
            if snippets_by_import_source
                .get(&first_import.source)
                .is_some_and(|snippets| snippets.len() > 1)
            {
                continue;
            }
            if bindings.iter().any(|binding| {
                imports.get(binding).is_some_and(|import| {
                    matches!(import.kind, RuntimePreludeDirectImportKind::Namespace)
                })
            }) {
                continue;
            }
            let Some(statement_bindings) =
                import_statement_local_bindings(first_import.snippet_source.as_str())
            else {
                continue;
            };
            if statement_bindings != *bindings {
                continue;
            }
            if bindings.iter().any(|binding| {
                runtime_prelude_import_has_runtime_reader(
                    prelude,
                    runtime_lazy_folds,
                    *source_file_id,
                    binding,
                    first_import.snippet_byte_start,
                )
            }) {
                continue;
            }
            if !runtime_prelude_direct_import_group_is_profitable(
                *source_file_id,
                bindings,
                imports,
                &consumers,
            ) {
                continue;
            }
            allowed
                .entry(*source_file_id)
                .or_default()
                .extend(bindings.iter().cloned());
        }
    }

    allowed
}

fn runtime_prelude_direct_import_consumers(
    program: &EnrichedProgram,
    lowered_runtime_sources: &BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
) -> BTreeMap<(u32, BindingName), BTreeSet<ModuleId>> {
    let mut consumers = BTreeMap::<(u32, BindingName), BTreeSet<ModuleId>>::new();
    for module in program.model().modules() {
        for import in program.model().graph().runtime_imports_for(module.id) {
            consumers
                .entry((import.source_file_id, import.binding))
                .or_default()
                .insert(module.id);
        }
        if let Some(source) = lowered_runtime_sources.get(&module.id) {
            for binding in &source.remaining_helpers {
                consumers
                    .entry((source.source_file_id, binding.clone()))
                    .or_default()
                    .insert(module.id);
            }
        }
    }
    consumers
}

fn runtime_prelude_import_has_runtime_reader(
    prelude: &RuntimePrelude,
    runtime_lazy_folds: &RuntimeLazyFoldPlan,
    source_file_id: u32,
    binding: &BindingName,
    import_snippet_byte_start: u32,
) -> bool {
    for snippet in prelude.snippets.values() {
        if snippet.byte_start == import_snippet_byte_start {
            continue;
        }
        if identifiers_in_source(snippet.source.as_str())
            .into_iter()
            .any(|identifier| identifier == binding.as_str())
        {
            return true;
        }
    }
    for namespace_export in &prelude.namespace_exports {
        if namespace_export.namespace == *binding
            || namespace_export
                .exports
                .values()
                .any(|export| export == binding)
        {
            return true;
        }
    }
    if let Some(entrypoint) = &prelude.entrypoint {
        if entrypoint.callee == *binding {
            return true;
        }
        for side_effect in runtime_entrypoint_side_effects(prelude, entrypoint) {
            if identifiers_in_source(side_effect.source.as_str())
                .into_iter()
                .any(|identifier| identifier == binding.as_str())
            {
                return true;
            }
        }
    }
    runtime_lazy_folds
        .chunks_by_source_file
        .get(&source_file_id)
        .is_some_and(|chunks| {
            chunks.iter().any(|chunk| {
                identifiers_in_source(chunk.source.as_str())
                    .into_iter()
                    .any(|identifier| identifier == binding.as_str())
            })
        })
}

fn runtime_prelude_direct_import_group_is_profitable(
    source_file_id: u32,
    bindings: &BTreeSet<BindingName>,
    imports: &BTreeMap<BindingName, RuntimePreludeDirectImport>,
    consumers: &BTreeMap<(u32, BindingName), BTreeSet<ModuleId>>,
) -> bool {
    let Some(first_binding) = bindings.first() else {
        return false;
    };
    let Some(first_import) = imports.get(first_binding) else {
        return false;
    };
    let mut saved = first_import.snippet_source.len();
    let mut added = BTreeMap::<ModuleId, BTreeMap<String, BTreeSet<(String, BindingName)>>>::new();
    let mut default_or_namespace_imports = Vec::<(ModuleId, &RuntimePreludeDirectImport)>::new();
    for binding in bindings {
        saved += binding.as_str().len() + 2; // runtime export list
        let Some(import) = imports.get(binding) else {
            return false;
        };
        let binding_consumers = consumers
            .get(&(source_file_id, binding.clone()))
            .cloned()
            .unwrap_or_default();
        if binding_consumers.is_empty() {
            return false;
        }
        for module_id in binding_consumers {
            saved += binding.as_str().len() + 2; // runtime import list in the consumer
            match &import.kind {
                RuntimePreludeDirectImportKind::Named { imported } => {
                    added
                        .entry(module_id)
                        .or_default()
                        .entry(import.source.clone())
                        .or_default()
                        .insert((imported.clone(), binding.clone()));
                }
                RuntimePreludeDirectImportKind::Default
                | RuntimePreludeDirectImportKind::Namespace => {
                    default_or_namespace_imports.push((module_id, import));
                }
            }
        }
    }

    let mut added_bytes = 0usize;
    for (_module_id, imports_by_source) in added {
        for (source, specifiers) in imports_by_source {
            added_bytes += named_import_alias_statement(
                specifiers
                    .iter()
                    .map(|(imported, local)| (imported.as_str(), local)),
                source.as_str(),
            )
            .len();
        }
    }
    for (_module_id, import) in default_or_namespace_imports {
        added_bytes += match import.kind {
            RuntimePreludeDirectImportKind::Default => default_import_statement(
                first_local_for_import(bindings, imports, import),
                &import.source,
            )
            .len(),
            RuntimePreludeDirectImportKind::Namespace => namespace_import_statement(
                first_local_for_import(bindings, imports, import),
                &import.source,
            )
            .len(),
            RuntimePreludeDirectImportKind::Named { .. } => 0,
        };
    }

    saved > added_bytes
}

fn first_local_for_import<'a>(
    bindings: &'a BTreeSet<BindingName>,
    imports: &'a BTreeMap<BindingName, RuntimePreludeDirectImport>,
    target: &RuntimePreludeDirectImport,
) -> &'a BindingName {
    bindings
        .iter()
        .find(|binding| imports.get(*binding).is_some_and(|import| import == target))
        .unwrap_or_else(|| bindings.first().expect("non-empty bindings"))
}

fn import_statement_local_bindings(source: &str) -> Option<BTreeSet<BindingName>> {
    let source = source.trim();
    let rest = source.strip_prefix("import ")?;
    if rest.starts_with("type ") {
        return None;
    }
    let rest = rest.strip_suffix(';')?.trim();
    if rest.contains(" with ") || rest.contains(" assert ") {
        return None;
    }
    let (clause, specifier) = split_import_clause_and_specifier(rest)?;
    if !is_bare_import_specifier(specifier) {
        return None;
    }
    let mut bindings = BTreeSet::<BindingName>::new();
    if let Some(namespace) = parse_namespace_import_clause(clause) {
        bindings.insert(BindingName::new(namespace));
        return Some(bindings);
    }
    let (default_part, rest) = split_default_import_clause(clause);
    if let Some(default_part) = default_part {
        bindings.insert(BindingName::new(default_part));
    }
    if let Some(rest) = rest {
        if let Some(namespace) = parse_namespace_import_clause(rest) {
            bindings.insert(BindingName::new(namespace));
        } else {
            for (_imported, local) in parse_named_import_clause(rest)? {
                bindings.insert(BindingName::new(local));
            }
        }
    }
    Some(bindings)
}

fn parse_runtime_prelude_direct_import(
    source: &str,
    binding: &BindingName,
) -> Option<RuntimePreludeDirectImport> {
    let source = source.trim();
    let rest = source.strip_prefix("import ")?;
    if rest.starts_with("type ") {
        return None;
    }
    let rest = rest.strip_suffix(';')?.trim();
    if rest.contains(" with ") || rest.contains(" assert ") {
        return None;
    }
    let (clause, specifier) = split_import_clause_and_specifier(rest)?;
    if !is_bare_import_specifier(specifier) {
        return None;
    }
    parse_import_clause_for_binding(clause, binding).map(|kind| RuntimePreludeDirectImport {
        source: specifier.to_string(),
        snippet_source: source.to_string(),
        snippet_byte_start: 0,
        kind,
    })
}

fn split_import_clause_and_specifier(rest: &str) -> Option<(&str, &str)> {
    for delimiter in [" from '", " from \""] {
        let Some((clause, tail)) = rest.rsplit_once(delimiter) else {
            continue;
        };
        let quote = delimiter.as_bytes().last().copied()? as char;
        let specifier = tail.strip_suffix(quote)?;
        return Some((clause.trim(), specifier));
    }
    None
}

fn parse_import_clause_for_binding(
    clause: &str,
    binding: &BindingName,
) -> Option<RuntimePreludeDirectImportKind> {
    let binding = binding.as_str();
    if let Some(namespace) = parse_namespace_import_clause(clause)
        && namespace == binding
    {
        return Some(RuntimePreludeDirectImportKind::Namespace);
    }

    let (default_part, rest) = split_default_import_clause(clause);
    if let Some(default_part) = default_part
        && default_part == binding
    {
        return Some(RuntimePreludeDirectImportKind::Default);
    }

    let rest = rest?;
    if let Some(namespace) = parse_namespace_import_clause(rest)
        && namespace == binding
    {
        return Some(RuntimePreludeDirectImportKind::Namespace);
    }

    for (imported, local) in parse_named_import_clause(rest)? {
        if local == binding {
            return Some(RuntimePreludeDirectImportKind::Named { imported });
        }
    }
    None
}

fn parse_namespace_import_clause(clause: &str) -> Option<&str> {
    let local = clause.trim().strip_prefix("* as ")?.trim();
    is_identifier_like(local).then_some(local)
}

fn split_default_import_clause(clause: &str) -> (Option<&str>, Option<&str>) {
    let clause = clause.trim();
    if clause.starts_with('{') || clause.starts_with("* as ") {
        return (None, Some(clause));
    }
    let (default_part, rest) = clause
        .split_once(',')
        .map_or((clause, None), |(default_part, rest)| {
            (default_part, Some(rest.trim()))
        });
    let default_part = default_part.trim();
    if is_identifier_like(default_part) {
        (Some(default_part), rest)
    } else {
        (None, rest)
    }
}

fn parse_named_import_clause(clause: &str) -> Option<Vec<(String, String)>> {
    let clause = clause.trim();
    let inner = clause.strip_prefix('{')?.strip_suffix('}')?.trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    let mut specifiers = Vec::new();
    for raw in inner.split(',') {
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with("type ") {
            return None;
        }
        let (imported, local) = raw
            .split_once(" as ")
            .map_or((raw, raw), |(imported, local)| {
                (imported.trim(), local.trim())
            });
        if !is_identifier_like(imported) || !is_identifier_like(local) {
            return None;
        }
        specifiers.push((imported.to_string(), local.to_string()));
    }
    Some(specifiers)
}

fn is_bare_import_specifier(specifier: &str) -> bool {
    !specifier.is_empty() && !specifier.starts_with('.') && !specifier.starts_with('/')
}

fn is_identifier_like(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && is_identifier_start(bytes[0])
        && bytes[1..].iter().all(|byte| is_identifier_continue(*byte))
}

/// Identify bindings that can move from the shared runtime helpers file
/// to a single owner module. Conservative gating:
///   1. The binding must have a `__reverts_set_X` setter function (i.e.,
///      it's part of the cross-module mutation surface).
///   2. The runtime prelude itself must not REFERENCE the binding outside
///      its own `var X;` declaration and `__reverts_set_X` body. Any
///      other snippet that names X is a runtime read, and migrating X
///      would force the runtime to import it back from the owner — a
///      cycle hazard.
///   3. Exactly one application module must write the binding (via the
///      `written_helpers` set populated during lowering). With multiple
///      writers, no single module can own the binding without one of
///      them needing setter-mediated cross-module access, defeating the
///      migration.
fn compute_runtime_var_migration_plan(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
    lowered_runtime_sources: &BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
    runtime_lazy_folds: &RuntimeLazyFoldPlan,
    externalized_packages: &BTreeSet<ModuleId>,
) -> RuntimeVarMigrationPlan {
    // Modules whose lazy initializer bodies have already been folded
    // into the runtime helper file. Their consumer file is an empty
    // re-export stub; there is no source body to host a migrated
    // declaration or to absorb same-module assignments. Skip them.
    let folded_modules: BTreeSet<ModuleId> = runtime_lazy_folds.modules.keys().copied().collect();
    let source_definition_modules =
        unique_source_definition_modules(program, externalized_packages);
    let module_dependencies_by_owner = module_dependency_modules_by_owner(program);
    let source_local_definitions_by_module = lowered_runtime_sources
        .iter()
        .map(|(module_id, source)| (*module_id, source.local_definitions.clone()))
        .collect::<BTreeMap<_, _>>();
    // Invert `written_helpers` to find single-writer bindings — but
    // exclude writes that came from a module that was later folded.
    let mut writers: BTreeMap<BindingName, BTreeSet<(ModuleId, u32)>> = BTreeMap::new();
    for (module_id, source) in lowered_runtime_sources {
        if folded_modules.contains(module_id) {
            continue;
        }
        let Some(module) = program
            .model()
            .modules()
            .iter()
            .find(|module| module.id == *module_id)
        else {
            continue;
        };
        if module.kind == ModuleKind::Package && externalized_packages.contains(module_id) {
            continue;
        }
        for binding in &source.written_helpers {
            writers
                .entry(binding.clone())
                .or_default()
                .insert((*module_id, source.source_file_id));
        }
    }
    let single_writers: BTreeMap<BindingName, (ModuleId, u32)> = writers
        .into_iter()
        .filter_map(|(binding, writers)| {
            if writers.len() == 1 {
                writers.into_iter().next().map(|w| (binding, w))
            } else {
                None
            }
        })
        .collect();
    // Group single-writer candidates by source file id so each runtime
    // prelude is scanned once.
    let mut by_source: BTreeMap<u32, Vec<(BindingName, ModuleId)>> = BTreeMap::new();
    for (binding, (module_id, source_id)) in single_writers {
        by_source
            .entry(source_id)
            .or_default()
            .push((binding, module_id));
    }
    let mut plan = RuntimeVarMigrationPlan::default();
    for (source_id, candidates) in by_source {
        let Some(prelude) = program.model().graph().runtime_prelude(source_id) else {
            continue;
        };
        let candidates = candidates
            .into_iter()
            .filter_map(|(binding, owner_module)| {
                let initializer = migratable_runtime_var_initializer(prelude, &binding)?;
                Some((binding, owner_module, initializer))
            })
            .collect::<Vec<_>>();
        let movable_bindings = candidates
            .iter()
            .map(|(binding, _, _)| binding.clone())
            .collect::<BTreeSet<_>>();
        let candidate_owners = candidates
            .iter()
            .map(|(binding, owner_module, _)| (binding.clone(), *owner_module))
            .collect::<BTreeMap<_, _>>();
        let candidate_initializers = candidates
            .iter()
            .map(|(binding, _, initializer)| (binding.clone(), initializer.clone()))
            .collect::<BTreeMap<_, _>>();
        let folded_chunks = runtime_lazy_folds
            .chunks_by_source_file
            .get(&source_id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let read_index = runtime_source_read_index(prelude, folded_chunks);
        let reader_cluster_context = RuntimeReaderClusterContext {
            owner_available_bindings: runtime_reader_owner_available_bindings(
                program,
                source_module_wiring,
                lowered_runtime_sources,
                candidate_owners.values().copied(),
            ),
            source_definition_modules: &source_definition_modules,
            module_dependencies_by_owner: &module_dependencies_by_owner,
            source_local_definitions_by_module: &source_local_definitions_by_module,
            folded_modules: &folded_modules,
            prelude,
            read_index: &read_index,
            movable_bindings: &movable_bindings,
            candidate_owners: &candidate_owners,
        };
        let mut migrated_primary_bindings = BTreeSet::<BindingName>::new();
        for (binding, owner_module, initializer) in candidates {
            if migrated_primary_bindings.contains(&binding) {
                continue;
            }
            // The binding must have a setter (zero-writer or shared
            // bindings never enter `written_helpers`, but the prelude may
            // also expose source-backed reads for which there is no setter
            // — skip those).
            // The binding must be a real prelude declaration. The
            // migration accepts bare `var X;` and also `var X = LITERAL;`
            // where LITERAL is a side-effect-free literal that the
            // writer can re-emit verbatim. Anything more complex (calls,
            // identifier references, member access) stays put — moving
            // such an initializer would require dragging its
            // dependencies along.
            // The setter function is synthesized by the planner at emit
            // time — it doesn't appear in the prelude snippets. Any
            // OTHER prelude snippet, namespace export, or folded chunk
            // that references X counts as a runtime read.
            let migration = match runtime_binding_read_profile(&read_index, &binding) {
                RuntimeBindingReadProfile::NoReads => RuntimeReaderClusterMigration {
                    primary_bindings: BTreeSet::from([binding.clone()]),
                    extra_snippets: BTreeSet::new(),
                    extra_namespace_exports: BTreeSet::new(),
                    extra_runtime_deps: BTreeSet::new(),
                    extra_source_deps: BTreeMap::new(),
                },
                RuntimeBindingReadProfile::SnippetReaders(readers) => {
                    let Ok(migration) = migratable_runtime_reader_cluster_result(
                        &reader_cluster_context,
                        owner_module,
                        &binding,
                        readers,
                    ) else {
                        continue;
                    };
                    migration
                }
                RuntimeBindingReadProfile::Rejected => continue,
            };
            if migration
                .primary_bindings
                .iter()
                .any(|primary| migrated_primary_bindings.contains(primary))
            {
                continue;
            }
            for primary in &migration.primary_bindings {
                let Some(primary_initializer) = candidate_initializers.get(primary).cloned() else {
                    continue;
                };
                plan.insert(
                    primary.clone(),
                    RuntimeVarMigration {
                        owner_module,
                        source_file_id: source_id,
                        extra_snippets: migration.extra_snippets.clone(),
                        extra_namespace_exports: migration.extra_namespace_exports.clone(),
                        extra_runtime_deps: migration.extra_runtime_deps.clone(),
                        extra_source_deps: migration.extra_source_deps.clone(),
                        initializer: if primary == &binding {
                            initializer.clone()
                        } else {
                            primary_initializer
                        },
                    },
                );
                migrated_primary_bindings.insert(primary.clone());
            }
        }
    }
    plan
}

fn runtime_setter_migration_blocker_report(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
    lowered_runtime_sources: &BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
    runtime_lazy_folds: &RuntimeLazyFoldPlan,
    externalized_packages: &BTreeSet<ModuleId>,
) -> RuntimeSetterMigrationBlockerReport {
    let actual_migrations = compute_runtime_var_migration_plan(
        program,
        source_module_wiring,
        lowered_runtime_sources,
        runtime_lazy_folds,
        externalized_packages,
    );
    let folded_modules: BTreeSet<ModuleId> = runtime_lazy_folds.modules.keys().copied().collect();
    let source_definition_modules =
        unique_source_definition_modules(program, externalized_packages);
    let module_dependencies_by_owner = module_dependency_modules_by_owner(program);
    let source_local_definitions_by_module = lowered_runtime_sources
        .iter()
        .map(|(module_id, source)| (*module_id, source.local_definitions.clone()))
        .collect::<BTreeMap<_, _>>();
    let modules_by_id = program
        .model()
        .modules()
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let mut eligible_writers = BTreeMap::<(u32, BindingName), BTreeSet<ModuleId>>::new();
    let mut excluded_folded = BTreeMap::<(u32, BindingName), BTreeSet<ModuleId>>::new();
    let mut excluded_externalized = BTreeMap::<(u32, BindingName), BTreeSet<ModuleId>>::new();

    for (module_id, source) in lowered_runtime_sources {
        for binding in &source.written_helpers {
            let key = (source.source_file_id, binding.clone());
            if folded_modules.contains(module_id) {
                excluded_folded.entry(key).or_default().insert(*module_id);
                continue;
            }
            if modules_by_id.get(module_id).is_some_and(|module| {
                module.kind == ModuleKind::Package && externalized_packages.contains(module_id)
            }) {
                excluded_externalized
                    .entry(key)
                    .or_default()
                    .insert(*module_id);
                continue;
            }
            eligible_writers.entry(key).or_default().insert(*module_id);
        }
    }

    let all_keys = eligible_writers
        .keys()
        .chain(excluded_folded.keys())
        .chain(excluded_externalized.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut report = RuntimeSetterMigrationBlockerReport {
        total_bindings: all_keys.len(),
        ..Default::default()
    };

    let mut by_source = BTreeMap::<u32, Vec<(BindingName, ModuleId)>>::new();
    for (source_id, binding) in &all_keys {
        if actual_migrations
            .migrations_by_binding
            .get(binding)
            .is_some_and(|migration| migration.source_file_id == *source_id)
        {
            report.add_accepted(*source_id, binding.clone());
            continue;
        }
        let eligible = eligible_writers
            .get(&(*source_id, binding.clone()))
            .cloned()
            .unwrap_or_default();
        if eligible.is_empty() {
            if excluded_folded.contains_key(&(*source_id, binding.clone())) {
                report.add_reason(
                    *source_id,
                    binding.clone(),
                    RuntimeSetterMigrationBlockerReason::FoldedWriterOnly,
                );
            } else if excluded_externalized.contains_key(&(*source_id, binding.clone())) {
                report.add_reason(
                    *source_id,
                    binding.clone(),
                    RuntimeSetterMigrationBlockerReason::ExternalizedPackageWriterOnly,
                );
            } else {
                report.add_reason(
                    *source_id,
                    binding.clone(),
                    RuntimeSetterMigrationBlockerReason::NoEligibleWriter,
                );
            }
            continue;
        }
        if eligible.len() > 1 {
            report.add_reason(
                *source_id,
                binding.clone(),
                RuntimeSetterMigrationBlockerReason::MultipleEligibleWriters,
            );
            continue;
        }
        let owner_module = *eligible
            .iter()
            .next()
            .expect("single eligible writer must be present");
        by_source
            .entry(*source_id)
            .or_default()
            .push((binding.clone(), owner_module));
    }

    for (source_id, candidates) in by_source {
        let Some(prelude) = program.model().graph().runtime_prelude(source_id) else {
            for (binding, _owner_module) in candidates {
                report.add_reason(
                    source_id,
                    binding,
                    RuntimeSetterMigrationBlockerReason::MissingRuntimePrelude,
                );
            }
            continue;
        };
        let mut initialized_candidates = Vec::<(BindingName, ModuleId)>::new();
        for (binding, owner_module) in candidates {
            if migratable_runtime_var_initializer(prelude, &binding).is_none() {
                report.add_reason(
                    source_id,
                    binding,
                    RuntimeSetterMigrationBlockerReason::InitializerNotMigratable,
                );
                continue;
            }
            initialized_candidates.push((binding, owner_module));
        }
        let movable_bindings = initialized_candidates
            .iter()
            .map(|(binding, _)| binding.clone())
            .collect::<BTreeSet<_>>();
        let candidate_owners = initialized_candidates
            .iter()
            .map(|(binding, owner_module)| (binding.clone(), *owner_module))
            .collect::<BTreeMap<_, _>>();
        let folded_chunks = runtime_lazy_folds
            .chunks_by_source_file
            .get(&source_id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let read_index = runtime_source_read_index(prelude, folded_chunks);
        let reader_cluster_context = RuntimeReaderClusterContext {
            owner_available_bindings: runtime_reader_owner_available_bindings(
                program,
                source_module_wiring,
                lowered_runtime_sources,
                candidate_owners.values().copied(),
            ),
            source_definition_modules: &source_definition_modules,
            module_dependencies_by_owner: &module_dependencies_by_owner,
            source_local_definitions_by_module: &source_local_definitions_by_module,
            folded_modules: &folded_modules,
            prelude,
            read_index: &read_index,
            movable_bindings: &movable_bindings,
            candidate_owners: &candidate_owners,
        };
        let mut reported_primary_bindings = BTreeSet::<BindingName>::new();
        for (binding, owner_module) in initialized_candidates {
            if reported_primary_bindings.contains(&binding) {
                continue;
            }
            match runtime_binding_read_profile_diagnostic(&read_index, &binding) {
                Ok(RuntimeBindingReadProfile::NoReads) => {
                    report.add_reason(
                        source_id,
                        binding.clone(),
                        RuntimeSetterMigrationBlockerReason::NoDiagnosticStatus,
                    );
                    reported_primary_bindings.insert(binding);
                }
                Ok(RuntimeBindingReadProfile::SnippetReaders(readers)) => {
                    let cluster_result = migratable_runtime_reader_cluster_result(
                        &reader_cluster_context,
                        owner_module,
                        &binding,
                        readers,
                    );
                    match cluster_result {
                        Ok(_) => report.add_reason(
                            source_id,
                            binding,
                            RuntimeSetterMigrationBlockerReason::ReaderClusterOverlapsMigratedBinding,
                        ),
                        Err(reason) => report.add_reason(source_id, binding, reason.into()),
                    }
                }
                Ok(RuntimeBindingReadProfile::Rejected) => {
                    unreachable!("diagnostic read profile returns Err for rejected bindings");
                }
                Err(reason) => report.add_reason(source_id, binding, reason),
            }
        }
    }

    debug_assert_eq!(
        report.total_bindings,
        report.accepted_bindings + report.blocked_bindings
    );
    report
}

fn migratable_runtime_var_initializer(
    prelude: &RuntimePrelude,
    binding: &BindingName,
) -> Option<Option<String>> {
    let snippet = prelude.snippets.get(binding)?;
    classify_migratable_var_declaration(snippet.source.as_str(), binding.as_str())
        .map(|initializer| initializer.map(str::to_string))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RuntimeBindingReadProfile {
    NoReads,
    SnippetReaders(BTreeSet<BindingName>),
    Rejected,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct RuntimeSourceReadIndex {
    snippet_readers_by_binding: BTreeMap<BindingName, BTreeSet<BindingName>>,
    namespace_readers_by_binding: BTreeMap<BindingName, BTreeSet<BindingName>>,
    namespace_exports_by_namespace: BTreeMap<BindingName, RuntimeNamespaceExport>,
    namespace_export_helpers: BTreeSet<BindingName>,
    free_bindings_by_snippet: BTreeMap<BindingName, BTreeSet<BindingName>>,
    non_snippet_runtime_reads: BTreeSet<BindingName>,
    entrypoint_callee: Option<BindingName>,
}

fn runtime_source_read_index(
    prelude: &RuntimePrelude,
    folded_chunks: &[RuntimeFoldedSourceChunk],
) -> RuntimeSourceReadIndex {
    let mut index = RuntimeSourceReadIndex {
        entrypoint_callee: prelude
            .entrypoint
            .as_ref()
            .map(|entrypoint| entrypoint.callee.clone()),
        ..RuntimeSourceReadIndex::default()
    };
    for (key, snippet) in &prelude.snippets {
        let free_bindings = runtime_import_identifiers_in_source(snippet.source.as_str())
            .into_iter()
            .map(BindingName::new)
            .collect::<BTreeSet<_>>();
        for binding in &free_bindings {
            index
                .snippet_readers_by_binding
                .entry(binding.clone())
                .or_default()
                .insert(key.clone());
        }
        index
            .free_bindings_by_snippet
            .insert(key.clone(), free_bindings);
    }

    for chunk in folded_chunks {
        index.non_snippet_runtime_reads.extend(
            runtime_import_identifiers_in_source(chunk.source.as_str())
                .into_iter()
                .map(BindingName::new),
        );
    }

    if let Some(entrypoint) = &prelude.entrypoint {
        for side_effect in &entrypoint.side_effects {
            index.non_snippet_runtime_reads.extend(
                runtime_import_identifiers_in_source(side_effect.source.as_str())
                    .into_iter()
                    .map(BindingName::new),
            );
        }
    }

    for namespace_export in &prelude.namespace_exports {
        index
            .namespace_export_helpers
            .insert(namespace_export.helper.clone());
        index
            .namespace_exports_by_namespace
            .insert(namespace_export.namespace.clone(), namespace_export.clone());
        for target in namespace_export.exports.values() {
            index
                .namespace_readers_by_binding
                .entry(target.clone())
                .or_default()
                .insert(namespace_export.namespace.clone());
        }
    }

    index
}

fn runtime_binding_read_profile(
    read_index: &RuntimeSourceReadIndex,
    binding: &BindingName,
) -> RuntimeBindingReadProfile {
    if runtime_binding_read_profile_diagnostic(read_index, binding).is_err() {
        return RuntimeBindingReadProfile::Rejected;
    }

    let readers = runtime_readers_for_binding(read_index, binding);
    if readers.is_empty() {
        RuntimeBindingReadProfile::NoReads
    } else {
        RuntimeBindingReadProfile::SnippetReaders(readers)
    }
}

fn runtime_binding_read_profile_diagnostic(
    read_index: &RuntimeSourceReadIndex,
    binding: &BindingName,
) -> Result<RuntimeBindingReadProfile, RuntimeSetterMigrationBlockerReason> {
    if read_index.non_snippet_runtime_reads.contains(binding) {
        return Err(RuntimeSetterMigrationBlockerReason::RuntimeNonSnippetRead);
    }
    if read_index.namespace_export_helpers.contains(binding) {
        return Err(RuntimeSetterMigrationBlockerReason::RuntimeNamespaceExportHelper);
    }
    if read_index
        .namespace_exports_by_namespace
        .contains_key(binding)
    {
        return Err(RuntimeSetterMigrationBlockerReason::RuntimeNamespaceObjectBinding);
    }

    let readers = runtime_readers_for_binding(read_index, binding);
    if readers.is_empty() {
        Ok(RuntimeBindingReadProfile::NoReads)
    } else {
        Ok(RuntimeBindingReadProfile::SnippetReaders(readers))
    }
}

fn runtime_readers_for_binding(
    read_index: &RuntimeSourceReadIndex,
    binding: &BindingName,
) -> BTreeSet<BindingName> {
    let mut readers = read_index
        .snippet_readers_by_binding
        .get(binding)
        .cloned()
        .unwrap_or_default();
    readers.extend(
        read_index
            .namespace_readers_by_binding
            .get(binding)
            .into_iter()
            .flatten()
            .cloned(),
    );
    readers
}

struct RuntimeReaderClusterContext<'a> {
    owner_available_bindings: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    source_definition_modules: &'a BTreeMap<BindingName, Option<ModuleId>>,
    module_dependencies_by_owner: &'a BTreeMap<ModuleId, BTreeSet<ModuleId>>,
    source_local_definitions_by_module: &'a BTreeMap<ModuleId, BTreeSet<BindingName>>,
    folded_modules: &'a BTreeSet<ModuleId>,
    prelude: &'a RuntimePrelude,
    read_index: &'a RuntimeSourceReadIndex,
    movable_bindings: &'a BTreeSet<BindingName>,
    candidate_owners: &'a BTreeMap<BindingName, ModuleId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeReaderClusterBlocker {
    NonSnippetUse,
    MissingSnippet,
    InvalidNamespaceSnippet,
    InvalidReaderFunctionSnippet,
    RuntimeGlobalWrite,
    ClosureEscapes,
    MissingFreeBindingIndex,
    ReadsOtherMovableBinding,
    ReadsNonRuntimeBinding,
    NamespaceTargetDifferentWriter,
    OwnerSourceMissing,
    OwnerNameConflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeReaderClusterMigration {
    /// Primary runtime vars that can move as one same-writer component.
    /// This always includes the binding that seeded the cluster; it may
    /// also include other movable vars read by the same reader closure
    /// when they have the same owner module.
    primary_bindings: BTreeSet<BindingName>,
    extra_snippets: BTreeSet<BindingName>,
    extra_namespace_exports: BTreeSet<BindingName>,
    extra_runtime_deps: BTreeSet<BindingName>,
    extra_source_deps: BTreeMap<ModuleId, BTreeSet<BindingName>>,
}

type RuntimeReaderClusterResult =
    Result<RuntimeReaderClusterMigration, RuntimeReaderClusterBlocker>;

impl From<RuntimeReaderClusterBlocker> for RuntimeSetterMigrationBlockerReason {
    fn from(reason: RuntimeReaderClusterBlocker) -> Self {
        match reason {
            RuntimeReaderClusterBlocker::NonSnippetUse => Self::ReaderNonSnippetUse,
            RuntimeReaderClusterBlocker::MissingSnippet => Self::ReaderSnippetMissing,
            RuntimeReaderClusterBlocker::InvalidNamespaceSnippet
            | RuntimeReaderClusterBlocker::InvalidReaderFunctionSnippet => {
                Self::ReaderNotMovableShape
            }
            RuntimeReaderClusterBlocker::RuntimeGlobalWrite => Self::ReaderWritesRuntimeBinding,
            RuntimeReaderClusterBlocker::ClosureEscapes => Self::ReaderClosureEscapes,
            RuntimeReaderClusterBlocker::MissingFreeBindingIndex => {
                Self::ReaderFreeBindingIndexMissing
            }
            RuntimeReaderClusterBlocker::ReadsOtherMovableBinding => {
                Self::ReaderReadsOtherMovableBinding
            }
            RuntimeReaderClusterBlocker::ReadsNonRuntimeBinding => {
                Self::ReaderReadsNonRuntimeBinding
            }
            RuntimeReaderClusterBlocker::NamespaceTargetDifferentWriter => {
                Self::NamespaceTargetDifferentWriter
            }
            RuntimeReaderClusterBlocker::OwnerSourceMissing => Self::OwnerSourceMissing,
            RuntimeReaderClusterBlocker::OwnerNameConflict => Self::OwnerNameConflict,
        }
    }
}

fn migratable_runtime_reader_cluster_result(
    ctx: &RuntimeReaderClusterContext<'_>,
    owner_module: ModuleId,
    binding: &BindingName,
    initial_readers: BTreeSet<BindingName>,
) -> RuntimeReaderClusterResult {
    let mut primary_bindings = BTreeSet::<BindingName>::from([binding.clone()]);
    if initial_readers.is_empty() {
        return Ok(RuntimeReaderClusterMigration {
            primary_bindings,
            extra_snippets: BTreeSet::new(),
            extra_namespace_exports: BTreeSet::new(),
            extra_runtime_deps: BTreeSet::new(),
            extra_source_deps: BTreeMap::new(),
        });
    }

    let owner_available_bindings = owner_declared_or_imported_bindings(ctx, owner_module)?;
    let mut moved_snippets = BTreeSet::<BindingName>::new();
    let mut moved_namespace_exports = BTreeSet::<BindingName>::new();
    let mut extra_runtime_deps = BTreeSet::<BindingName>::new();
    let mut extra_source_deps = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
    let mut queue = initial_readers.into_iter().collect::<Vec<_>>();
    while let Some(reader) = queue.pop() {
        if !moved_snippets.insert(reader.clone()) {
            continue;
        }
        if runtime_binding_has_blocking_non_snippet_use(ctx.read_index, &reader) {
            return Err(RuntimeReaderClusterBlocker::NonSnippetUse);
        }
        let snippet = ctx
            .prelude
            .snippets
            .get(&reader)
            .ok_or(RuntimeReaderClusterBlocker::MissingSnippet)?;
        let is_namespace_reader = ctx
            .read_index
            .namespace_exports_by_namespace
            .contains_key(&reader);
        if is_namespace_reader {
            if !is_migratable_namespace_reader_snippet(&reader, snippet.source.as_str()) {
                return Err(RuntimeReaderClusterBlocker::InvalidNamespaceSnippet);
            }
            moved_namespace_exports.insert(reader.clone());
        } else if !is_migratable_reader_function_snippet(&reader, snippet.source.as_str()) {
            return Err(RuntimeReaderClusterBlocker::InvalidReaderFunctionSnippet);
        }
        // Moving a function that mutates runtime globals would either try to
        // assign to an imported binding in the writer module or silently split
        // mutable state. Keep the first cluster pass read-only.
        if implicit_global_writes_in_source(snippet.source.as_str())
            .into_iter()
            .any(|write| ctx.prelude.defines(&write) || write == *binding)
        {
            return Err(RuntimeReaderClusterBlocker::RuntimeGlobalWrite);
        }

        // Any runtime snippet that still reads this newly moved function would
        // force runtime -> owner imports. Pull function dependents into the same
        // cluster instead; reject later if one of them is not a simple function
        // declaration.
        for dependent in runtime_readers_for_binding(ctx.read_index, &reader) {
            if dependent != reader && !moved_snippets.contains(&dependent) {
                queue.push(dependent);
            }
        }

        let free_runtime_bindings = ctx
            .read_index
            .free_bindings_by_snippet
            .get(&reader)
            .ok_or(RuntimeReaderClusterBlocker::MissingFreeBindingIndex)?;
        for dep in free_runtime_bindings {
            if primary_bindings.contains(dep) || moved_snippets.contains(dep) {
                continue;
            }
            if ctx.movable_bindings.contains(dep) {
                let Some(dep_owner) = ctx.candidate_owners.get(dep) else {
                    return Err(RuntimeReaderClusterBlocker::ReadsOtherMovableBinding);
                };
                if *dep_owner != owner_module {
                    return Err(RuntimeReaderClusterBlocker::ReadsOtherMovableBinding);
                }
                let dep_readers = match runtime_binding_read_profile_diagnostic(ctx.read_index, dep)
                {
                    Ok(RuntimeBindingReadProfile::NoReads) => BTreeSet::new(),
                    Ok(RuntimeBindingReadProfile::SnippetReaders(readers)) => readers,
                    Ok(RuntimeBindingReadProfile::Rejected) => {
                        return Err(RuntimeReaderClusterBlocker::ReadsOtherMovableBinding);
                    }
                    Err(_reason) => {
                        return Err(RuntimeReaderClusterBlocker::ReadsOtherMovableBinding);
                    }
                };
                if primary_bindings.insert(dep.clone()) {
                    for dep_reader in dep_readers {
                        if !moved_snippets.contains(&dep_reader) {
                            queue.push(dep_reader);
                        }
                    }
                }
                continue;
            }
            if !ctx.prelude.defines(dep) {
                if owner_available_bindings.contains(dep) {
                    continue;
                }
                if let Some(dep_module) = runtime_reader_source_dependency(ctx, owner_module, dep) {
                    extra_source_deps
                        .entry(dep_module)
                        .or_default()
                        .insert(dep.clone());
                    continue;
                }
                return Err(RuntimeReaderClusterBlocker::ReadsNonRuntimeBinding);
            }
            extra_runtime_deps.insert(dep.clone());
        }
    }

    if !primary_bindings.iter().all(|primary| {
        runtime_readers_for_binding(ctx.read_index, primary)
            .into_iter()
            .all(|reader| moved_snippets.contains(&reader))
    }) {
        return Err(RuntimeReaderClusterBlocker::ClosureEscapes);
    }

    if !moved_snippets.iter().all(|snippet| {
        runtime_readers_for_binding(ctx.read_index, snippet)
            .into_iter()
            .all(|reader| reader == *snippet || moved_snippets.contains(&reader))
    }) {
        return Err(RuntimeReaderClusterBlocker::ClosureEscapes);
    }

    for namespace in &moved_namespace_exports {
        let namespace_export = ctx
            .read_index
            .namespace_exports_by_namespace
            .get(namespace)
            .ok_or(RuntimeReaderClusterBlocker::MissingSnippet)?;
        for dep in namespace_export.exports.values() {
            if primary_bindings.contains(dep) || moved_snippets.contains(dep) {
                continue;
            }
            if let Some(dep_owner) = ctx.candidate_owners.get(dep) {
                if *dep_owner != owner_module {
                    return Err(RuntimeReaderClusterBlocker::NamespaceTargetDifferentWriter);
                }
                extra_runtime_deps.insert(dep.clone());
                continue;
            }
            if !ctx.prelude.defines(dep) {
                if owner_available_bindings.contains(dep) {
                    continue;
                }
                if let Some(dep_module) = runtime_reader_source_dependency(ctx, owner_module, dep) {
                    extra_source_deps
                        .entry(dep_module)
                        .or_default()
                        .insert(dep.clone());
                    continue;
                }
                return Err(RuntimeReaderClusterBlocker::ReadsNonRuntimeBinding);
            }
            extra_runtime_deps.insert(dep.clone());
        }
    }

    if moved_snippets
        .iter()
        .chain(extra_runtime_deps.iter())
        .chain(primary_bindings.iter())
        .any(|binding| owner_available_bindings.contains(binding))
    {
        return Err(RuntimeReaderClusterBlocker::OwnerNameConflict);
    }

    for primary in &primary_bindings {
        extra_runtime_deps.remove(primary);
    }

    Ok(RuntimeReaderClusterMigration {
        primary_bindings,
        extra_snippets: moved_snippets,
        extra_namespace_exports: moved_namespace_exports,
        extra_runtime_deps,
        extra_source_deps,
    })
}

fn owner_declared_or_imported_bindings(
    ctx: &RuntimeReaderClusterContext<'_>,
    owner_module: ModuleId,
) -> Result<BTreeSet<BindingName>, RuntimeReaderClusterBlocker> {
    ctx.owner_available_bindings
        .get(&owner_module)
        .cloned()
        .ok_or(RuntimeReaderClusterBlocker::OwnerSourceMissing)
}

fn runtime_reader_owner_available_bindings(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
    lowered_runtime_sources: &BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
    owners: impl IntoIterator<Item = ModuleId>,
) -> BTreeMap<ModuleId, BTreeSet<BindingName>> {
    let mut by_owner = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
    for owner_module in owners {
        if by_owner.contains_key(&owner_module) {
            continue;
        }
        let Some(owner_source) = lowered_runtime_sources.get(&owner_module) else {
            continue;
        };
        let mut bindings = owner_source.local_definitions.clone();
        bindings.extend(program.model().graph().ast_imports_for(owner_module));
        if let Some(imports_by_target) = source_module_wiring.imports_by_module.get(&owner_module) {
            for bindings_from_target in imports_by_target.values() {
                bindings.extend(bindings_from_target.iter().cloned());
            }
        }
        by_owner.insert(owner_module, bindings);
    }
    by_owner
}

fn runtime_reader_source_dependency(
    ctx: &RuntimeReaderClusterContext<'_>,
    owner_module: ModuleId,
    binding: &BindingName,
) -> Option<ModuleId> {
    let dep_module = ctx
        .source_definition_modules
        .get(binding)
        .and_then(|module_id| *module_id)?;
    if dep_module == owner_module || ctx.folded_modules.contains(&dep_module) {
        return None;
    }
    if !ctx
        .module_dependencies_by_owner
        .get(&owner_module)
        .is_some_and(|deps| deps.contains(&dep_module))
    {
        return None;
    }
    if !ctx
        .source_local_definitions_by_module
        .get(&dep_module)
        .is_some_and(|definitions| definitions.contains(binding))
    {
        return None;
    }
    Some(dep_module)
}

fn module_dependency_modules_by_owner(
    program: &EnrichedProgram,
) -> BTreeMap<ModuleId, BTreeSet<ModuleId>> {
    let mut dependencies = BTreeMap::<ModuleId, BTreeSet<ModuleId>>::new();
    for dependency in &program.model().input().dependencies {
        let ModuleDependencyTarget::Module(target_module_id) = dependency.target else {
            continue;
        };
        dependencies
            .entry(dependency.from_module_id)
            .or_default()
            .insert(target_module_id);
    }
    dependencies
}

fn runtime_binding_has_blocking_non_snippet_use(
    read_index: &RuntimeSourceReadIndex,
    binding: &BindingName,
) -> bool {
    if read_index
        .entrypoint_callee
        .as_ref()
        .is_some_and(|entrypoint| entrypoint == binding)
    {
        return true;
    }
    read_index.non_snippet_runtime_reads.contains(binding)
        || read_index.namespace_export_helpers.contains(binding)
}

fn is_migratable_reader_function_snippet(binding: &BindingName, source: &str) -> bool {
    let source = source.trim();
    if let Some(rest) = source.strip_prefix("async")
        && rest.starts_with(|c: char| c.is_ascii_whitespace())
    {
        let rest = rest.trim_start();
        return function_declaration_names_binding(rest, binding);
    }
    class_declaration_names_binding(source, binding)
        || function_declaration_names_binding(source, binding)
        || variable_declaration_names_function_like_binding(source, binding)
}

fn is_migratable_namespace_reader_snippet(binding: &BindingName, source: &str) -> bool {
    let source = source.trim();
    for keyword in ["var", "let", "const"] {
        let Some(rest) = source.strip_prefix(keyword) else {
            continue;
        };
        if !rest.starts_with(|c: char| c.is_ascii_whitespace()) {
            continue;
        }
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_suffix(';') else {
            continue;
        };
        let mut splitter = rest.splitn(2, '=');
        let lhs = splitter.next().unwrap_or("").trim();
        let rhs = splitter.next().unwrap_or("").trim();
        if lhs != binding.as_str() {
            continue;
        }
        let compact_rhs = rhs
            .chars()
            .filter(|character| !character.is_ascii_whitespace())
            .collect::<String>();
        if compact_rhs == "{}" {
            return true;
        }
    }
    false
}

fn function_declaration_names_binding(source: &str, binding: &BindingName) -> bool {
    if !keyword_at(source, 0, "function") {
        return false;
    }
    parse_identifier_after_keyword(source, 0, "function")
        .is_some_and(|(name, _)| name == binding.as_str())
}

fn class_declaration_names_binding(source: &str, binding: &BindingName) -> bool {
    if !keyword_at(source, 0, "class") {
        return false;
    }
    parse_identifier_after_keyword(source, 0, "class")
        .is_some_and(|(name, _)| name == binding.as_str())
        && is_migratable_reader_class(source)
}

fn is_migratable_reader_class(source: &str) -> bool {
    let Some(open) = find_top_level_byte(source, b'{') else {
        return false;
    };
    let header = source[..open].trim();
    if !migratable_reader_class_header(header) {
        return false;
    }
    let Some(close) = find_matching_brace(source, open) else {
        return false;
    };
    if skip_ws(source.as_bytes(), close + 1) != source.len() {
        return false;
    }
    !class_body_has_top_level_computed_key(source) && !class_body_has_eager_static_element(source)
}

fn migratable_reader_class_header(header: &str) -> bool {
    if header.contains('[') {
        return false;
    }
    if let Some(extends) = header
        .split_once("extends")
        .map(|(_, extends)| extends.trim())
    {
        let (base, end) = match parse_identifier(extends, 0) {
            Some(parsed) => parsed,
            None => return false,
        };
        if end != extends.len() || !is_runtime_global_identifier(base) {
            return false;
        }
    }
    true
}

fn class_body_has_top_level_computed_key(source: &str) -> bool {
    let Some(open) = find_top_level_byte(source, b'{') else {
        return true;
    };
    let Some(close) = find_matching_brace(source, open) else {
        return true;
    };
    let bytes = source.as_bytes();
    let mut cursor = open + 1;
    while cursor < close {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'[' => return true,
            b'(' => {
                let Some(end) = find_matching_paren(source, cursor) else {
                    return true;
                };
                cursor = end + 1;
            }
            b'{' => {
                let Some(end) = find_matching_brace(source, cursor) else {
                    return true;
                };
                cursor = end + 1;
            }
            _ => cursor += 1,
        }
    }
    false
}

fn class_body_has_eager_static_element(source: &str) -> bool {
    let Some(open) = find_top_level_byte(source, b'{') else {
        return true;
    };
    let Some(close) = find_matching_brace(source, open) else {
        return true;
    };
    let bytes = source.as_bytes();
    let mut cursor = open + 1;
    while cursor < close {
        cursor = skip_ws_and_comments(bytes, cursor, close);
        if cursor >= close {
            break;
        }
        if keyword_at(source, cursor, "static")
            && static_class_element_is_eager(source, cursor + "static".len(), close)
        {
            return true;
        }
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'(' => {
                let Some(end) = find_matching_paren(source, cursor) else {
                    return true;
                };
                cursor = end + 1;
            }
            b'{' => {
                let Some(end) = find_matching_brace(source, cursor) else {
                    return true;
                };
                cursor = end + 1;
            }
            _ => cursor += 1,
        }
    }
    false
}

fn static_class_element_is_eager(source: &str, after_static: usize, close: usize) -> bool {
    let bytes = source.as_bytes();
    let cursor = skip_ws_and_comments(bytes, after_static, close);
    if cursor >= close {
        return false;
    }
    match bytes[cursor] {
        // `static() {}` / `static = ...` / `static;` are instance
        // members named "static", not static elements, so they don't run
        // during class definition.
        b'(' | b'=' | b';' => return false,
        // Static blocks run immediately while the class is being defined.
        b'{' => return true,
        _ => {}
    }

    let mut cursor = cursor;
    while cursor < close {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            // Method/accessor parameter list: safe, because the body is not
            // evaluated until the method is called after the writer module's
            // same-module assignment has run.
            b'(' => return false,
            b'=' => return static_field_initializer_is_eager(source, cursor, close),
            // Empty static fields don't read migrated runtime state.
            b';' => return false,
            // Static blocks run immediately while the class is being defined.
            b'{' => return true,
            _ => cursor += 1,
        }
    }
    false
}

fn static_field_initializer_is_eager(source: &str, equals: usize, close: usize) -> bool {
    let bytes = source.as_bytes();
    let initializer_start = skip_ws_and_comments(bytes, equals + 1, close);
    if initializer_start >= close {
        return false;
    }
    let initializer_end = find_statement_end(&source[..close], initializer_start).unwrap_or(close);
    let initializer = source[initializer_start..initializer_end].trim();
    !initializer.is_empty() && !is_pure_initializer_expression(initializer)
}

fn variable_declaration_names_function_like_binding(source: &str, binding: &BindingName) -> bool {
    let source = source.trim();
    for keyword in ["var", "let", "const"] {
        let Some(rest) = source.strip_prefix(keyword) else {
            continue;
        };
        if !rest.starts_with(|c: char| c.is_ascii_whitespace()) {
            continue;
        }
        let Some(rest) = rest.trim_start().strip_suffix(';') else {
            continue;
        };
        let mut splitter = rest.splitn(2, '=');
        let lhs = splitter.next().unwrap_or("").trim();
        let rhs = splitter.next().unwrap_or("").trim();
        if lhs != binding.as_str() {
            continue;
        }
        if expression_is_function_like_reader(rhs) {
            return true;
        }
    }
    false
}

fn expression_is_function_like_reader(source: &str) -> bool {
    let source = source.trim();
    if keyword_at(source, 0, "function") || looks_like_arrow_function_expression(source) {
        return true;
    }
    if let Some(rest) = source.strip_prefix("async")
        && rest.starts_with(|c: char| c.is_ascii_whitespace())
    {
        let rest = rest.trim_start();
        return keyword_at(rest, 0, "function") || looks_like_arrow_function_expression(rest);
    }
    false
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeLazyFoldModule {
    source_file_id: u32,
    required_bindings: BTreeSet<BindingName>,
    stub_exports: BTreeSet<BindingName>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeFoldedSourceChunk {
    byte_start: u32,
    source: String,
}

fn lowered_runtime_sources(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
    eager_safe_analysis: &EagerSafeAnalysis,
    externalized_packages: &BTreeSet<ModuleId>,
) -> BTreeMap<ModuleId, LoweredRuntimeModuleSource> {
    let mut sources = BTreeMap::new();
    for module in program.model().modules() {
        if module.kind == ModuleKind::Package && externalized_packages.contains(&module.id) {
            continue;
        }
        let runtime_imports = program.model().graph().runtime_imports_for(module.id);
        let Some(source) = program.model().input().module_source_slice(module.id) else {
            continue;
        };
        let mut helper_kinds = runtime_helper_kinds(program.model().graph(), &runtime_imports);
        helper_kinds.extend(runtime_helper_kinds_for_source(
            program.model().graph(),
            source.source_file_id,
            source.source,
        ));
        // Cross-module export set: any binding this module exports stays
        // observable from other modules' call sites. Pass them to the
        // lowering pass so delazify / namespace decomposition refuse to
        // collapse bindings that external consumers still call as thunks.
        // Two sources of "this module's binding is observed externally":
        //   1. `import_export().exports_for(m)` — bindings the graph marked
        //      as explicit ESM exports.
        //   2. `source_module_wiring.exports_by_module[m]` — bindings the
        //      planner is about to emit `export { X }` for because some
        //      other module's source imports them. In bundle inputs the
        //      module rarely has a literal `export` keyword, so this is
        //      where the real cross-module surface lives.
        //
        // Phase 8 relaxation: bindings the cross-module eager-safe
        // analysis cleared are SUBTRACTED from the blocked set. They've
        // already been verified to be (a) outside any top-level
        // evaluation cycle (singleton SCC) and (b) only referenced by
        // consumers in zero-arg `X()` shape — i.e. mechanically
        // rewritable via the consumer-side pass below.
        let mut exported_bindings: BTreeSet<BindingName> = program
            .model()
            .graph()
            .import_export()
            .exports_for(module.id)
            .into_iter()
            .collect();
        if let Some(wiring_exports) = source_module_wiring.exports_by_module.get(&module.id) {
            exported_bindings.extend(wiring_exports.iter().cloned());
        }
        if let Some(eager_safe) = eager_safe_analysis
            .eager_safe_exports_by_module
            .get(&module.id)
        {
            for binding in eager_safe {
                exported_bindings.remove(binding);
            }
        }
        let empty_safe_targets = BTreeSet::<String>::new();
        let eager_safe_call_targets = eager_safe_analysis
            .safe_call_targets_by_module
            .get(&module.id)
            .unwrap_or(&empty_safe_targets);
        let mut lowering = lower_runtime_helpers_with_options(
            source.source,
            &helper_kinds,
            &exported_bindings,
            eager_safe_call_targets,
            false,
        );
        // Phase 8 cross-module rewrite: bindings this module imports
        // that the eager-safe analysis cleared no longer carry their
        // lazy thunk in the exporting module. Their `X()` call sites
        // in this consumer must be stripped to bare `X` so the
        // import-binding (now a direct value) is accessed instead of
        // attempting to invoke a non-function.
        let eagerified_imports =
            consumer_eagerified_imports(module.id, source_module_wiring, eager_safe_analysis);
        if !eagerified_imports.is_empty() {
            lowering.source = rewrite_eagerified_call_sites(&lowering.source, &eagerified_imports);
        }
        let local_definitions = top_level_definitions_in_source(lowering.source.as_str());
        let local_writes = implicit_global_writes_in_source(lowering.source.as_str());
        let mut planned_bindings = BTreeSet::<BindingName>::new();
        if let Some(module_imports) = source_module_wiring.imports_by_module.get(&module.id) {
            for bindings in module_imports.values() {
                planned_bindings.extend(bindings.iter().cloned());
            }
        }
        let remaining_helpers = lowering
            .remaining_helpers
            .into_iter()
            .filter(|binding| !planned_bindings.contains(binding))
            .filter(|binding| !local_definitions.contains(binding))
            .collect::<BTreeSet<_>>();
        let written_helpers = remaining_helpers
            .intersection(&local_writes)
            .cloned()
            .collect::<BTreeSet<_>>();
        let byte_start = source.span.map(|span| span.byte_start).unwrap_or_default();
        sources.insert(
            module.id,
            LoweredRuntimeModuleSource {
                source_file_id: source.source_file_id,
                source_file_path: source.source_file_path.to_string(),
                byte_start,
                source: lowering.source,
                lowered_helpers: lowering.lowered_helpers,
                remaining_helpers,
                local_definitions,
                local_writes,
                written_helpers,
                uses_lazy_module: lowering.uses_lazy_module,
                uses_lazy_value: lowering.uses_lazy_value,
                reshaped_bindings: lowering.reshaped_bindings,
            },
        );
    }
    sources
}

fn runtime_lazy_fold_plan(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
    lowered_runtime_sources: &BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
) -> RuntimeLazyFoldPlan {
    let mut plan = RuntimeLazyFoldPlan::default();
    for module in program.model().modules() {
        if module.kind != ModuleKind::Application {
            continue;
        }
        if !program.package_imports_for(module.id).is_empty() {
            continue;
        }
        let Some(source) = lowered_runtime_sources.get(&module.id) else {
            continue;
        };
        if source.written_helpers.is_empty() {
            continue;
        }
        let Some(prelude) = program
            .model()
            .graph()
            .runtime_prelude(source.source_file_id)
        else {
            continue;
        };
        if !source
            .written_helpers
            .iter()
            .all(|binding| prelude.defines(binding))
        {
            continue;
        }
        let Some((folded_source, explicit_exports)) =
            strip_top_level_named_exports(source.source.as_str())
        else {
            continue;
        };
        if !source_is_lazy_preserving_foldable(folded_source.as_str()) {
            continue;
        }
        let folded_source =
            purify_folded_lazy_initializers(folded_source.as_str(), &source.written_helpers);
        let mut available_bindings = program.model().graph().ast_definitions_for(module.id);
        available_bindings.extend(program.model().graph().ast_imports_for(module.id));
        available_bindings.extend(source.written_helpers.iter().cloned());
        let mut stub_exports = program
            .model()
            .graph()
            .import_export()
            .exports_for(module.id)
            .into_iter()
            .collect::<BTreeSet<_>>();
        stub_exports.extend(explicit_exports);
        if let Some(extra_exports) = source_module_wiring.exports_by_module.get(&module.id) {
            stub_exports.extend(extra_exports.iter().cloned());
        }
        if stub_exports.is_empty()
            || !stub_exports
                .iter()
                .all(|binding| available_bindings.contains(binding))
        {
            continue;
        }
        let mut required_bindings = source.remaining_helpers.clone();
        required_bindings.extend(source.written_helpers.iter().cloned());
        plan.modules.insert(
            module.id,
            RuntimeLazyFoldModule {
                source_file_id: source.source_file_id,
                required_bindings,
                stub_exports,
            },
        );
        plan.chunks_by_source_file
            .entry(source.source_file_id)
            .or_default()
            .push(RuntimeFoldedSourceChunk {
                byte_start: source.byte_start,
                source: folded_source.trim().to_string(),
            });
    }
    for chunks in plan.chunks_by_source_file.values_mut() {
        chunks.sort_by_key(|chunk| chunk.byte_start);
    }
    plan
}

fn strip_top_level_named_exports(source: &str) -> Option<(String, BTreeSet<BindingName>)> {
    let bytes = source.as_bytes();
    let mut output = String::new();
    let mut exports = BTreeSet::new();
    let mut cursor = 0usize;
    let mut last = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;

    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            _ if paren_depth == 0
                && bracket_depth == 0
                && brace_depth == 0
                && keyword_at(source, cursor, "import") =>
            {
                return None;
            }
            _ if paren_depth == 0
                && bracket_depth == 0
                && brace_depth == 0
                && keyword_at(source, cursor, "export") =>
            {
                let export_start = cursor;
                cursor = skip_ws(bytes, cursor + "export".len());
                if bytes.get(cursor) != Some(&b'{') {
                    return None;
                }
                let export_end = find_matching_brace(source, cursor)?;
                exports.extend(parse_named_export_bindings(
                    &source[cursor + 1..export_end],
                )?);
                cursor = skip_ws(bytes, export_end + 1);
                if keyword_at(source, cursor, "from") {
                    return None;
                }
                if bytes.get(cursor) == Some(&b';') {
                    cursor += 1;
                }
                output.push_str(&source[last..export_start]);
                last = cursor;
            }
            b'(' => {
                paren_depth += 1;
                cursor += 1;
            }
            b'[' => {
                bracket_depth += 1;
                cursor += 1;
            }
            b'{' => {
                brace_depth += 1;
                cursor += 1;
            }
            b')' => {
                paren_depth = paren_depth.saturating_sub(1);
                cursor += 1;
            }
            b']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                cursor += 1;
            }
            b'}' => {
                brace_depth = brace_depth.saturating_sub(1);
                cursor += 1;
            }
            _ => cursor += 1,
        }
    }
    output.push_str(&source[last..]);
    Some((output, exports))
}

fn parse_named_export_bindings(source: &str) -> Option<BTreeSet<BindingName>> {
    let mut bindings = BTreeSet::new();
    for item in split_top_level_properties(source) {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        if item.starts_with("type ") || item == "type" {
            return None;
        }
        let parts = item.split_whitespace().collect::<Vec<_>>();
        if parts.contains(&"as") {
            return None;
        }
        let local = parts.first().copied()?;
        let (binding, end) = parse_identifier(local, 0)?;
        if end != local.len() || is_js_keyword(binding) {
            return None;
        }
        bindings.insert(BindingName::new(binding));
    }
    Some(bindings)
}

fn source_is_lazy_preserving_foldable(source: &str) -> bool {
    let mut saw_lazy_initializer = false;
    for statement in top_level_statement_slices(source) {
        let statement = statement.trim();
        if statement.is_empty() {
            continue;
        }
        if lowered_lazy_initializer_statement_binding(statement).is_some() {
            saw_lazy_initializer = true;
            continue;
        }
        if variable_declaration_without_initializer(statement) {
            continue;
        }
        return false;
    }
    saw_lazy_initializer
}

fn purify_folded_lazy_initializers(
    source: &str,
    writable_helpers: &BTreeSet<BindingName>,
) -> String {
    top_level_statement_slices(source)
        .into_iter()
        .filter_map(|statement| {
            let statement = statement.trim();
            if statement.is_empty() {
                return None;
            }
            Some(
                pure_lazy_initializer_replacement(statement, writable_helpers)
                    .unwrap_or_else(|| statement.to_string()),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn purify_private_runtime_lazy_initializers(
    source: &str,
    writable_helpers: &BTreeSet<BindingName>,
) -> String {
    let pure_value_bindings = pure_runtime_value_bindings(source);
    let bytes = source.as_bytes();
    let mut edits = Vec::<(usize, usize, String)>::new();
    let mut cursor = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && let Some((initializer, after)) =
                try_parse_runtime_lazy_initializer_declaration(source, cursor)
        {
            if let Some(replacement) = private_runtime_lazy_initializer_replacement(
                &initializer,
                writable_helpers,
                &pure_value_bindings,
            ) {
                edits.push((initializer.span.0, initializer.span.1, replacement));
            }
            cursor = after;
            continue;
        }
        match bytes[cursor] {
            b'(' => paren_depth += 1,
            b')' => paren_depth = paren_depth.saturating_sub(1),
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b'{' => brace_depth += 1,
            b'}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
        cursor += 1;
    }
    if edits.is_empty() {
        return source.to_string();
    }
    apply_text_edits(source, &edits)
}

fn apply_text_edits(source: &str, edits: &[(usize, usize, String)]) -> String {
    let mut edits = edits.to_vec();
    edits.sort_by_key(|(start, _, _)| *start);
    let mut output = String::with_capacity(source.len());
    let mut cursor = 0usize;
    for (start, end, replacement) in edits {
        debug_assert!(start >= cursor, "text edits must not overlap");
        output.push_str(&source[cursor..start]);
        output.push_str(replacement.as_str());
        cursor = end;
    }
    output.push_str(&source[cursor..]);
    output
}

fn private_runtime_lazy_initializer_replacement(
    initializer: &ParsedRuntimeLazyInitializer<'_>,
    writable_helpers: &BTreeSet<BindingName>,
    pure_value_bindings: &BTreeSet<BindingName>,
) -> Option<String> {
    let assignments = pure_runtime_lazy_body_assignments(
        initializer.body,
        writable_helpers,
        pure_value_bindings,
    )?;
    if assignments.is_empty() {
        return None;
    }
    let mut lines = assignments
        .into_iter()
        .map(|(target, value)| format!("{target} = {value};"))
        .collect::<Vec<_>>();
    lines.push(format!(
        "var {} = () => {{}};",
        initializer.binding.as_str()
    ));
    Some(lines.join("\n"))
}

fn pure_runtime_value_bindings(source: &str) -> BTreeSet<BindingName> {
    let mut bindings = BTreeSet::new();
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 {
            if keyword_at(source, cursor, "function")
                && let Some((binding, next)) =
                    parse_identifier_after_keyword(source, cursor, "function")
            {
                bindings.insert(BindingName::new(binding));
                cursor = next;
                continue;
            }
            if keyword_at(source, cursor, "class")
                && let Some((binding, next)) =
                    parse_identifier_after_keyword(source, cursor, "class")
            {
                bindings.insert(BindingName::new(binding));
                cursor = next;
                continue;
            }
            if let Some((binding, value, after)) = parse_pure_top_level_var_value(source, cursor) {
                if is_pure_initializer_expression(value) {
                    bindings.insert(binding);
                }
                cursor = after;
                continue;
            }
        }
        match bytes[cursor] {
            b'(' => paren_depth += 1,
            b')' => paren_depth = paren_depth.saturating_sub(1),
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b'{' => brace_depth += 1,
            b'}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
        cursor += 1;
    }
    bindings
}

fn parse_pure_top_level_var_value(
    source: &str,
    start: usize,
) -> Option<(BindingName, &str, usize)> {
    let (keyword, after_keyword) = declaration_keyword_at(source, start)?;
    let mut cursor = skip_ws(source.as_bytes(), start + after_keyword);
    let (binding, after_binding) = parse_identifier(source, cursor)?;
    cursor = skip_ws(source.as_bytes(), after_binding);
    if source.as_bytes().get(cursor) != Some(&b'=') {
        return None;
    }
    let value_start = skip_ws(source.as_bytes(), cursor + 1);
    let stmt_end = find_statement_end(source, value_start)?;
    let _ = keyword;
    Some((
        BindingName::new(binding),
        source[value_start..stmt_end].trim(),
        stmt_end + usize::from(source.as_bytes().get(stmt_end) == Some(&b';')),
    ))
}

#[derive(Debug, Clone)]
struct ParsedRuntimeLazyInitializer<'a> {
    binding: BindingName,
    body: &'a str,
    span: (usize, usize),
}

fn try_parse_runtime_lazy_initializer_declaration(
    source: &str,
    start: usize,
) -> Option<(ParsedRuntimeLazyInitializer<'_>, usize)> {
    let (keyword, after_keyword) = declaration_keyword_at(source, start)?;
    let bytes = source.as_bytes();
    let mut cursor = skip_ws(bytes, start + after_keyword);
    let (binding, after_binding) = parse_identifier(source, cursor)?;
    cursor = skip_ws(bytes, after_binding);
    if bytes.get(cursor) != Some(&b'=') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    let prefix = "lazyValue(";
    if !source[cursor..].starts_with(prefix) {
        return None;
    }
    cursor += prefix.len();
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'(') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    if bytes.get(cursor) != Some(&b')') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    cursor = expect_arrow(bytes, cursor)?;
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'{') {
        return None;
    }
    let body_end = find_matching_brace(source, cursor)?;
    let body = source[cursor + 1..body_end].trim();
    let after_body = skip_ws(bytes, body_end + 1);
    if bytes.get(after_body) != Some(&b')') {
        return None;
    }
    let after_paren = skip_ws(bytes, after_body + 1);
    if bytes.get(after_paren) != Some(&b';') {
        return None;
    }
    let _ = keyword;
    let stmt_end = after_paren + 1;
    Some((
        ParsedRuntimeLazyInitializer {
            binding: BindingName::new(binding),
            body,
            span: (start, stmt_end),
        },
        stmt_end,
    ))
}

fn declaration_keyword_at(source: &str, start: usize) -> Option<(&'static str, usize)> {
    for keyword in ["var", "let", "const"] {
        if keyword_at(source, start, keyword) {
            return Some((keyword, keyword.len()));
        }
    }
    None
}

fn find_statement_end(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut cursor = start;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        match bytes[cursor] {
            b'(' => paren_depth += 1,
            b')' => paren_depth = paren_depth.saturating_sub(1),
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b'{' => brace_depth += 1,
            b'}' => brace_depth = brace_depth.saturating_sub(1),
            b';' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => {
                return Some(cursor);
            }
            _ => {}
        }
        cursor += 1;
    }
    None
}

fn pure_runtime_lazy_body_assignments(
    body: &str,
    writable_helpers: &BTreeSet<BindingName>,
    pure_value_bindings: &BTreeSet<BindingName>,
) -> Option<Vec<(BindingName, String)>> {
    let mut assignments = Vec::new();
    let mut written = BTreeSet::new();
    for statement in top_level_statement_slices(body) {
        let statement = statement.trim().trim_end_matches(';').trim();
        if statement.is_empty() {
            continue;
        }
        let (target, after_target) = parse_identifier(statement, 0)?;
        let target = BindingName::new(target);
        if !writable_helpers.contains(&target) || !written.insert(target.clone()) {
            return None;
        }
        let equals = skip_ws(statement.as_bytes(), after_target);
        if statement.as_bytes().get(equals) != Some(&b'=')
            || statement.as_bytes().get(equals + 1) == Some(&b'=')
            || statement.as_bytes().get(equals + 1) == Some(&b'>')
        {
            return None;
        }
        let value_start = skip_ws(statement.as_bytes(), equals + 1);
        let value = statement[value_start..].trim();
        if !is_pure_runtime_assignment_value(value, pure_value_bindings) {
            return None;
        }
        assignments.push((target, value.to_string()));
    }
    Some(assignments)
}

fn is_pure_runtime_assignment_value(
    value: &str,
    pure_value_bindings: &BTreeSet<BindingName>,
) -> bool {
    if is_pure_initializer_expression(value) {
        return true;
    }
    let value = value.trim();
    let Some((identifier, end)) = parse_identifier(value, 0) else {
        return false;
    };
    if end != value.len() {
        return false;
    }
    is_runtime_global_identifier(identifier)
        || pure_value_bindings.contains(&BindingName::new(identifier))
}

fn pure_lazy_initializer_replacement(
    statement: &str,
    writable_helpers: &BTreeSet<BindingName>,
) -> Option<String> {
    let initializer = parse_lowered_lazy_initializer_statement(statement)?;
    let assignments = pure_lazy_body_assignments(initializer.body, writable_helpers)?;
    if assignments.is_empty() {
        return None;
    }
    let mut lines = assignments
        .into_iter()
        .map(|(target, value)| format!("{target} = {value};"))
        .collect::<Vec<_>>();
    lines.push(format!(
        "var {} = () => {{}};",
        initializer.binding.as_str()
    ));
    Some(lines.join("\n"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedLoweredLazyInitializer<'a> {
    binding: BindingName,
    body: &'a str,
}

fn parse_lowered_lazy_initializer_statement(
    statement: &str,
) -> Option<ParsedLoweredLazyInitializer<'_>> {
    let statement = statement.trim().trim_end_matches(';').trim();
    let (_keyword, after_keyword) = declaration_keyword_at_start(statement)?;
    let cursor = skip_ws(statement.as_bytes(), after_keyword);
    let (binding, after_binding) = parse_identifier(statement, cursor)?;
    let equals = skip_ws(statement.as_bytes(), after_binding);
    if statement.as_bytes().get(equals) != Some(&b'=') {
        return None;
    }
    let initializer_start = skip_ws(statement.as_bytes(), equals + 1);
    let body = parse_lowered_lazy_initializer_body(&statement[initializer_start..])?;
    Some(ParsedLoweredLazyInitializer {
        binding: BindingName::new(binding),
        body,
    })
}

fn parse_lowered_lazy_initializer_body(initializer: &str) -> Option<&str> {
    if !looks_like_lowered_lazy_initializer(initializer) {
        return None;
    }
    // Expected shape: `lazyValue(() => { BODY })`.
    let prefix = "lazyValue(";
    if !initializer.starts_with(prefix) {
        return None;
    }
    let mut cursor = prefix.len();
    cursor = skip_ws(initializer.as_bytes(), cursor);
    if initializer.as_bytes().get(cursor) != Some(&b'(') {
        return None;
    }
    cursor = skip_ws(initializer.as_bytes(), cursor + 1);
    if initializer.as_bytes().get(cursor) != Some(&b')') {
        return None;
    }
    cursor = skip_ws(initializer.as_bytes(), cursor + 1);
    cursor = expect_arrow(initializer.as_bytes(), cursor)?;
    cursor = skip_ws(initializer.as_bytes(), cursor);
    if initializer.as_bytes().get(cursor) != Some(&b'{') {
        return None;
    }
    let body_end = find_matching_brace(initializer, cursor)?;
    let after_body = skip_ws(initializer.as_bytes(), body_end + 1);
    if initializer.as_bytes().get(after_body) != Some(&b')') {
        return None;
    }
    Some(initializer[cursor + 1..body_end].trim())
}

fn pure_lazy_body_assignments(
    body: &str,
    writable_helpers: &BTreeSet<BindingName>,
) -> Option<Vec<(BindingName, String)>> {
    let mut assignments = Vec::new();
    let mut written = BTreeSet::new();
    for statement in top_level_statement_slices(body) {
        let statement = statement.trim().trim_end_matches(';').trim();
        if statement.is_empty() {
            continue;
        }
        let (target, after_target) = parse_identifier(statement, 0)?;
        let target = BindingName::new(target);
        if !writable_helpers.contains(&target) || !written.insert(target.clone()) {
            return None;
        }
        let equals = skip_ws(statement.as_bytes(), after_target);
        if statement.as_bytes().get(equals) != Some(&b'=')
            || statement.as_bytes().get(equals + 1) == Some(&b'=')
            || statement.as_bytes().get(equals + 1) == Some(&b'>')
        {
            return None;
        }
        let value_start = skip_ws(statement.as_bytes(), equals + 1);
        let value = statement[value_start..].trim();
        if !is_pure_initializer_expression(value) {
            return None;
        }
        assignments.push((target, value.to_string()));
    }
    Some(assignments)
}

fn is_pure_initializer_expression(source: &str) -> bool {
    let source = source.trim();
    if source.is_empty() {
        return false;
    }
    if is_literal_expression(source) {
        return true;
    }
    if source.as_bytes().first() == Some(&b'{') {
        return pure_object_literal(source);
    }
    if source.as_bytes().first() == Some(&b'[') {
        return pure_array_literal(source);
    }
    if keyword_at(source, 0, "class") {
        return pure_class_expression(source);
    }
    if keyword_at(source, 0, "function") || looks_like_arrow_function_expression(source) {
        return true;
    }
    false
}

fn is_literal_expression(source: &str) -> bool {
    if matches!(
        source,
        "true" | "false" | "null" | "undefined" | "NaN" | "Infinity"
    ) {
        return true;
    }
    if quoted_literal_covers_source(source) {
        return true;
    }
    if matches!(source, "!0" | "!1") {
        return true;
    }
    if source.starts_with('`') && source.ends_with('`') && !source.contains("${") {
        return true;
    }
    let number = source.strip_prefix(['+', '-']).unwrap_or(source);
    !number.is_empty()
        && number
            .chars()
            .all(|character| character.is_ascii_digit() || matches!(character, '.' | '_' | 'n'))
        && number.chars().any(|character| character.is_ascii_digit())
}

fn quoted_literal_covers_source(source: &str) -> bool {
    let bytes = source.as_bytes();
    let Some(quote @ (b'\'' | b'"')) = bytes.first().copied() else {
        return false;
    };
    skip_quoted(bytes, 0, quote) == bytes.len()
}

fn pure_object_literal(source: &str) -> bool {
    let Some(close) = find_matching_brace(source, 0) else {
        return false;
    };
    if skip_ws(source.as_bytes(), close + 1) != source.len() {
        return false;
    }
    split_top_level_properties(&source[1..close])
        .into_iter()
        .all(pure_object_property)
}

fn pure_object_property(property: &str) -> bool {
    let property = property.trim();
    if property.is_empty() || property.starts_with("...") {
        return false;
    }
    if let Some(colon) = find_top_level_byte(property, b':') {
        if !pure_object_property_key(property[..colon].trim()) {
            return false;
        }
        let value = property[colon + 1..].trim();
        return is_pure_initializer_expression(value);
    }
    pure_object_method_property(property)
}

fn pure_object_property_key(source: &str) -> bool {
    let source = source.trim();
    if source.is_empty() {
        return false;
    }
    if source.as_bytes().first() == Some(&b'[') {
        let Some(close) = find_matching_bracket(source, 0) else {
            return false;
        };
        return skip_ws(source.as_bytes(), close + 1) == source.len()
            && is_literal_expression(&source[1..close]);
    }
    if quoted_literal_covers_source(source) {
        return true;
    }
    let key = source.strip_prefix(['+', '-']).unwrap_or(source);
    key.as_bytes()
        .first()
        .is_some_and(|byte| is_identifier_start(*byte) || byte.is_ascii_digit())
        && key
            .as_bytes()
            .iter()
            .all(|byte| is_identifier_continue(*byte) || *byte == b'.')
}

fn pure_object_method_property(source: &str) -> bool {
    let source = source.trim();
    let method_source = source
        .strip_prefix("async ")
        .or_else(|| source.strip_prefix("get "))
        .or_else(|| source.strip_prefix("set "))
        .unwrap_or(source)
        .trim_start();
    let Some(open_paren) = find_top_level_byte(method_source, b'(') else {
        return false;
    };
    if !pure_object_property_key(method_source[..open_paren].trim()) {
        return false;
    }
    let Some(close_paren) = find_matching_paren(method_source, open_paren) else {
        return false;
    };
    let open_body = skip_ws(method_source.as_bytes(), close_paren + 1);
    if method_source.as_bytes().get(open_body) != Some(&b'{') {
        return false;
    }
    let Some(close_body) = find_matching_brace(method_source, open_body) else {
        return false;
    };
    skip_ws(method_source.as_bytes(), close_body + 1) == method_source.len()
}

fn pure_array_literal(source: &str) -> bool {
    let Some(close) = find_matching_bracket(source, 0) else {
        return false;
    };
    if skip_ws(source.as_bytes(), close + 1) != source.len() {
        return false;
    }
    split_top_level_properties(&source[1..close])
        .into_iter()
        .all(|element| {
            let element = element.trim();
            !element.is_empty()
                && !element.starts_with("...")
                && is_pure_initializer_expression(element)
        })
}

fn pure_class_expression(source: &str) -> bool {
    let Some(open) = find_top_level_byte(source, b'{') else {
        return false;
    };
    let header = source[..open].trim();
    if header.contains('[') {
        return false;
    }
    if let Some(extends) = header
        .split_once("extends")
        .map(|(_, extends)| extends.trim())
    {
        let (base, end) = match parse_identifier(extends, 0) {
            Some(parsed) => parsed,
            None => return false,
        };
        if end != extends.len() || !is_runtime_global_identifier(base) {
            return false;
        }
    }
    let Some(close) = find_matching_brace(source, open) else {
        return false;
    };
    if skip_ws(source.as_bytes(), close + 1) != source.len() {
        return false;
    }
    !source[open + 1..close]
        .split(|character: char| !is_identifier_continue(character as u8))
        .any(|word| word == "static")
}

fn looks_like_arrow_function_expression(source: &str) -> bool {
    find_top_level_arrow(source).is_some()
}

fn find_top_level_arrow(source: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    while cursor + 1 < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            b'=' if paren_depth == 0
                && bracket_depth == 0
                && brace_depth == 0
                && bytes.get(cursor + 1) == Some(&b'>') =>
            {
                return Some(cursor);
            }
            b'(' => {
                paren_depth += 1;
                cursor += 1;
            }
            b'[' => {
                bracket_depth += 1;
                cursor += 1;
            }
            b'{' => {
                brace_depth += 1;
                cursor += 1;
            }
            b')' => {
                paren_depth = paren_depth.saturating_sub(1);
                cursor += 1;
            }
            b']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                cursor += 1;
            }
            b'}' => {
                brace_depth = brace_depth.saturating_sub(1);
                cursor += 1;
            }
            _ => cursor += 1,
        }
    }
    None
}

fn find_top_level_byte(source: &str, target: u8) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            byte if byte == target
                && paren_depth == 0
                && bracket_depth == 0
                && brace_depth == 0 =>
            {
                return Some(cursor);
            }
            b'(' => {
                paren_depth += 1;
                cursor += 1;
            }
            b'[' => {
                bracket_depth += 1;
                cursor += 1;
            }
            b'{' => {
                brace_depth += 1;
                cursor += 1;
            }
            b')' => {
                paren_depth = paren_depth.saturating_sub(1);
                cursor += 1;
            }
            b']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                cursor += 1;
            }
            b'}' => {
                brace_depth = brace_depth.saturating_sub(1);
                cursor += 1;
            }
            _ => cursor += 1,
        }
    }
    None
}

fn top_level_statement_slices(source: &str) -> Vec<&str> {
    let bytes = source.as_bytes();
    let mut statements = Vec::new();
    let mut start = 0usize;
    let mut cursor = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            b'(' => {
                paren_depth += 1;
                cursor += 1;
            }
            b'[' => {
                bracket_depth += 1;
                cursor += 1;
            }
            b'{' => {
                brace_depth += 1;
                cursor += 1;
            }
            b')' => {
                paren_depth = paren_depth.saturating_sub(1);
                cursor += 1;
            }
            b']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                cursor += 1;
            }
            b'}' => {
                brace_depth = brace_depth.saturating_sub(1);
                cursor += 1;
            }
            b';' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => {
                statements.push(&source[start..=cursor]);
                cursor += 1;
                start = cursor;
            }
            _ => cursor += 1,
        }
    }
    if source[start..].trim().is_empty() {
        statements
    } else {
        statements.push(&source[start..]);
        statements
    }
}

fn variable_declaration_without_initializer(statement: &str) -> bool {
    let statement = statement.trim().trim_end_matches(';').trim();
    let Some((keyword, after_keyword)) = declaration_keyword_at_start(statement) else {
        return false;
    };
    if keyword == "const" {
        return false;
    }
    let mut cursor = skip_ws(statement.as_bytes(), after_keyword);
    let Some((_binding, next)) = parse_identifier(statement, cursor) else {
        return false;
    };
    cursor = next;
    !contains_top_level_initializer_operator(statement, cursor)
}

fn lowered_lazy_initializer_statement_binding(statement: &str) -> Option<BindingName> {
    let statement = statement.trim().trim_end_matches(';').trim();
    let (_keyword, after_keyword) = declaration_keyword_at_start(statement)?;
    let cursor = skip_ws(statement.as_bytes(), after_keyword);
    let (binding, after_binding) = parse_identifier(statement, cursor)?;
    let equals = skip_ws(statement.as_bytes(), after_binding);
    if statement.as_bytes().get(equals) != Some(&b'=') {
        return None;
    }
    let initializer = statement[skip_ws(statement.as_bytes(), equals + 1)..].trim();
    looks_like_lowered_lazy_initializer(initializer).then(|| BindingName::new(binding))
}

fn declaration_keyword_at_start(source: &str) -> Option<(&'static str, usize)> {
    ["var", "let", "const"]
        .into_iter()
        .find(|keyword| keyword_at(source, 0, keyword))
        .map(|keyword| (keyword, keyword.len()))
}

fn looks_like_lowered_lazy_initializer(initializer: &str) -> bool {
    let compact = compact_js_source(initializer);
    compact.starts_with("lazyValue(()=>{") && compact.ends_with("})")
}

fn contains_top_level_initializer_operator(source: &str, mut cursor: usize) -> bool {
    let bytes = source.as_bytes();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            b'(' => {
                paren_depth += 1;
                cursor += 1;
            }
            b'[' => {
                bracket_depth += 1;
                cursor += 1;
            }
            b'{' => {
                brace_depth += 1;
                cursor += 1;
            }
            b')' => {
                paren_depth = paren_depth.saturating_sub(1);
                cursor += 1;
            }
            b']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                cursor += 1;
            }
            b'}' => {
                brace_depth = brace_depth.saturating_sub(1);
                cursor += 1;
            }
            b'=' if paren_depth == 0
                && bracket_depth == 0
                && brace_depth == 0
                && bytes.get(cursor + 1) != Some(&b'=')
                && bytes.get(cursor + 1) != Some(&b'>') =>
            {
                return true;
            }
            _ => cursor += 1,
        }
    }
    false
}

fn runtime_entrypoint(program: &EnrichedProgram) -> Option<(&RuntimePrelude, &RuntimeEntrypoint)> {
    program
        .model()
        .graph()
        .runtime_preludes()
        .values()
        .find_map(|prelude| {
            prelude
                .entrypoint
                .as_ref()
                .map(|entrypoint| (prelude, entrypoint))
        })
}

fn runtime_entrypoint_root_bindings(
    prelude: &RuntimePrelude,
    entrypoint: &RuntimeEntrypoint,
) -> BTreeSet<BindingName> {
    let side_effects = runtime_entrypoint_side_effects(prelude, entrypoint);
    let mut source_bindings = BTreeSet::from([entrypoint.callee.clone()]);
    for side_effect in &side_effects {
        for identifier in identifiers_in_source(side_effect.source.as_str()) {
            let binding = BindingName::new(identifier);
            if prelude.defines(&binding) {
                source_bindings.insert(binding);
            }
        }
    }
    source_bindings
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClosedRuntimeHelperSource {
    emitted_bindings: BTreeSet<BindingName>,
    source: String,
}

fn close_runtime_helper_source(
    prelude: &RuntimePrelude,
    root_bindings: &BTreeSet<BindingName>,
    entrypoint: Option<&RuntimeEntrypoint>,
    folded_chunks: &[RuntimeFoldedSourceChunk],
) -> ClosedRuntimeHelperSource {
    let mut source_bindings = prelude.required_bindings_for(root_bindings.iter());

    loop {
        let namespace_exports = runtime_namespace_exports_for_helpers(prelude, &source_bindings);
        let mut roots = source_bindings.clone();
        for namespace_export in &namespace_exports {
            roots.extend(namespace_export.exports.values().cloned());
        }

        let source_bindings_with_namespaces = prelude.required_bindings_for(roots.iter());
        let helper_source = runtime_helper_source(
            prelude,
            &source_bindings_with_namespaces,
            &namespace_exports,
            entrypoint,
            folded_chunks,
        );
        let mut next_roots = source_bindings_with_namespaces.clone();
        for identifier in runtime_import_identifiers_in_source(helper_source.as_str()) {
            let binding = BindingName::new(identifier);
            if prelude.defines(&binding) {
                next_roots.insert(binding);
            }
        }
        let next_source_bindings = prelude.required_bindings_for(next_roots.iter());
        if next_source_bindings == source_bindings_with_namespaces {
            let namespace_exports =
                runtime_namespace_exports_for_helpers(prelude, &next_source_bindings);
            let source = runtime_helper_source(
                prelude,
                &next_source_bindings,
                &namespace_exports,
                entrypoint,
                folded_chunks,
            );
            return ClosedRuntimeHelperSource {
                emitted_bindings: emitted_runtime_helper_bindings(prelude, &next_source_bindings),
                source,
            };
        }

        source_bindings = next_source_bindings;
    }
}

fn emitted_runtime_helper_bindings(
    prelude: &RuntimePrelude,
    bindings: &BTreeSet<BindingName>,
) -> BTreeSet<BindingName> {
    bindings
        .iter()
        .filter(|binding| prelude.snippets.contains_key(*binding))
        .cloned()
        .collect()
}

fn runtime_helper_source(
    prelude: &RuntimePrelude,
    source_bindings: &BTreeSet<BindingName>,
    namespace_exports: &[RuntimeNamespaceExport],
    entrypoint: Option<&RuntimeEntrypoint>,
    folded_chunks: &[RuntimeFoldedSourceChunk],
) -> String {
    let mut snippets = BTreeMap::<u32, String>::new();
    for binding in source_bindings {
        let Some(snippet) = prelude.snippets.get(binding) else {
            continue;
        };
        snippets
            .entry(snippet.byte_start)
            .or_insert_with(|| snippet.source.clone());
    }
    let mut chunks = snippets
        .into_iter()
        .map(|(byte_start, source)| (byte_start, 0u8, source))
        .collect::<Vec<_>>();
    for namespace_export in namespace_exports {
        chunks.push((
            namespace_export.byte_start,
            1,
            runtime_namespace_export_statement(namespace_export),
        ));
    }
    for folded_chunk in folded_chunks {
        chunks.push((folded_chunk.byte_start, 2, folded_chunk.source.clone()));
    }
    if let Some(entrypoint) = entrypoint {
        for side_effect in runtime_entrypoint_side_effects(prelude, entrypoint) {
            chunks.push((side_effect.byte_start, 3, side_effect.source));
        }
    }
    chunks.sort_by_key(|(byte_start, kind, _source)| (*byte_start, *kind));
    chunks
        .into_iter()
        .map(|(_, _, source)| source)
        .collect::<Vec<_>>()
        .join("\n")
}

fn runtime_entrypoint_side_effects(
    prelude: &RuntimePrelude,
    entrypoint: &RuntimeEntrypoint,
) -> Vec<reverts_graph::RuntimePreludeSideEffect> {
    let mut side_effects = entrypoint
        .side_effects
        .iter()
        .filter(|side_effect| !is_noop_runtime_side_effect(prelude, side_effect.source.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    side_effects.sort_by_key(|side_effect| side_effect.byte_start);
    side_effects
}

fn is_noop_runtime_side_effect(prelude: &RuntimePrelude, source: &str) -> bool {
    let Some(binding) = simple_call_statement_binding(source) else {
        return false;
    };
    let Some(snippet) = prelude.snippets.get(&binding) else {
        return false;
    };
    runtime_prelude_snippet_is_noop(binding.as_str(), snippet.source.as_str())
}

fn simple_call_statement_binding(source: &str) -> Option<BindingName> {
    let source = source.trim().trim_end_matches(';').trim();
    let (identifier, after_identifier) = parse_identifier(source, 0)?;
    let open = skip_ws(source.as_bytes(), after_identifier);
    if source.as_bytes().get(open) != Some(&b'(') {
        return None;
    }
    let close = find_matching_paren(source, open)?;
    if !source[open + 1..close].trim().is_empty() {
        return None;
    }
    (skip_ws(source.as_bytes(), close + 1) == source.len()).then(|| BindingName::new(identifier))
}

fn runtime_prelude_snippet_is_noop(binding: &str, source: &str) -> bool {
    let compact = compact_js_source(source);
    let candidates = [
        format!("var{binding}=()=>{{}};"),
        format!("let{binding}=()=>{{}};"),
        format!("const{binding}=()=>{{}};"),
        format!("function{binding}(){{}}"),
    ];
    candidates.contains(&compact)
}

fn compact_js_source(source: &str) -> String {
    source
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn runtime_source_module_imports(
    program: &EnrichedProgram,
    source: &str,
    satisfied_runtime_bindings: &BTreeSet<BindingName>,
    externalized_packages: &BTreeSet<ModuleId>,
) -> BTreeMap<ModuleId, BTreeSet<BindingName>> {
    let definition_modules = unique_source_definition_modules(program, externalized_packages);
    let mut imports = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
    let mut runtime_import_identifiers = runtime_import_identifiers_in_source(source);
    runtime_import_identifiers.extend(call_identifiers_in_source(source));
    for identifier in runtime_import_identifiers {
        let binding = BindingName::new(identifier);
        if satisfied_runtime_bindings.contains(&binding) {
            continue;
        }
        let Some(Some(module_id)) = definition_modules.get(&binding) else {
            continue;
        };
        imports
            .entry(*module_id)
            .or_default()
            .insert(binding.clone());
    }
    imports
}

fn externalized_package_init_shims(
    program: &EnrichedProgram,
    source: &str,
    externalized_packages: &BTreeSet<ModuleId>,
) -> BTreeSet<BindingName> {
    if externalized_packages.is_empty() {
        return BTreeSet::new();
    }
    let local_bindings = local_bindings_in_source(source);
    let definition_modules = unique_source_definition_modules(program, &BTreeSet::new());
    call_identifiers_in_source(source)
        .into_iter()
        .filter(|identifier| !local_bindings.contains(identifier))
        .map(BindingName::new)
        .filter(|binding| {
            definition_modules
                .get(binding)
                .and_then(|module_id| *module_id)
                .is_some_and(|module_id| externalized_packages.contains(&module_id))
        })
        .collect()
}

fn noop_function_statement(binding: &BindingName) -> String {
    format!("function {}() {{}}", binding.as_str())
}

fn unresolved_runtime_helper_references(
    prelude: &RuntimePrelude,
    source: &str,
    emitted_runtime_bindings: &BTreeSet<BindingName>,
    imports: &BTreeMap<ModuleId, BTreeSet<BindingName>>,
) -> BTreeSet<BindingName> {
    let imported = imports
        .values()
        .flat_map(|bindings| bindings.iter().cloned())
        .collect::<BTreeSet<_>>();

    runtime_import_identifiers_in_source(source)
        .into_iter()
        .map(BindingName::new)
        .filter(|binding| prelude.defines(binding))
        .filter(|binding| !emitted_runtime_bindings.contains(binding))
        .filter(|binding| !imported.contains(binding))
        .filter(|binding| !is_planner_synthetic_binding(binding.as_str()))
        .collect()
}

fn runtime_import_identifiers_in_source(source: &str) -> BTreeSet<String> {
    let local_bindings = local_bindings_in_source(source);
    value_identifiers_in_source(source)
        .into_iter()
        .filter(|identifier| !is_runtime_global_identifier(identifier))
        .filter(|identifier| !local_bindings.contains(identifier))
        .collect()
}

fn call_identifiers_in_source(source: &str) -> BTreeSet<String> {
    identifier_read_facts_in_source(source)
        .into_iter()
        .filter(|fact| fact.is_call_callee)
        .map(|fact| fact.name)
        .filter(|identifier| !is_runtime_global_identifier(identifier))
        .collect()
}

fn value_identifiers_in_source(source: &str) -> BTreeSet<String> {
    identifier_read_facts_in_source(source)
        .into_iter()
        .map(|fact| fact.name)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IdentifierReadUsage {
    name: String,
    byte_end: usize,
    is_call_callee: bool,
}

fn identifier_read_facts_in_source(source: &str) -> Vec<IdentifierReadUsage> {
    collect_identifier_read_facts(source, None, ParseGoal::TypeScript)
        .expect("planner identifier reads require parseable TypeScript source")
        .into_iter()
        .map(|fact| IdentifierReadUsage {
            name: fact.name,
            byte_end: fact.byte_end as usize,
            is_call_callee: fact.is_call_callee,
        })
        .collect()
}

fn identifier_occurrence_is_value_reference(source: &str, start: usize, end: usize) -> bool {
    let bytes = source.as_bytes();
    if previous_non_ws(bytes, start)
        .and_then(|index| bytes.get(index))
        .is_some_and(|byte| *byte == b'#')
    {
        return false;
    }
    if previous_non_ws(bytes, start)
        .and_then(|index| bytes.get(index).map(|byte| (index, *byte)))
        .is_some_and(|(index, byte)| {
            byte == b'.' && index.checked_sub(1).and_then(|prev| bytes.get(prev)) != Some(&b'.')
        })
    {
        return false;
    }
    if identifier_is_declaration_name_after_keyword(source, start, "class")
        || identifier_is_declaration_name_after_keyword(source, start, "function")
    {
        return false;
    }

    let after = skip_ws(bytes, end);
    let before = previous_non_ws(bytes, start).and_then(|index| bytes.get(index));
    if bytes.get(after) == Some(&b'=') && bytes.get(after + 1) == Some(&b'>') {
        return false;
    }
    if bytes.get(after) == Some(&b':')
        && previous_non_ws(bytes, start)
            .and_then(|index| bytes.get(index))
            .is_some_and(|byte| matches!(*byte, b'{' | b',' | b'('))
    {
        return false;
    }

    if bytes.get(after) == Some(&b'=')
        && before.is_none_or(|byte| matches!(*byte, b'{' | b'}' | b';' | b',' | b'('))
    {
        return false;
    }

    if bytes.get(after) == Some(&b'(')
        && let Some(close) = find_matching_paren(source, after)
        && bytes.get(skip_ws(bytes, close + 1)) == Some(&b'{')
        && before.is_none_or(|byte| !matches!(*byte, b'.' | b')' | b']'))
    {
        return false;
    }

    true
}

fn identifier_is_declaration_name_after_keyword(source: &str, start: usize, keyword: &str) -> bool {
    let bytes = source.as_bytes();
    let Some(keyword_end) = previous_non_ws(bytes, start) else {
        return false;
    };
    let Some(keyword_start) = keyword_end
        .checked_add(1)
        .and_then(|end| end.checked_sub(keyword.len()))
    else {
        return false;
    };
    if bytes.get(keyword_start..keyword_end + 1) != Some(keyword.as_bytes()) {
        return false;
    }
    let before = keyword_start
        .checked_sub(1)
        .and_then(|index| bytes.get(index))
        .copied();
    let after = bytes.get(keyword_end + 1).copied();
    before.is_none_or(|byte| !is_identifier_continue(byte))
        && after.is_some_and(|byte| byte.is_ascii_whitespace())
}

fn control_flow_keyword_before_paren(source: &str, open_paren: usize) -> bool {
    let bytes = source.as_bytes();
    let Some(before) = previous_non_ws(bytes, open_paren) else {
        return false;
    };
    let mut start = before;
    while start > 0 && is_identifier_continue(bytes[start - 1]) {
        start -= 1;
    }
    matches!(
        &source[start..=before],
        "if" | "while" | "switch" | "for" | "catch" | "with"
    )
}

fn class_field_bindings_in_source(source: &str) -> BTreeMap<usize, String> {
    let mut bindings = BTreeMap::new();
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            _ if keyword_at(source, cursor, "class") => {
                let Some(open) = find_class_body_open(source, cursor + "class".len()) else {
                    cursor += "class".len();
                    continue;
                };
                let Some(close) = find_matching_brace(source, open) else {
                    cursor = bytes.len();
                    continue;
                };
                collect_class_field_bindings(source, open + 1, close, &mut bindings);
                cursor = close + 1;
            }
            _ => cursor += 1,
        }
    }
    bindings
}

fn find_class_body_open(source: &str, mut cursor: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            b'{' => return Some(cursor),
            b';' => return None,
            _ => cursor += 1,
        }
    }
    None
}

fn collect_class_field_bindings(
    source: &str,
    body_start: usize,
    body_end: usize,
    bindings: &mut BTreeMap<usize, String>,
) {
    let bytes = source.as_bytes();
    let mut cursor = body_start;
    let mut nested_braces = 0usize;
    let mut nested_brackets = 0usize;
    let mut nested_parens = 0usize;
    while cursor < body_end && cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            b'{' => {
                nested_braces += 1;
                cursor += 1;
            }
            b'}' => {
                nested_braces = nested_braces.saturating_sub(1);
                cursor += 1;
            }
            b'[' => {
                nested_brackets += 1;
                cursor += 1;
            }
            b']' => {
                nested_brackets = nested_brackets.saturating_sub(1);
                cursor += 1;
            }
            b'(' => {
                nested_parens += 1;
                cursor += 1;
            }
            b')' => {
                nested_parens = nested_parens.saturating_sub(1);
                cursor += 1;
            }
            byte if nested_braces == 0
                && nested_brackets == 0
                && nested_parens == 0
                && is_identifier_start(byte) =>
            {
                let start = cursor;
                cursor += 1;
                while cursor < body_end
                    && cursor < bytes.len()
                    && is_identifier_continue(bytes[cursor])
                {
                    cursor += 1;
                }
                let after = skip_ws(bytes, cursor);
                if bytes
                    .get(after)
                    .is_some_and(|byte| matches!(*byte, b';' | b'='))
                {
                    bindings.insert(start, source[start..cursor].to_string());
                }
            }
            _ => cursor += 1,
        }
    }
}

fn local_bindings_in_source(source: &str) -> BTreeSet<String> {
    let mut bindings = BTreeSet::new();
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            _ if keyword_at(source, cursor, "function") => {
                if let Some((binding, next)) =
                    parse_identifier_after_keyword(source, cursor, "function")
                {
                    bindings.insert(binding.to_string());
                    cursor = next;
                } else {
                    cursor += "function".len();
                }
            }
            _ if keyword_at(source, cursor, "class") => {
                if let Some((binding, next)) =
                    parse_identifier_after_keyword(source, cursor, "class")
                {
                    bindings.insert(binding.to_string());
                    cursor = next;
                } else {
                    cursor += "class".len();
                }
            }
            _ if keyword_at(source, cursor, "var") => {
                cursor =
                    collect_local_variable_bindings(source, cursor + "var".len(), &mut bindings);
            }
            _ if keyword_at(source, cursor, "let") => {
                cursor =
                    collect_local_variable_bindings(source, cursor + "let".len(), &mut bindings);
            }
            _ if keyword_at(source, cursor, "const") => {
                cursor =
                    collect_local_variable_bindings(source, cursor + "const".len(), &mut bindings);
            }
            b'(' => {
                let Some(close) = find_matching_paren(source, cursor) else {
                    cursor += 1;
                    continue;
                };
                let after = skip_ws(bytes, close + 1);
                if !control_flow_keyword_before_paren(source, cursor)
                    && (bytes.get(after) == Some(&b'{')
                        || (bytes.get(after) == Some(&b'=') && bytes.get(after + 1) == Some(&b'>')))
                {
                    collect_binding_pattern_identifiers(&source[cursor + 1..close], &mut bindings);
                }
                cursor += 1;
            }
            byte if is_identifier_start(byte) => {
                let start = cursor;
                cursor += 1;
                while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
                    cursor += 1;
                }
                if bytes.get(skip_ws(bytes, cursor)) == Some(&b'=')
                    && bytes.get(skip_ws(bytes, cursor) + 1) == Some(&b'>')
                {
                    bindings.insert(source[start..cursor].to_string());
                }
            }
            _ => cursor += 1,
        }
    }
    bindings
}

fn collect_local_variable_bindings(
    source: &str,
    mut cursor: usize,
    bindings: &mut BTreeSet<String>,
) -> usize {
    let bytes = source.as_bytes();
    loop {
        cursor = skip_ws(bytes, cursor);
        if let Some((binding, next)) = parse_identifier(source, cursor) {
            bindings.insert(binding.to_string());
            cursor = next;
        } else if bytes.get(cursor) == Some(&b'{') {
            let Some(end) = find_matching_brace(source, cursor) else {
                return bytes.len();
            };
            collect_binding_pattern_identifiers(&source[cursor + 1..end], bindings);
            cursor = end + 1;
        } else if bytes.get(cursor) == Some(&b'[') {
            let Some(end) = find_matching_bracket(source, cursor) else {
                return bytes.len();
            };
            collect_binding_pattern_identifiers(&source[cursor + 1..end], bindings);
            cursor = end + 1;
        }

        let mut nested = 0usize;
        while cursor < bytes.len() {
            match bytes[cursor] {
                b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
                b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                    cursor = skip_line_comment(bytes, cursor + 2);
                }
                b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                    cursor = skip_block_comment(bytes, cursor + 2);
                }
                b'/' if looks_like_regex_literal(bytes, cursor) => {
                    cursor = skip_regex_literal(bytes, cursor);
                }
                b'(' => {
                    if let Some(close) = find_matching_paren(source, cursor) {
                        let after = skip_ws(bytes, close + 1);
                        if bytes.get(after) == Some(&b'=') && bytes.get(after + 1) == Some(&b'>') {
                            collect_binding_pattern_identifiers(
                                &source[cursor + 1..close],
                                bindings,
                            );
                        }
                    }
                    nested += 1;
                    cursor += 1;
                }
                b'[' | b'{' => {
                    nested += 1;
                    cursor += 1;
                }
                b')' | b']' | b'}' => {
                    if nested == 0 {
                        return cursor;
                    }
                    nested -= 1;
                    cursor += 1;
                }
                b',' if nested == 0 => {
                    cursor += 1;
                    break;
                }
                b';' if nested == 0 => return cursor + 1,
                byte if is_identifier_start(byte) => {
                    let start = cursor;
                    cursor += 1;
                    while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
                        cursor += 1;
                    }
                    let after = skip_ws(bytes, cursor);
                    if bytes.get(after) == Some(&b'=') && bytes.get(after + 1) == Some(&b'>') {
                        bindings.insert(source[start..cursor].to_string());
                    }
                }
                _ => cursor += 1,
            }
        }
        if cursor >= bytes.len() {
            return cursor;
        }
    }
}

fn collect_binding_pattern_identifiers(source: &str, bindings: &mut BTreeSet<String>) {
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            byte if is_identifier_start(byte) => {
                let start = cursor;
                cursor += 1;
                while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
                    cursor += 1;
                }
                let identifier = &source[start..cursor];
                if !is_js_keyword(identifier) && bytes.get(skip_ws(bytes, cursor)) != Some(&b':') {
                    bindings.insert(identifier.to_string());
                }
            }
            _ => cursor += 1,
        }
    }
}

fn is_runtime_global_identifier(identifier: &str) -> bool {
    matches!(
        identifier,
        "AbortController"
            | "AbortSignal"
            | "Array"
            | "ArrayBuffer"
            | "BigInt"
            | "Boolean"
            | "Buffer"
            | "DataView"
            | "Date"
            | "Error"
            | "EvalError"
            | "Float32Array"
            | "Float64Array"
            | "Function"
            | "Infinity"
            | "Int16Array"
            | "Int32Array"
            | "Int8Array"
            | "Intl"
            | "JSON"
            | "Map"
            | "Math"
            | "NaN"
            | "Number"
            | "Object"
            | "Promise"
            | "Proxy"
            | "RangeError"
            | "ReferenceError"
            | "Reflect"
            | "RegExp"
            | "Set"
            | "String"
            | "Symbol"
            | "SyntaxError"
            | "TextDecoder"
            | "TextEncoder"
            | "TypeError"
            | "URIError"
            | "URL"
            | "URLSearchParams"
            | "Uint16Array"
            | "Uint32Array"
            | "Uint8Array"
            | "Uint8ClampedArray"
            | "WeakMap"
            | "WeakSet"
            | "__dirname"
            | "__filename"
            | "clearImmediate"
            | "clearInterval"
            | "clearTimeout"
            | "console"
            | "decodeURI"
            | "decodeURIComponent"
            | "encodeURI"
            | "encodeURIComponent"
            | "exports"
            | "global"
            | "globalThis"
            | "isFinite"
            | "isNaN"
            | "module"
            | "parseFloat"
            | "parseInt"
            | "process"
            | "queueMicrotask"
            | "require"
            | "setImmediate"
            | "setInterval"
            | "setTimeout"
            | "undefined"
    )
}

fn ensure_planned_module_exports(
    plan: &mut EmitPlan,
    program: &EnrichedProgram,
    module_id: ModuleId,
    bindings: &BTreeSet<BindingName>,
) {
    let Some(path) = module_output_path(program, module_id) else {
        return;
    };
    let Some(file) = plan.files.iter_mut().find(|file| file.path == path) else {
        return;
    };
    let existing = file
        .exports
        .iter()
        .map(|export| export.binding.clone())
        .collect::<BTreeSet<_>>();
    let missing = bindings
        .iter()
        .filter(|binding| !existing.contains(*binding))
        .cloned()
        .collect::<BTreeSet<_>>();
    if missing.is_empty() {
        return;
    }
    file.push_source(named_export_statement(missing.iter()));
    for binding in missing {
        file.add_export_with_source_backed(binding, true);
    }
}

fn runtime_namespace_exports_for_helpers(
    prelude: &RuntimePrelude,
    helper_bindings: &BTreeSet<BindingName>,
) -> Vec<RuntimeNamespaceExport> {
    prelude
        .namespace_exports
        .iter()
        .filter(|namespace_export| helper_bindings.contains(&namespace_export.namespace))
        .cloned()
        .collect()
}

fn previous_non_ws(bytes: &[u8], before: usize) -> Option<usize> {
    let mut cursor = before.checked_sub(1)?;
    while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
        cursor = cursor.checked_sub(1)?;
    }
    Some(cursor)
}

fn externalized_package_modules(program: &EnrichedProgram) -> BTreeSet<ModuleId> {
    program
        .model()
        .input()
        .package_attributions
        .iter()
        .filter(|attribution| {
            attribution.status == PackageAttributionStatus::Accepted
                && attribution.emission_mode == PackageEmissionMode::ExternalImport
        })
        .map(|attribution| attribution.module_id)
        .collect()
}

fn accepted_external_package_attribution(
    program: &EnrichedProgram,
    module_id: ModuleId,
) -> Option<&PackageAttributionInput> {
    program
        .model()
        .input()
        .package_attributions
        .iter()
        .find(|attribution| {
            attribution.module_id == module_id
                && attribution.status == PackageAttributionStatus::Accepted
                && attribution.emission_mode == PackageEmissionMode::ExternalImport
                && attribution.export_specifier.is_some()
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExternalPackageAdapterKind {
    CommonJsWrapper,
    NamespaceReturn,
}

fn external_package_adapter_kind(
    program: &EnrichedProgram,
    module_id: ModuleId,
    adapter_bindings: &BTreeSet<BindingName>,
) -> Option<ExternalPackageAdapterKind> {
    let Some(source) = program.model().input().module_source_slice(module_id) else {
        return None;
    };
    let compact = compact_js_source(source.source);
    if compact.contains("let_$cached;")
        && compact.contains("return_$module.exports;")
        && compact.contains("export{")
    {
        return Some(ExternalPackageAdapterKind::CommonJsWrapper);
    }
    if adapter_bindings
        .iter()
        .any(|binding| compact.contains(format!("var{}=p(", binding.as_str()).as_str()))
    {
        return Some(ExternalPackageAdapterKind::CommonJsWrapper);
    }
    if adapter_bindings
        .iter()
        .all(|binding| !compact_source_defines_binding(compact.as_str(), binding.as_str()))
    {
        return Some(ExternalPackageAdapterKind::NamespaceReturn);
    }
    None
}

fn compact_source_defines_binding(compact_source: &str, binding: &str) -> bool {
    [
        format!("function{binding}("),
        format!("var{binding}="),
        format!("let{binding}="),
        format!("const{binding}="),
        format!("class{binding}"),
    ]
    .iter()
    .any(|needle| compact_source.contains(needle))
}

fn populate_external_package_adapter_file(
    file: &mut PlannedFile,
    attribution: &PackageAttributionInput,
    exportable_bindings: &BTreeSet<BindingName>,
    adapter_kind: ExternalPackageAdapterKind,
) {
    let Some(specifier) = attribution.export_specifier.as_deref() else {
        return;
    };
    let namespace = external_package_adapter_namespace(attribution, exportable_bindings);
    file.add_import(PlannedImport {
        namespace: namespace.clone(),
        resolution: PackageResolution::External {
            specifier: specifier.to_string(),
            package_name: attribution.package_name.clone(),
            import_attributes: import_attributes_for_package_attribution(attribution),
        },
        source_backed: false,
    });
    let return_expression =
        external_package_adapter_return_expression(namespace.as_str(), adapter_kind);
    for binding in exportable_bindings {
        file.push_source(format!(
            "function {}() {{ return {}; }}",
            binding.as_str(),
            return_expression
        ));
        file.add_binding(PlannedBinding::new(
            binding.clone(),
            binding.clone(),
            BindingShape::Callable,
            true,
        ));
    }
    file.push_source(named_export_statement(exportable_bindings.iter()));
    for binding in exportable_bindings {
        file.add_export_with_source_backed(binding.clone(), true);
    }
}

fn external_package_adapter_return_expression(
    namespace: &str,
    adapter_kind: ExternalPackageAdapterKind,
) -> String {
    match adapter_kind {
        ExternalPackageAdapterKind::CommonJsWrapper => format!(
            "Object.prototype.hasOwnProperty.call({namespace}, \"default\") ? {namespace}.default : {namespace}"
        ),
        ExternalPackageAdapterKind::NamespaceReturn => namespace.to_string(),
    }
}

fn package_adapter_export_bindings(
    program: &EnrichedProgram,
    module_id: ModuleId,
    source_facts: &SourceModuleFacts,
) -> BTreeSet<BindingName> {
    let mut bindings: BTreeSet<BindingName> = program
        .model()
        .graph()
        .import_export()
        .exports_for(module_id)
        .into_iter()
        .collect();
    let target_bindings = source_facts
        .exportable_bindings_by_module
        .get(&module_id)
        .cloned()
        .unwrap_or_else(|| source_exportable_bindings(program, module_id));
    for dependency in &program.model().input().dependencies {
        let ModuleDependencyTarget::Module(target_module_id) = dependency.target else {
            continue;
        };
        if target_module_id != module_id {
            continue;
        }
        let Some(candidate_reads) = source_facts
            .candidate_reads_by_module
            .get(&dependency.from_module_id)
        else {
            continue;
        };
        bindings.extend(candidate_reads.intersection(&target_bindings).cloned());
    }
    bindings
}

fn external_package_adapter_namespace(
    attribution: &PackageAttributionInput,
    exportable_bindings: &BTreeSet<BindingName>,
) -> BindingName {
    let sanitized = sanitize_identifier(attribution.package_name.as_str());
    let base = if sanitized.is_empty() {
        "externalPackage".to_string()
    } else {
        format!("external_{sanitized}")
    };
    if !exportable_bindings
        .iter()
        .any(|binding| binding.as_str() == base.as_str())
    {
        return BindingName::new(base);
    }
    BindingName::new(format!("{base}Namespace"))
}

fn import_attributes_for_package_attribution(
    attribution: &PackageAttributionInput,
) -> BTreeMap<String, String> {
    if attribution
        .resolved_file
        .as_deref()
        .is_some_and(is_json_resolved_file)
    {
        return BTreeMap::from([("type".to_string(), "json".to_string())]);
    }
    BTreeMap::new()
}

fn is_json_resolved_file(resolved_file: &str) -> bool {
    resolved_file
        .split(['?', '#'])
        .next()
        .unwrap_or(resolved_file)
        .trim()
        .to_ascii_lowercase()
        .ends_with(".json")
}

fn source_required_package_modules(
    program: &EnrichedProgram,
    externalized_packages: &BTreeSet<ModuleId>,
    source_facts: &SourceModuleFacts,
) -> BTreeSet<ModuleId> {
    let candidate_reads_by_module = &source_facts.candidate_reads_by_module;
    let definition_modules = &source_facts.definition_modules_all;
    let modules_by_id = program
        .model()
        .modules()
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let exportable_bindings_by_module = &source_facts.exportable_bindings_by_module;
    let mut required = BTreeSet::new();

    loop {
        let mut changed = false;
        for dependency in &program.model().input().dependencies {
            let ModuleDependencyTarget::Module(target_module_id) = dependency.target else {
                continue;
            };
            if !externalized_packages.contains(&target_module_id)
                || required.contains(&target_module_id)
            {
                continue;
            }
            let Some(from_module) = modules_by_id.get(&dependency.from_module_id) else {
                continue;
            };
            if from_module.kind == ModuleKind::Package
                && externalized_packages.contains(&from_module.id)
                && !required.contains(&from_module.id)
            {
                continue;
            }
            let Some(candidate_reads) = candidate_reads_by_module.get(&dependency.from_module_id)
            else {
                continue;
            };
            let Some(target_bindings) = exportable_bindings_by_module.get(&target_module_id) else {
                continue;
            };
            if candidate_reads.is_disjoint(target_bindings) {
                continue;
            }
            required.insert(target_module_id);
            changed = true;
        }
        for (from_module_id, candidate_reads) in candidate_reads_by_module {
            let Some(from_module) = modules_by_id.get(from_module_id) else {
                continue;
            };
            if from_module.kind == ModuleKind::Package
                && externalized_packages.contains(&from_module.id)
                && !required.contains(&from_module.id)
            {
                continue;
            }
            for binding in candidate_reads {
                let Some(Some(target_module_id)) = definition_modules.get(binding) else {
                    continue;
                };
                if target_module_id == from_module_id
                    || !externalized_packages.contains(target_module_id)
                    || required.contains(target_module_id)
                {
                    continue;
                }
                required.insert(*target_module_id);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    required
}

fn source_module_wiring(
    program: &EnrichedProgram,
    externalized_packages: &BTreeSet<ModuleId>,
    source_facts: &SourceModuleFacts,
) -> SourceModuleWiring {
    let mut wiring = SourceModuleWiring::default();
    let candidate_reads_by_module = &source_facts.candidate_reads_by_module;
    let modules_by_id = program
        .model()
        .modules()
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let exportable_bindings_by_module = &source_facts.exportable_bindings_by_module;

    for dependency in &program.model().input().dependencies {
        let ModuleDependencyTarget::Module(target_module_id) = dependency.target else {
            continue;
        };
        let Some(from_module) = modules_by_id.get(&dependency.from_module_id) else {
            continue;
        };
        let Some(target_module) = modules_by_id.get(&target_module_id) else {
            continue;
        };
        if (from_module.kind == ModuleKind::Package
            && externalized_packages.contains(&from_module.id))
            || (target_module.kind == ModuleKind::Package
                && externalized_packages.contains(&target_module.id))
        {
            continue;
        }
        let Some(candidate_reads) = candidate_reads_by_module.get(&dependency.from_module_id)
        else {
            continue;
        };
        let Some(target_bindings) = exportable_bindings_by_module.get(&target_module_id) else {
            continue;
        };
        let imported_bindings = candidate_reads
            .intersection(target_bindings)
            .cloned()
            .collect::<BTreeSet<_>>();
        if imported_bindings.is_empty() {
            continue;
        }
        wiring
            .imports_by_module
            .entry(dependency.from_module_id)
            .or_default()
            .entry(target_module_id)
            .or_default()
            .extend(imported_bindings.iter().cloned());
        wiring
            .exports_by_module
            .entry(target_module_id)
            .or_default()
            .extend(imported_bindings);
    }

    wiring
}

/// Cross-module eager-safety analysis: for each target module's exported
/// binding, decide whether collapsing its lazy thunk into a direct value
/// is safe **given how every other module observes that binding**.
///
/// A binding `X` exported by module `M` is "eager-safe" iff:
///   1. `M` is a singleton SCC in the top-level module-dependency graph
///      (no cycle whose every edge is a top-level reference passes through
///      `M` — see [`build_top_level_dep_graph`]).
///   2. Every consumer module that imports `X` references it exclusively
///      from inside function / arrow / method bodies (its
///      [`ImportUsageScope`] is `NestedOnly`). Top-level use anywhere
///      forces the lazy semantics to be observable.
///
/// Note: per-module body purity (literal / class / function RHS) is **not**
/// checked here — that's the existing `delazify_pure_value_bindings` /
/// `delazify_pure_module_bindings` filter and applies per-binding inside
/// the lowering pass. The eager-safety analysis only adds the missing
/// cross-module dimension.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct EagerSafeAnalysis {
    /// For each target module, the subset of its exported bindings that
    /// pass the cross-module eager-safety check.
    eager_safe_exports_by_module: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    /// For each consumer module, the set of import names that resolve
    /// to bindings the fixpoint marked eager-safe. The lowering pass
    /// passes this set to the body extractor so `EagerWithDeps` bodies
    /// (zero-arg calls to imported thunks) can extract their value
    /// when every dep would itself have been eagerified.
    safe_call_targets_by_module: BTreeMap<ModuleId, BTreeSet<String>>,
}

const CROSS_MODULE_EAGER_SAFE_ANALYSIS_MODULE_LIMIT: usize = 1024;

fn should_compute_cross_module_eager_safe_analysis(program: &EnrichedProgram) -> bool {
    program.model().modules().len() <= CROSS_MODULE_EAGER_SAFE_ANALYSIS_MODULE_LIMIT
        || std::env::var_os("REVERTS_CROSS_MODULE_EAGER_SAFE").is_some()
}

fn compute_eager_safe_analysis(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
) -> EagerSafeAnalysis {
    let usage_scopes = compute_consumer_usage_scopes(program, source_module_wiring);
    let call_forms = compute_consumer_call_forms(program, source_module_wiring);
    let singleton_modules = singleton_scc_modules(program, source_module_wiring, &usage_scopes);
    // Only bindings declared as `var X = <lazy_helper>(...)` in their
    // exporting module are eagerification candidates — a regular function
    // or value export is already "direct" and any consumer `X()` call is
    // already calling it correctly (not invoking a thunk).
    let thunk_wrapped_exports = compute_thunk_wrapped_exports(program);
    // Additional gate: only bindings whose BODY actually passes the
    // delazify-extraction check qualify for eagerification. The
    // prediction also reports per-consumer `safe_call_targets` so the
    // lowering extractor can accept `EagerWithDeps` bodies whose deps
    // would themselves eagerify.
    let prediction = predict_delazifiable_exports(program, source_module_wiring);
    let mut eager_safe_exports_by_module = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
    for (target_id, exported_bindings) in &source_module_wiring.exports_by_module {
        if !singleton_modules.contains(target_id) {
            continue;
        }
        let Some(thunk_wrapped) = thunk_wrapped_exports.get(target_id) else {
            continue;
        };
        let Some(delazifiable) = prediction.delazifiable_exports_by_module.get(target_id) else {
            continue;
        };
        let mut safe = BTreeSet::<BindingName>::new();
        'each_export: for binding in exported_bindings {
            if !thunk_wrapped.contains(binding) {
                continue;
            }
            if !delazifiable.contains(binding) {
                continue;
            }
            // For every consumer that imports this binding, the consumer
            // must reference it exclusively in the zero-arg `X()` call
            // shape — the only pattern the cross-module rewriter knows
            // how to mechanically convert to a bare `X` once M emits the
            // direct value. Body-purity is checked separately inside
            // `lower_runtime_helpers` (we never delazify an impure RHS),
            // so call-form is the only additional gate Phase 8 introduces
            // on top of SCC clearance.
            for (consumer_id, imports_by_target) in &source_module_wiring.imports_by_module {
                let Some(imported_from_target) = imports_by_target.get(target_id) else {
                    continue;
                };
                if !imported_from_target.contains(binding) {
                    continue;
                }
                let Some(consumer_call_forms) = call_forms.get(consumer_id) else {
                    continue 'each_export;
                };
                if !consumer_call_forms
                    .get(binding.as_str())
                    .copied()
                    .unwrap_or(false)
                {
                    continue 'each_export;
                }
            }
            safe.insert(binding.clone());
        }
        if !safe.is_empty() {
            eager_safe_exports_by_module.insert(*target_id, safe);
        }
    }
    EagerSafeAnalysis {
        eager_safe_exports_by_module,
        safe_call_targets_by_module: prediction.safe_call_targets_by_module,
    }
}

/// For each consumer module, classify every imported binding by whether
/// its uses in that consumer are all the zero-arg call shape `X()`. A
/// binding can be eagerified only if every consumer that references it
/// passes this check — otherwise cross-module rewriting can't
/// mechanically convert `X()` (returning the thunk's value) into `X`
/// (the value directly).
fn compute_consumer_call_forms(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
) -> BTreeMap<ModuleId, BTreeMap<String, bool>> {
    let mut out = BTreeMap::new();
    for (consumer_id, imports_by_target) in &source_module_wiring.imports_by_module {
        let Some(source_slice) = program.model().input().module_source_slice(*consumer_id) else {
            continue;
        };
        let binding_names: BTreeSet<String> = imports_by_target
            .values()
            .flatten()
            .map(|binding| binding.as_str().to_string())
            .collect();
        if binding_names.is_empty() {
            continue;
        }
        let call_forms = verify_only_immediate_call_references(
            source_slice.source,
            &binding_names,
            Some(std::path::Path::new(source_slice.source_file_path)),
            ParseGoal::TypeScript,
        );
        out.insert(*consumer_id, call_forms);
    }
    out
}

/// Parse every consumer module's source slice once and classify the usage
/// scope of every binding it imports. Returns a map keyed by consumer
/// module id, with values keyed by the imported binding's identifier.
fn compute_consumer_usage_scopes(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
) -> BTreeMap<ModuleId, BTreeMap<String, ImportUsageScope>> {
    let mut out = BTreeMap::new();
    for (consumer_id, imports_by_target) in &source_module_wiring.imports_by_module {
        let Some(source_slice) = program.model().input().module_source_slice(*consumer_id) else {
            continue;
        };
        let binding_names: BTreeSet<String> = imports_by_target
            .values()
            .flatten()
            .map(|binding| binding.as_str().to_string())
            .collect();
        if binding_names.is_empty() {
            continue;
        }
        let scopes = classify_import_usage_scope(
            source_slice.source,
            &binding_names,
            Some(std::path::Path::new(source_slice.source_file_path)),
            ParseGoal::TypeScript,
        );
        out.insert(*consumer_id, scopes);
    }
    out
}

/// Compute the set of modules that are singleton SCCs in the
/// top-level-only module dependency graph: edges include only the
/// references a consumer makes at its own top-level (not the ones nested
/// inside fn/arrow/method bodies). Modules in singleton SCCs are not part
/// of any module-evaluation cycle and can therefore be eagerified without
/// reordering observable side effects.
fn singleton_scc_modules(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
    usage_scopes: &BTreeMap<ModuleId, BTreeMap<String, ImportUsageScope>>,
) -> BTreeSet<ModuleId> {
    use petgraph::algo::tarjan_scc;
    use petgraph::graph::{DiGraph, NodeIndex};
    let mut graph: DiGraph<ModuleId, ()> = DiGraph::new();
    let mut node_by_module = BTreeMap::<ModuleId, NodeIndex>::new();
    for module in program.model().modules() {
        let idx = graph.add_node(module.id);
        node_by_module.insert(module.id, idx);
    }
    for (consumer_id, imports_by_target) in &source_module_wiring.imports_by_module {
        let consumer_scopes = usage_scopes.get(consumer_id);
        let Some(&consumer_idx) = node_by_module.get(consumer_id) else {
            continue;
        };
        for (target_id, bindings) in imports_by_target {
            let Some(&target_idx) = node_by_module.get(target_id) else {
                continue;
            };
            let has_top_level_use = bindings.iter().any(|binding| {
                consumer_scopes
                    .and_then(|scopes| scopes.get(binding.as_str()))
                    .copied()
                    == Some(ImportUsageScope::TopLevel)
            });
            if has_top_level_use {
                graph.add_edge(consumer_idx, target_idx, ());
            }
        }
    }
    let sccs = tarjan_scc(&graph);
    let mut singleton = BTreeSet::new();
    for scc in &sccs {
        if scc.len() != 1 {
            continue;
        }
        let node = scc[0];
        // Reject if there's a self-loop — that's a (trivial) cycle.
        if graph.find_edge(node, node).is_some() {
            continue;
        }
        singleton.insert(graph[node]);
    }
    singleton
}

/// For each module, find every top-level binding declared as
/// `var X = HELPER(...)` where HELPER is a lazy-wrapping helper from
/// the bundle's runtime prelude (CommonJS wrapper or lazy initializer).
/// These are the only exports the cross-module eager-safety analysis
/// is allowed to consider as candidates — non-thunk exports (a literal,
/// a function declaration, a class) are already direct and their
/// consumer call sites are not zero-arg thunk invocations.
fn compute_thunk_wrapped_exports(
    program: &EnrichedProgram,
) -> BTreeMap<ModuleId, BTreeSet<BindingName>> {
    let mut out = BTreeMap::new();
    for module in program.model().modules() {
        let Some(source) = program.model().input().module_source_slice(module.id) else {
            continue;
        };
        let runtime_imports = program.model().graph().runtime_imports_for(module.id);
        let mut helper_kinds = runtime_helper_kinds(program.model().graph(), &runtime_imports);
        helper_kinds.extend(runtime_helper_kinds_for_source(
            program.model().graph(),
            source.source_file_id,
            source.source,
        ));
        let lazy_helpers: BTreeSet<&str> = helper_kinds
            .iter()
            .filter(|(_, kind)| {
                matches!(
                    kind,
                    RuntimePreludeBindingKind::CommonJsWrapper
                        | RuntimePreludeBindingKind::LazyInitializer
                )
            })
            .map(|(binding, _)| binding.as_str())
            .collect();
        if lazy_helpers.is_empty() {
            continue;
        }
        let thunk_bindings = scan_thunk_wrapped_bindings(source.source, &lazy_helpers);
        if !thunk_bindings.is_empty() {
            out.insert(module.id, thunk_bindings);
        }
    }
    out
}

/// Scan `source` for every `var/let/const X = HELPER(...)` declaration
/// where HELPER is one of `lazy_helpers`, and return the binding name X.
/// The scan is byte-level (skipping quoted strings, comments, regex
/// literals, templates) — same conventions as the existing
/// declaration scanners in this module.
fn scan_thunk_wrapped_bindings(
    source: &str,
    lazy_helpers: &BTreeSet<&str>,
) -> BTreeSet<BindingName> {
    let mut out = BTreeSet::new();
    let bytes = source.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        let Some(keyword) = ["var", "let", "const"]
            .into_iter()
            .find(|kw| keyword_at(source, cursor, kw))
        else {
            cursor += 1;
            continue;
        };
        let mut c = cursor + keyword.len();
        c = skip_ws(bytes, c);
        let Some((binding_name, after_binding)) = parse_identifier(source, c) else {
            cursor += 1;
            continue;
        };
        c = skip_ws(bytes, after_binding);
        if bytes.get(c) != Some(&b'=') {
            cursor = after_binding;
            continue;
        }
        c = skip_ws(bytes, c + 1);
        let Some((helper_name, after_helper)) = parse_identifier(source, c) else {
            cursor = after_binding;
            continue;
        };
        c = skip_ws(bytes, after_helper);
        if bytes.get(c) != Some(&b'(') {
            cursor = after_helper;
            continue;
        }
        if lazy_helpers.contains(helper_name) {
            out.insert(BindingName::new(binding_name));
        }
        cursor = after_helper;
    }
    out
}

/// For each module, predict the subset of its thunk-wrapped exports
/// whose body would actually delazify — including transitively, via
/// the inter-procedural fixpoint over zero-arg thunk-call dependencies.
///
/// Pipeline:
///   1. Enumerate every `var X = HELPER((params) => { BODY })` across
///      every module and classify each BODY via the AST-level
///      `classify_lazy_module_body`. Outcomes:
///         * `Eager` — body has a value with no calls; immediately safe.
///         * `EagerWithDeps` — body composes a value but invokes one or
///           more zero-arg bindings; safe iff those bindings are
///           themselves eager-safe.
///         * `Impure` — body has unrecognized side effects; never safe.
///   2. Build a per-module name-resolution table from
///      `source_module_wiring.imports_by_module` so each `call_deps`
///      identifier in step 1 can be mapped to a `(target_module, binding)`
///      pair. Local thunks (declared in the consumer module itself)
///      resolve to that same module's `(M, name)`.
///   3. Fixpoint: seed with all `Eager` bindings, then loop: add
///      `EagerWithDeps{deps}` bindings where every resolved dep already
///      lives in the safe set, until stable. Mutual recursion (cycles
///      in the dep graph) keep both sides unsafe — neither can be added
///      without the other already in the set.
fn predict_delazifiable_exports(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
) -> EagerSafetyPrediction {
    let classifications = enumerate_and_classify_lazy_bindings(program);
    let resolution = build_dep_resolution_map(program, source_module_wiring, &classifications);
    let safe_keys = compute_eager_safe_fixpoint(&classifications, &resolution);
    let mut delazifiable_exports_by_module: BTreeMap<ModuleId, BTreeSet<BindingName>> =
        BTreeMap::new();
    for (module_id, binding) in &safe_keys {
        delazifiable_exports_by_module
            .entry(*module_id)
            .or_default()
            .insert(binding.clone());
    }
    let mut safe_call_targets_by_module: BTreeMap<ModuleId, BTreeSet<String>> = BTreeMap::new();
    for module in program.model().modules() {
        let names = build_eager_safe_call_targets_for_module(module.id, &safe_keys, &resolution);
        if !names.is_empty() {
            safe_call_targets_by_module.insert(module.id, names);
        }
    }
    EagerSafetyPrediction {
        delazifiable_exports_by_module,
        safe_call_targets_by_module,
    }
}

/// Bundle of outputs from the inter-procedural fixpoint. Both fields
/// are needed by the lowering: the exports set tells the cross-module
/// rewriter which consumer `X()` calls to strip, and the call-targets
/// set tells the body extractor which thunk-call deps to treat as
/// "already handled by the producer's eagerification" — i.e., drop
/// from the consumer's prologue.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct EagerSafetyPrediction {
    delazifiable_exports_by_module: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    safe_call_targets_by_module: BTreeMap<ModuleId, BTreeSet<String>>,
}

/// Walk every module, find each `var X = HELPER((params) => { BODY })`
/// declaration where HELPER is a lazy wrapper, and classify the BODY
/// via the AST classifier in `reverts_js`. Returns the classification
/// keyed by `(module_id, binding)`.
fn enumerate_and_classify_lazy_bindings(
    program: &EnrichedProgram,
) -> BTreeMap<(ModuleId, BindingName), reverts_js::LazyBodyClassification> {
    let mut classifications = BTreeMap::new();
    for module in program.model().modules() {
        let Some(source) = program.model().input().module_source_slice(module.id) else {
            continue;
        };
        let runtime_imports = program.model().graph().runtime_imports_for(module.id);
        let mut helper_kinds = runtime_helper_kinds(program.model().graph(), &runtime_imports);
        helper_kinds.extend(runtime_helper_kinds_for_source(
            program.model().graph(),
            source.source_file_id,
            source.source,
        ));
        let module_helpers: BTreeSet<&str> = helper_kinds
            .iter()
            .filter(|(_, kind)| matches!(kind, RuntimePreludeBindingKind::CommonJsWrapper))
            .map(|(binding, _)| binding.as_str())
            .collect();
        let value_helpers: BTreeSet<&str> = helper_kinds
            .iter()
            .filter(|(_, kind)| matches!(kind, RuntimePreludeBindingKind::LazyInitializer))
            .map(|(binding, _)| binding.as_str())
            .collect();
        if module_helpers.is_empty() && value_helpers.is_empty() {
            continue;
        }
        scan_and_classify_lazy_bindings_in_module(
            source.source,
            &module_helpers,
            &value_helpers,
            module.id,
            &mut classifications,
        );
    }
    classifications
}

/// Companion to `enumerate_and_classify_lazy_bindings`: for one module,
/// scan its source for lazy declarations and stash the body classification.
fn scan_and_classify_lazy_bindings_in_module(
    source: &str,
    commonjs_helpers: &BTreeSet<&str>,
    lazy_value_helpers: &BTreeSet<&str>,
    module_id: ModuleId,
    out: &mut BTreeMap<(ModuleId, BindingName), reverts_js::LazyBodyClassification>,
) {
    let bytes = source.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        let Some(keyword) = ["var", "let", "const"]
            .into_iter()
            .find(|kw| keyword_at(source, cursor, kw))
        else {
            cursor += 1;
            continue;
        };
        let mut c = cursor + keyword.len();
        c = skip_ws(bytes, c);
        let Some((binding_name, after_binding)) = parse_identifier(source, c) else {
            cursor += 1;
            continue;
        };
        c = skip_ws(bytes, after_binding);
        if bytes.get(c) != Some(&b'=') {
            cursor = after_binding;
            continue;
        }
        c = skip_ws(bytes, c + 1);
        let Some((helper_name, after_helper)) = parse_identifier(source, c) else {
            cursor = after_binding;
            continue;
        };
        c = skip_ws(bytes, after_helper);
        if bytes.get(c) != Some(&b'(') {
            cursor = after_helper;
            continue;
        }
        let is_commonjs = commonjs_helpers.contains(helper_name);
        let is_lazy_value = lazy_value_helpers.contains(helper_name);
        if !is_commonjs && !is_lazy_value {
            cursor = after_helper;
            continue;
        }
        c = skip_ws(bytes, c + 1);
        let (exports_param, module_param, body_start, body_end) =
            match parse_lazy_factory_signature(source, c, is_commonjs) {
                Some(parts) => parts,
                None => {
                    cursor = after_helper;
                    continue;
                }
            };
        let body = &source[body_start..body_end];
        let classification = reverts_js::classify_lazy_module_body(
            body,
            exports_param,
            module_param,
            None,
            ParseGoal::TypeScript,
        );
        if !matches!(classification, reverts_js::LazyBodyClassification::Impure) {
            out.insert((module_id, BindingName::new(binding_name)), classification);
        }
        cursor = body_end;
    }
}

/// For each consumer module, build a `name → (target_module, binding)`
/// table so dep names appearing in lazy bodies can be resolved.
/// Combines two sources:
///   * Cross-module imports recorded in `source_module_wiring` — every
///     imported binding is mapped to its target module.
///   * Local thunk bindings — if a body calls `localX()` and the same
///     module has `var localX = lazyValue(...)`, that resolves to
///     `(self, localX)`.
fn build_dep_resolution_map(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
    _classifications: &BTreeMap<(ModuleId, BindingName), reverts_js::LazyBodyClassification>,
) -> BTreeMap<ModuleId, BTreeMap<String, (ModuleId, BindingName)>> {
    // Only cross-module imports are resolved as eager-safe call deps —
    // local thunks within the same module would need source-order
    // verification (a thunk declared AFTER its consumer can't be
    // referenced before its declaration runs) which we don't yet
    // perform. Restricting to imports is the conservative direction:
    // local-only chains stay lazy; cross-module chains get the
    // fixpoint benefit.
    let mut out: BTreeMap<ModuleId, BTreeMap<String, (ModuleId, BindingName)>> = BTreeMap::new();
    for module in program.model().modules() {
        let entry = out.entry(module.id).or_default();
        if let Some(targets) = source_module_wiring.imports_by_module.get(&module.id) {
            for (target_id, bindings) in targets {
                for binding in bindings {
                    entry.insert(binding.as_str().to_string(), (*target_id, binding.clone()));
                }
            }
        }
    }
    out
}

/// Standard worklist fixpoint over the dep graph. Seeds with `Eager`
/// bindings (no deps), then iteratively adds `EagerWithDeps` bindings
/// whose dependencies all resolve to entries already in the safe set.
/// O(N × max-deps) per round; converges in a small number of rounds
/// in practice because most chains are shallow.
///
/// Note: the fixpoint result is used both to gate cross-module
/// rewriting (consumer `X()` → `X`) and to gate value extraction in
/// the lowering pass — the matching extractor
/// `extract_lazy_module_eager_value_with_safe_deps` accepts
/// `EagerWithDeps` bindings whose every dep is in the safe set, so
/// producer and consumer agree on whether the binding emits as a
/// direct value.
fn compute_eager_safe_fixpoint(
    classifications: &BTreeMap<(ModuleId, BindingName), reverts_js::LazyBodyClassification>,
    resolution: &BTreeMap<ModuleId, BTreeMap<String, (ModuleId, BindingName)>>,
) -> BTreeSet<(ModuleId, BindingName)> {
    let mut safe: BTreeSet<(ModuleId, BindingName)> = BTreeSet::new();
    for (key, classification) in classifications {
        if matches!(
            classification,
            reverts_js::LazyBodyClassification::Eager { .. }
        ) {
            safe.insert(key.clone());
        }
    }
    loop {
        let mut added = false;
        for (key, classification) in classifications {
            if safe.contains(key) {
                continue;
            }
            let reverts_js::LazyBodyClassification::EagerWithDeps { call_deps, .. } =
                classification
            else {
                continue;
            };
            let module_resolution = resolution.get(&key.0);
            let all_deps_safe = call_deps.iter().all(|name| {
                module_resolution
                    .and_then(|r| r.get(name))
                    .map(|resolved| safe.contains(resolved))
                    .unwrap_or(false)
            });
            if all_deps_safe {
                safe.insert(key.clone());
                added = true;
            }
        }
        if !added {
            break;
        }
    }
    safe
}

/// For a given module M, project the global fixpoint result onto the
/// names visible in M's scope. Returns the set of names X such that
/// `X()` (zero-arg call) in M's body resolves to a binding that the
/// fixpoint marked eager-safe — feeds into
/// `extract_lazy_module_eager_value_with_safe_deps` when lowering M.
fn build_eager_safe_call_targets_for_module(
    module_id: ModuleId,
    safe_keys: &BTreeSet<(ModuleId, BindingName)>,
    resolution: &BTreeMap<ModuleId, BTreeMap<String, (ModuleId, BindingName)>>,
) -> BTreeSet<String> {
    let Some(module_resolution) = resolution.get(&module_id) else {
        return BTreeSet::new();
    };
    module_resolution
        .iter()
        .filter_map(|(name, resolved)| {
            if safe_keys.contains(resolved) {
                Some(name.clone())
            } else {
                None
            }
        })
        .collect()
}

/// Parse the `((exports[, module]) => { body })` signature inside a
/// lazy-helper call, starting from the byte right after the helper's
/// opening `(`. Returns the parameter names and the byte range of the
/// arrow body (exclusive of the surrounding braces).
fn parse_lazy_factory_signature(
    source: &str,
    open_paren_after_helper: usize,
    expect_two_params: bool,
) -> Option<(&str, Option<&str>, usize, usize)> {
    let bytes = source.as_bytes();
    let mut c = open_paren_after_helper;
    if bytes.get(c) != Some(&b'(') {
        return None;
    }
    c = skip_ws(bytes, c + 1);
    let (exports_param, after_exports) = if expect_two_params {
        let (name, after) = parse_identifier(source, c)?;
        c = skip_ws(bytes, after);
        (name, after)
    } else {
        // lazyValue arrow: `() => { ... }`. Allow empty parameter list.
        if bytes.get(c) == Some(&b')') {
            ("", c)
        } else {
            return None;
        }
    };
    let _ = after_exports;
    let module_param = if expect_two_params && bytes.get(c) == Some(&b',') {
        c = skip_ws(bytes, c + 1);
        let (name, after) = parse_identifier(source, c)?;
        c = skip_ws(bytes, after);
        Some(name)
    } else {
        None
    };
    if bytes.get(c) != Some(&b')') {
        return None;
    }
    c = skip_ws(bytes, c + 1);
    let arrow_end = expect_arrow(bytes, c)?;
    c = skip_ws(bytes, arrow_end);
    if bytes.get(c) != Some(&b'{') {
        return None;
    }
    let body_end = find_matching_brace(source, c)?;
    Some((exports_param, module_param, c + 1, body_end))
}

/// For a given consumer module, gather every imported binding the
/// cross-module eager-safety analysis cleared. These are the call sites
/// `lowered_runtime_sources` must rewrite from `X()` to `X` so the
/// consumer can see the import's now-direct value rather than a missing
/// thunk function.
fn consumer_eagerified_imports(
    consumer_id: ModuleId,
    source_module_wiring: &SourceModuleWiring,
    eager_safe_analysis: &EagerSafeAnalysis,
) -> BTreeSet<BindingName> {
    let mut out = BTreeSet::new();
    let Some(imports_by_target) = source_module_wiring.imports_by_module.get(&consumer_id) else {
        return out;
    };
    for (target_id, bindings) in imports_by_target {
        let Some(eager_safe) = eager_safe_analysis
            .eager_safe_exports_by_module
            .get(target_id)
        else {
            continue;
        };
        for binding in bindings.intersection(eager_safe) {
            out.insert(binding.clone());
        }
    }
    out
}

/// Mechanically rewrite every `X()` zero-arg call site (where `X` is one
/// of `eagerified_imports`) to a bare `X`. The cross-module eager-safe
/// analysis has already verified upstream that every reference to each
/// binding is in this exact shape, so the rewrite cannot lose precision.
/// Property-access uses (`obj.X`) and non-value occurrences (import
/// specifiers, export specifiers) are correctly skipped via the same
/// classifier the local delazify pass uses.
fn rewrite_eagerified_call_sites(
    source: &str,
    eagerified_imports: &BTreeSet<BindingName>,
) -> String {
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let bytes = source.as_bytes();
    for binding in eagerified_imports {
        let target = binding.as_str();
        let mut cursor = 0;
        while cursor < bytes.len() {
            if let Some(next) = skip_non_code_at(source, cursor) {
                cursor = next;
                continue;
            }
            if !is_identifier_start(bytes[cursor]) {
                cursor += 1;
                continue;
            }
            let start = cursor;
            cursor += 1;
            while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
                cursor += 1;
            }
            if &source[start..cursor] != target {
                continue;
            }
            // Skip property access (`obj.X` / `obj#X`).
            if let Some(prev) = previous_non_ws(bytes, start)
                && matches!(bytes[prev], b'.' | b'#')
            {
                continue;
            }
            // Skip non-value occurrences (import specifier, etc.).
            if !identifier_occurrence_is_value_reference(source, start, cursor) {
                continue;
            }
            // Require zero-arg call shape `X()` — verified by the
            // eager-safe analysis to be the only shape present.
            let after = skip_ws(bytes, cursor);
            if bytes.get(after) != Some(&b'(') {
                continue;
            }
            let inner = skip_ws(bytes, after + 1);
            if bytes.get(inner) != Some(&b')') {
                continue;
            }
            edits.push((start, inner + 1, target.to_string()));
            cursor = inner + 1;
        }
    }
    if edits.is_empty() {
        return source.to_string();
    }
    edits.sort_by_key(|(start, _, _)| *start);
    let mut output = String::with_capacity(source.len());
    let mut cursor = 0;
    for (start, end, replacement) in &edits {
        debug_assert!(*start >= cursor, "cross-module rewrites must not overlap");
        output.push_str(&source[cursor..*start]);
        output.push_str(replacement);
        cursor = *end;
    }
    output.push_str(&source[cursor..]);
    output
}

fn candidate_source_reads_by_module_with_exportable(
    program: &EnrichedProgram,
    exportable_bindings_by_module: &BTreeMap<ModuleId, BTreeSet<BindingName>>,
) -> BTreeMap<ModuleId, BTreeSet<BindingName>> {
    let mut reads = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
    for (module_id, binding) in program.model().graph().def_use().unresolved_reads() {
        reads.entry(module_id).or_default().insert(binding);
    }
    let empty_bindings = BTreeSet::<BindingName>::new();
    for module in program.model().modules() {
        let Some(source) = program.model().input().module_source_slice(module.id) else {
            continue;
        };
        let local_bindings = exportable_bindings_by_module
            .get(&module.id)
            .unwrap_or(&empty_bindings);
        reads.entry(module.id).or_default().extend(
            identifiers_in_source(source.source)
                .into_iter()
                .map(BindingName::new)
                .filter(|binding| !local_bindings.contains(binding)),
        );
    }
    reads
}

fn source_exportable_bindings(
    program: &EnrichedProgram,
    module_id: ModuleId,
) -> BTreeSet<BindingName> {
    let mut bindings = source_definition_bindings(program, module_id);
    bindings.extend(program.model().graph().ast_imports_for(module_id));
    if let Some(source) = program.model().input().module_source_slice(module_id) {
        bindings.extend(named_reexported_bindings(source.source));
    }
    bindings
}

fn source_definition_bindings(
    program: &EnrichedProgram,
    module_id: ModuleId,
) -> BTreeSet<BindingName> {
    let mut bindings = program.model().graph().ast_definitions_for(module_id);
    if let Some(source) = program.model().input().module_source_slice(module_id) {
        bindings.extend(top_level_definitions_in_source(source.source));
        bindings.extend(implicit_global_writes_in_source(source.source));
    }
    bindings
}

fn named_reexported_bindings(source: &str) -> BTreeSet<BindingName> {
    source_statements(source)
        .into_iter()
        .filter(|statement| statement.starts_with("export {") && statement.contains("} from "))
        .flat_map(|statement| named_reexport_specifiers(statement).unwrap_or_default())
        .map(|specifier| BindingName::new(specifier.exported))
        .collect()
}

fn pure_named_barrel_reexports(source: &str) -> Option<BTreeSet<BindingName>> {
    let statements = source_statements(source);
    if statements.is_empty() {
        return None;
    }
    let mut direct_reexports = BTreeSet::<BindingName>::new();
    let mut imported_locals = BTreeSet::<BindingName>::new();
    let mut local_exports = BTreeSet::<BindingName>::new();
    for statement in statements {
        if statement.starts_with("export {") && statement.contains("} from ") {
            let specifiers = named_reexport_specifiers(statement)?;
            if specifiers.is_empty() || specifiers.iter().any(|specifier| specifier.is_aliased) {
                return None;
            }
            direct_reexports.extend(
                specifiers
                    .into_iter()
                    .map(|specifier| BindingName::new(specifier.exported)),
            );
            continue;
        }
        if statement.starts_with("import ") {
            let specifiers = named_import_specifiers(statement)?;
            if specifiers.is_empty() || specifiers.iter().any(|specifier| specifier.is_aliased) {
                return None;
            }
            imported_locals.extend(
                specifiers
                    .into_iter()
                    .map(|specifier| BindingName::new(specifier.local)),
            );
            continue;
        }
        if statement.starts_with("export {") {
            let specifiers = local_named_export_specifiers(statement)?;
            if specifiers.is_empty() || specifiers.iter().any(|specifier| specifier.is_aliased) {
                return None;
            }
            local_exports.extend(
                specifiers
                    .into_iter()
                    .map(|specifier| BindingName::new(specifier.exported)),
            );
            continue;
        }
        return None;
    }
    if imported_locals != local_exports {
        return None;
    }
    direct_reexports.extend(local_exports);
    (!direct_reexports.is_empty()).then_some(direct_reexports)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NamedReexportSpecifier {
    exported: String,
    is_aliased: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NamedImportSpecifier {
    local: String,
    is_aliased: bool,
}

fn source_statements(source: &str) -> Vec<&str> {
    source
        .split(';')
        .map(str::trim)
        .filter(|statement| !statement.is_empty())
        .collect()
}

fn named_reexport_specifiers(statement: &str) -> Option<Vec<NamedReexportSpecifier>> {
    let rest = statement.strip_prefix("export {")?;
    let (inner, after) = rest.split_once('}')?;
    if !after.trim_start().starts_with("from ") {
        return None;
    }
    let mut specifiers = Vec::new();
    for raw in inner.split(',') {
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with("type ") {
            return None;
        }
        let (imported, exported, is_aliased) = raw
            .split_once(" as ")
            .map_or((raw, raw, false), |(imported, exported)| {
                (imported.trim(), exported.trim(), true)
            });
        if !is_identifier_like(imported) || !is_identifier_like(exported) {
            return None;
        }
        specifiers.push(NamedReexportSpecifier {
            exported: exported.to_string(),
            is_aliased,
        });
    }
    Some(specifiers)
}

fn local_named_export_specifiers(statement: &str) -> Option<Vec<NamedReexportSpecifier>> {
    let rest = statement.strip_prefix("export {")?;
    let (inner, after) = rest.split_once('}')?;
    if !after.trim().is_empty() {
        return None;
    }
    parse_named_export_inner(inner)
}

fn parse_named_export_inner(inner: &str) -> Option<Vec<NamedReexportSpecifier>> {
    let mut specifiers = Vec::new();
    for raw in inner.split(',') {
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with("type ") {
            return None;
        }
        let (imported, exported, is_aliased) = raw
            .split_once(" as ")
            .map_or((raw, raw, false), |(imported, exported)| {
                (imported.trim(), exported.trim(), true)
            });
        if !is_identifier_like(imported) || !is_identifier_like(exported) {
            return None;
        }
        specifiers.push(NamedReexportSpecifier {
            exported: exported.to_string(),
            is_aliased,
        });
    }
    Some(specifiers)
}

fn named_import_specifiers(statement: &str) -> Option<Vec<NamedImportSpecifier>> {
    let rest = statement.strip_prefix("import ")?.trim();
    if rest.starts_with("type ") || rest.contains(" with ") || rest.contains(" assert ") {
        return None;
    }
    let (clause, _specifier) = split_import_clause_and_specifier(rest)?;
    if !clause.starts_with('{') {
        return None;
    }
    Some(
        parse_named_import_clause(clause)?
            .into_iter()
            .map(|(imported, local)| NamedImportSpecifier {
                is_aliased: imported != local,
                local,
            })
            .collect(),
    )
}

fn extra_exports_for_module<'a>(
    _program: &EnrichedProgram,
    _module_id: ModuleId,
    sources: impl IntoIterator<Item = Option<&'a BTreeSet<BindingName>>>,
) -> BTreeSet<BindingName> {
    let mut exports = BTreeSet::new();
    for source in sources.into_iter().flatten() {
        exports.extend(source.iter().cloned());
    }
    exports
}

fn unique_source_definition_modules(
    program: &EnrichedProgram,
    externalized_packages: &BTreeSet<ModuleId>,
) -> BTreeMap<BindingName, Option<ModuleId>> {
    let definition_bindings_by_module = source_definition_bindings_by_module(program);
    unique_source_definition_modules_from_bindings(
        program,
        externalized_packages,
        &definition_bindings_by_module,
    )
}

fn source_definition_bindings_by_module(
    program: &EnrichedProgram,
) -> BTreeMap<ModuleId, BTreeSet<BindingName>> {
    program
        .model()
        .modules()
        .iter()
        .map(|module| (module.id, source_definition_bindings(program, module.id)))
        .collect()
}

fn unique_source_definition_modules_from_bindings(
    program: &EnrichedProgram,
    externalized_packages: &BTreeSet<ModuleId>,
    definition_bindings_by_module: &BTreeMap<ModuleId, BTreeSet<BindingName>>,
) -> BTreeMap<BindingName, Option<ModuleId>> {
    let mut definitions = BTreeMap::<BindingName, Option<ModuleId>>::new();
    for module in program.model().modules() {
        if module.kind == ModuleKind::Package && externalized_packages.contains(&module.id) {
            continue;
        }
        let Some(source_definitions) = definition_bindings_by_module.get(&module.id) else {
            continue;
        };
        for binding in source_definitions {
            definitions
                .entry(binding.clone())
                .and_modify(|module_id| *module_id = None)
                .or_insert(Some(module.id));
        }
    }
    definitions
}

fn module_output_path(program: &EnrichedProgram, module_id: ModuleId) -> Option<String> {
    let module = program
        .model()
        .modules()
        .iter()
        .find(|module| module.id == module_id)?;
    Some(
        program
            .semantic_names()
            .module_path(module.id)
            .unwrap_or(module.semantic_path.as_str())
            .to_string(),
    )
}

fn runtime_helper_kinds(
    graph: &RevertsGraph,
    runtime_imports: &[RuntimePreludeImport],
) -> BTreeMap<BindingName, RuntimePreludeBindingKind> {
    let mut helpers = BTreeMap::new();
    for import in runtime_imports {
        let Some(prelude) = graph.runtime_prelude(import.source_file_id) else {
            continue;
        };
        let Some(kind) = prelude.binding_kind(&import.binding) else {
            continue;
        };
        helpers.insert(import.binding.clone(), kind);
    }
    helpers
}

fn runtime_helper_kinds_for_source(
    graph: &RevertsGraph,
    source_file_id: u32,
    source: &str,
) -> BTreeMap<BindingName, RuntimePreludeBindingKind> {
    let source_identifiers = identifiers_in_source(source)
        .into_iter()
        .map(BindingName::new)
        .collect::<BTreeSet<_>>();
    graph
        .runtime_prelude(source_file_id)
        .map(|prelude| {
            prelude
                .bindings
                .iter()
                .filter(|(binding, _kind)| source_identifiers.contains(*binding))
                .map(|(binding, kind)| (binding.clone(), *kind))
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeHelperLowering {
    source: String,
    lowered_helpers: BTreeSet<BindingName>,
    remaining_helpers: BTreeSet<BindingName>,
    uses_lazy_module: bool,
    uses_lazy_value: bool,
    /// Top-level bindings whose shape changed during delazify / namespace
    /// decomposition. The IR-derived `BindingShape` from the pre-lowering
    /// solution no longer matches the emitted RHS (e.g. a binding that was
    /// `Callable` because it wrapped `lazyValue(...)` is now a plain value
    /// literal), so the planner must downgrade these bindings' shape to
    /// `Unknown` to keep the audit consistent.
    reshaped_bindings: BTreeSet<BindingName>,
}

#[cfg(test)]
fn lower_runtime_helpers(
    source: &str,
    helper_kinds: &BTreeMap<BindingName, RuntimePreludeBindingKind>,
    exported_bindings: &BTreeSet<BindingName>,
    eager_safe_call_targets: &BTreeSet<String>,
) -> RuntimeHelperLowering {
    lower_runtime_helpers_with_options(
        source,
        helper_kinds,
        exported_bindings,
        eager_safe_call_targets,
        true,
    )
}

fn lower_runtime_helpers_with_options(
    source: &str,
    helper_kinds: &BTreeMap<BindingName, RuntimePreludeBindingKind>,
    exported_bindings: &BTreeSet<BindingName>,
    eager_safe_call_targets: &BTreeSet<String>,
    allow_local_lazy_value_wrappers: bool,
) -> RuntimeHelperLowering {
    let mut lowered = source.to_string();
    let mut lowered_helpers = BTreeSet::new();
    let mut uses_lazy_module = false;
    let mut uses_lazy_value = false;
    for (helper, kind) in helper_kinds {
        let result = match kind {
            RuntimePreludeBindingKind::CommonJsWrapper => {
                lower_commonjs_wrapper_helper(lowered.as_str(), helper.as_str())
            }
            RuntimePreludeBindingKind::LazyInitializer => {
                lower_lazy_initializer_helper(lowered.as_str(), helper.as_str())
            }
            RuntimePreludeBindingKind::SourceBacked => None,
        };
        if let Some(next) = result {
            let helper_removed = !contains_call_to_identifier(next.as_str(), helper.as_str());
            lowered = next;
            if helper_removed {
                lowered_helpers.insert(helper.clone());
            }
            match kind {
                RuntimePreludeBindingKind::CommonJsWrapper => uses_lazy_module = true,
                RuntimePreludeBindingKind::LazyInitializer => uses_lazy_value = true,
                RuntimePreludeBindingKind::SourceBacked => {}
            }
        }
    }
    let remaining_helpers: BTreeSet<BindingName> = helper_kinds
        .iter()
        .filter(|(helper, kind)| match kind {
            RuntimePreludeBindingKind::CommonJsWrapper
            | RuntimePreludeBindingKind::LazyInitializer => {
                contains_call_to_identifier(lowered.as_str(), helper.as_str())
            }
            RuntimePreludeBindingKind::SourceBacked => {
                contains_identifier_reference(lowered.as_str(), helper.as_str())
            }
        })
        .map(|(helper, _kind)| helper)
        .cloned()
        .collect();
    // Final pass: collapse trivial `var X = lazyValue(() => { return EXPR; })`
    // and `var X = lazyModule((e, m) => { m.exports = EXPR; })` back to
    // `var X = EXPR;` when EXPR is pure and X is only used as an immediate
    // call `X()`. This unwinds the bundler's lazy memoization for values
    // that never needed it, so the emitted code reads like the pre-bundle
    // source instead of a lazy thunk wrap.
    let mut reshaped_bindings = BTreeSet::<BindingName>::new();
    if uses_lazy_value || uses_lazy_module {
        let (next, delazified) =
            delazify_pure_value_bindings(&lowered, exported_bindings, eager_safe_call_targets);
        lowered = next;
        reshaped_bindings.extend(delazified);
        // After delazify produced `var X = { fn1, fn2, ... };` for CommonJS
        // modules whose body was `exports.fn1 = ...; exports.fn2 = ...;`,
        // explode the namespace object back into individual top-level
        // bindings — restoring the `export function ...` shape consumers
        // would have written in ESM. Safe only for objects whose every value
        // is a function/class/arrow expression (API surfaces), and whose
        // binding is accessed exclusively via `X.<key>` member access.
        let (next, decomposed) = decompose_function_namespace_objects(&lowered, exported_bindings);
        lowered = next;
        reshaped_bindings.extend(decomposed);
        let can_inline_remaining_lazy_values = {
            let local_definitions = top_level_definitions_in_source(&lowered);
            remaining_helpers
                .iter()
                .all(|binding| local_definitions.contains(binding))
        };
        // Anything that survived the pure-value delazify pass still needs
        // lazy CommonJS semantics, but it does not need to import the shared
        // `lazyModule` helper. Inline the tiny memoizing wrapper locally so
        // thousands of recovered CJS modules stop depending on the central
        // runtime helper file just for this loader.
        let (next, _inlined_lazy_modules) = inline_remaining_lazy_module_wrappers(&lowered);
        lowered = next;
        // Same for lazyValue thunks that must remain callable (exported,
        // first-class, or impure bodies): keep memoization semantics, but make
        // the wrapper local to the declaring module when that removes the
        // only runtime-helper edge. If the module already needs runtime
        // bindings, keeping the shared helper is smaller and avoids replacing
        // one import specifier with another generated local var.
        if allow_local_lazy_value_wrappers && can_inline_remaining_lazy_values {
            let (next, _inlined_lazy_values) = inline_remaining_lazy_value_wrappers(&lowered);
            lowered = next;
        }
        // Re-derive the flags from the post-rewrite source: if every lazy
        // thunk collapsed, the helper module no longer needs the helper.
        uses_lazy_value = source_contains_top_level_call(&lowered, "lazyValue");
        uses_lazy_module = source_contains_top_level_call(&lowered, "lazyModule");
    }
    RuntimeHelperLowering {
        source: lowered,
        lowered_helpers,
        remaining_helpers,
        uses_lazy_module,
        uses_lazy_value,
        reshaped_bindings,
    }
}

fn inline_remaining_lazy_value_wrappers(source: &str) -> (String, bool) {
    inline_remaining_lazy_value_wrappers_with_options(source, false)
}

fn inline_remaining_lazy_value_wrappers_allowing_assignments(source: &str) -> (String, bool) {
    inline_remaining_lazy_value_wrappers_with_options(source, true)
}

fn inline_remaining_lazy_value_wrappers_with_options(
    source: &str,
    allow_assignment_factories: bool,
) -> (String, bool) {
    let bytes = source.as_bytes();
    let mut edits = Vec::<(usize, usize, String)>::new();
    let helper_name = local_lazy_value_helper_name(source);
    let mut cursor = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && let Some((decl, after)) =
                try_parse_lazy_value_wrapper_declaration(source, cursor, allow_assignment_factories)
        {
            edits.push((decl.callee_span.0, decl.callee_span.1, helper_name.clone()));
            cursor = after;
            continue;
        }
        match bytes[cursor] {
            b'(' => paren_depth += 1,
            b')' => paren_depth = paren_depth.saturating_sub(1),
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b'{' => brace_depth += 1,
            b'}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
        cursor += 1;
    }
    if edits.is_empty() {
        return (source.to_string(), false);
    }
    let source = apply_text_edits(source, &edits);
    (
        format!("{}\n{source}", local_lazy_value_helper_source(&helper_name)),
        true,
    )
}

#[derive(Debug, Clone)]
struct ParsedLazyValueWrapper {
    callee_span: (usize, usize),
}

fn try_parse_lazy_value_wrapper_declaration(
    source: &str,
    start: usize,
    allow_assignment_factory: bool,
) -> Option<(ParsedLazyValueWrapper, usize)> {
    let (_keyword, after_keyword) = declaration_keyword_at(source, start)?;
    let bytes = source.as_bytes();
    let mut cursor = skip_ws(bytes, start + after_keyword);
    let (_binding, after_binding) = parse_identifier(source, cursor)?;
    cursor = skip_ws(bytes, after_binding);
    if bytes.get(cursor) != Some(&b'=') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    let callee_start = cursor;
    if !keyword_at(source, cursor, "lazyValue") {
        return None;
    }
    cursor += "lazyValue".len();
    let callee_end = cursor;
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'(') {
        return None;
    }
    let call_open = cursor;
    let call_close = find_matching_paren(source, call_open)?;
    let factory = source[call_open + 1..call_close].trim();
    if factory.is_empty() {
        return None;
    }
    if !lazy_value_factory_is_zero_arg_arrow(factory) {
        return None;
    }
    // Keep assignment-heavy lazy initializers in their canonical
    // `lazyValue(...)` shape for the runtime-folding pass. Those bodies are
    // often the writer side of runtime mutable bindings; inlining them before
    // fold planning would hide the `X = ...` writes and prevent the safer
    // runtime relocation/folding machinery from seeing them.
    if !allow_assignment_factory && lazy_value_factory_contains_assignment(factory) {
        return None;
    }
    let after_call = skip_ws(bytes, call_close + 1);
    if bytes.get(after_call) != Some(&b';') {
        return None;
    }
    let stmt_end = after_call + 1;
    Some((
        ParsedLazyValueWrapper {
            callee_span: (callee_start, callee_end),
        },
        stmt_end,
    ))
}

fn lazy_value_factory_contains_assignment(factory: &str) -> bool {
    let Some(arrow) = factory.find("=>") else {
        return true;
    };
    let body = &factory[arrow + 2..];
    let bytes = body.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(body, cursor) {
            cursor = next;
            continue;
        }
        if bytes[cursor] != b'=' {
            cursor += 1;
            continue;
        }
        let prev = previous_non_ws(bytes, cursor);
        let next = skip_ws(bytes, cursor + 1);
        let prev_is_operator =
            prev.is_some_and(|idx| matches!(bytes[idx], b'=' | b'!' | b'<' | b'>'));
        let next_is_operator = matches!(bytes.get(next), Some(b'=') | Some(b'>'));
        if !prev_is_operator && !next_is_operator {
            return true;
        }
        cursor += 1;
    }
    false
}

fn lazy_value_factory_is_zero_arg_arrow(factory: &str) -> bool {
    let bytes = factory.as_bytes();
    let mut cursor = skip_ws(bytes, 0);
    if bytes.get(cursor) != Some(&b'(') {
        return false;
    }
    let Some(params_end) = find_matching_paren(factory, cursor) else {
        return false;
    };
    if !factory[cursor + 1..params_end].trim().is_empty() {
        return false;
    }
    cursor = skip_ws(bytes, params_end + 1);
    expect_arrow(bytes, cursor).is_some()
}

fn local_lazy_value_helper_name(source: &str) -> String {
    let mut name = "_$l".to_string();
    let mut suffix = 0usize;
    let local_bindings = local_bindings_in_source(source);
    while local_bindings.contains(name.as_str())
        || contains_identifier_reference(source, name.as_str())
    {
        suffix += 1;
        name = format!("_$l{suffix}");
    }
    name
}

fn local_lazy_value_helper_source(helper_name: &str) -> String {
    format!("var {helper_name}=(_$f,_$v)=>()=>(_$f&&(_$v=_$f(_$f=0)),_$v);")
}

fn inline_remaining_lazy_module_wrappers(source: &str) -> (String, bool) {
    let bytes = source.as_bytes();
    let mut edits = Vec::<(usize, usize, String)>::new();
    let mut cursor = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && let Some((decl, after)) = try_parse_lazy_module_wrapper_declaration(source, cursor)
        {
            edits.push((
                decl.span.0,
                decl.span.1,
                inline_lazy_module_wrapper_replacement(&decl),
            ));
            cursor = after;
            continue;
        }
        match bytes[cursor] {
            b'(' => paren_depth += 1,
            b')' => paren_depth = paren_depth.saturating_sub(1),
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b'{' => brace_depth += 1,
            b'}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
        cursor += 1;
    }
    if edits.is_empty() {
        return (source.to_string(), false);
    }
    (apply_text_edits(source, &edits), true)
}

#[derive(Debug, Clone)]
struct ParsedLazyModuleWrapper<'a> {
    binding: BindingName,
    factory: &'a str,
    span: (usize, usize),
}

fn try_parse_lazy_module_wrapper_declaration(
    source: &str,
    start: usize,
) -> Option<(ParsedLazyModuleWrapper<'_>, usize)> {
    let (_keyword, after_keyword) = declaration_keyword_at(source, start)?;
    let bytes = source.as_bytes();
    let mut cursor = skip_ws(bytes, start + after_keyword);
    let (binding, after_binding) = parse_identifier(source, cursor)?;
    cursor = skip_ws(bytes, after_binding);
    if bytes.get(cursor) != Some(&b'=') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    if !keyword_at(source, cursor, "lazyModule") {
        return None;
    }
    cursor += "lazyModule".len();
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'(') {
        return None;
    }
    let call_open = cursor;
    let call_close = find_matching_paren(source, call_open)?;
    let factory = source[call_open + 1..call_close].trim();
    if factory.is_empty() {
        return None;
    }
    let after_call = skip_ws(bytes, call_close + 1);
    if bytes.get(after_call) != Some(&b';') {
        return None;
    }
    let stmt_end = after_call + 1;
    Some((
        ParsedLazyModuleWrapper {
            binding: BindingName::new(binding),
            factory,
            span: (start, stmt_end),
        },
        stmt_end,
    ))
}

fn inline_lazy_module_wrapper_replacement(decl: &ParsedLazyModuleWrapper<'_>) -> String {
    let binding = decl.binding.as_str();
    format!(
        "var {binding} = (() => {{ let _$cached; return () => {{ if (_$cached) return _$cached.exports; var _$module = _$cached = {{ exports: {{}} }}; ({factory})(_$module.exports, _$module); return _$module.exports; }}; }})();",
        factory = decl.factory,
    )
}

fn lower_commonjs_wrapper_helper(source: &str, helper: &str) -> Option<String> {
    lower_helper_declarations(source, helper, HelperDeclarationKind::CommonJsWrapper)
}

fn lower_lazy_initializer_helper(source: &str, helper: &str) -> Option<String> {
    lower_helper_declarations(source, helper, HelperDeclarationKind::LazyInitializer)
}

#[derive(Debug, Clone)]
struct DelazifyCandidate {
    binding: BindingName,
    /// Byte range covering the full `var X = lazyValue(() => { ... });` statement.
    declaration_span: (usize, usize),
    /// The pure expression extracted from the lazy body's `return EXPR;`.
    value_expr: String,
    /// Spans of every `X()` call site whose `(` `)` get dropped when X is delazified.
    call_sites: Vec<(usize, usize)>,
}

/// Replace `var X = lazyValue(() => { return EXPR; });` with `var X = EXPR;`
/// — and rewrite every `X()` call site to plain `X` — whenever the binding is
/// used only as an immediate-call thunk and EXPR is a pure literal-shape
/// expression (literal, object literal, array literal, class expression, or
/// function expression). Returns the rewritten source unchanged when no
/// candidates qualify, so callers can apply the pass unconditionally.
fn delazify_pure_value_bindings(
    source: &str,
    exported_bindings: &BTreeSet<BindingName>,
    eager_safe_call_targets: &BTreeSet<String>,
) -> (String, BTreeSet<BindingName>) {
    let mut candidates = collect_delazify_candidates(source, eager_safe_call_targets);
    // Cross-module gate: any binding listed in `exported_bindings` is
    // observed by some other module that still calls `X()` against the
    // unlowered surface. Collapsing it to a value here would crash those
    // call sites. The local `collect_safe_call_sites` check only sees uses
    // within this module's bundle slice — explicit export plumbing is the
    // only signal that catches cross-module usage.
    candidates.retain(|cand| !exported_bindings.contains(&cand.binding));
    if candidates.is_empty() {
        return (source.to_string(), BTreeSet::new());
    }
    let changed = candidates.iter().map(|cand| cand.binding.clone()).collect();
    (apply_delazify_rewrites(source, &candidates), changed)
}

fn collect_delazify_candidates(
    source: &str,
    eager_safe_call_targets: &BTreeSet<String>,
) -> Vec<DelazifyCandidate> {
    let bytes = source.as_bytes();
    let mut candidates = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        let parsed = try_parse_lazy_value_declaration(source, cursor, eager_safe_call_targets)
            .or_else(|| try_parse_lazy_module_declaration(source, cursor, eager_safe_call_targets));
        let Some((declaration, after)) = parsed else {
            cursor += 1;
            continue;
        };
        // Verify the binding is referenced only as immediate `X()` calls; any
        // other reference shape (used as value, exported, passed as argument,
        // typeof, etc.) makes delazification unsafe.
        if let Some(call_sites) =
            collect_safe_call_sites(source, declaration.binding.as_str(), declaration.span)
        {
            candidates.push(DelazifyCandidate {
                binding: declaration.binding,
                declaration_span: declaration.span,
                value_expr: declaration.value_expr,
                call_sites,
            });
        }
        cursor = after;
    }
    candidates
}

struct ParsedDelazifiableDeclaration {
    binding: BindingName,
    span: (usize, usize),
    value_expr: String,
}

fn try_parse_lazy_value_declaration(
    source: &str,
    start: usize,
    eager_safe_call_targets: &BTreeSet<String>,
) -> Option<(ParsedDelazifiableDeclaration, usize)> {
    let bytes = source.as_bytes();
    let keyword = ["var", "let", "const"]
        .into_iter()
        .find(|kw| keyword_at(source, start, kw))?;
    let mut cursor = start + keyword.len();
    cursor = skip_ws(bytes, cursor);
    let (binding_name, after_binding) = parse_identifier(source, cursor)?;
    cursor = skip_ws(bytes, after_binding);
    if bytes.get(cursor) != Some(&b'=') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    let prefix = "lazyValue(";
    if !source[cursor..].starts_with(prefix) {
        return None;
    }
    cursor += prefix.len();
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'(') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    if bytes.get(cursor) != Some(&b')') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    cursor = expect_arrow(bytes, cursor)?;
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'{') {
        return None;
    }
    let body_end = find_matching_brace(source, cursor)?;
    let body = &source[cursor + 1..body_end];
    let value_expr = reverts_js::extract_lazy_module_eager_value_with_safe_deps(
        body,
        "",
        None,
        None,
        ParseGoal::TypeScript,
        eager_safe_call_targets,
    )?;
    let after_body = skip_ws(bytes, body_end + 1);
    if bytes.get(after_body) != Some(&b')') {
        return None;
    }
    let after_paren = skip_ws(bytes, after_body + 1);
    if bytes.get(after_paren) != Some(&b';') {
        return None;
    }
    let stmt_end = after_paren + 1;
    Some((
        ParsedDelazifiableDeclaration {
            binding: BindingName::new(binding_name),
            span: (start, stmt_end),
            value_expr,
        },
        stmt_end,
    ))
}

/// Match `var X = lazyModule((EXPORTS, MODULE?) => { ... });` and return the
/// pure value the binding can collapse to. Supports two body shapes:
///   1. Single statement `MODULE.exports = PURE_EXPR;` — covers a CommonJS
///      module that re-exports a single pure value (literal, object/array,
///      class, function).
///   2. Series of `EXPORTS.k = PURE_EXPR_k;` statements — translated to an
///      inline object literal `{ k1: expr1, k2: expr2, ... }`. Covers the
///      `module.exports.foo = ...; module.exports.bar = ...;` style of
///      CommonJS multi-key exports.
///
/// Any other body shape (mixed assignments, control flow, `require` calls,
/// helper declarations) leaves the lazy module intact for a later, more
/// invasive pass to extract to a sibling file.
fn try_parse_lazy_module_declaration(
    source: &str,
    start: usize,
    eager_safe_call_targets: &BTreeSet<String>,
) -> Option<(ParsedDelazifiableDeclaration, usize)> {
    let bytes = source.as_bytes();
    let keyword = ["var", "let", "const"]
        .into_iter()
        .find(|kw| keyword_at(source, start, kw))?;
    let mut cursor = start + keyword.len();
    cursor = skip_ws(bytes, cursor);
    let (binding_name, after_binding) = parse_identifier(source, cursor)?;
    cursor = skip_ws(bytes, after_binding);
    if bytes.get(cursor) != Some(&b'=') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    let prefix = "lazyModule(";
    if !source[cursor..].starts_with(prefix) {
        return None;
    }
    cursor += prefix.len();
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'(') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    let (exports_param, after_first) = parse_identifier(source, cursor)?;
    cursor = skip_ws(bytes, after_first);
    let module_param = if bytes.get(cursor) == Some(&b',') {
        cursor = skip_ws(bytes, cursor + 1);
        let (name, after) = parse_identifier(source, cursor)?;
        cursor = skip_ws(bytes, after);
        Some(name)
    } else {
        None
    };
    if bytes.get(cursor) != Some(&b')') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    cursor = expect_arrow(bytes, cursor)?;
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'{') {
        return None;
    }
    let body_end = find_matching_brace(source, cursor)?;
    let body = &source[cursor + 1..body_end];
    let value_expr = reverts_js::extract_lazy_module_eager_value_with_safe_deps(
        body,
        exports_param,
        module_param,
        None,
        ParseGoal::TypeScript,
        eager_safe_call_targets,
    )?;
    let after_body = skip_ws(bytes, body_end + 1);
    if bytes.get(after_body) != Some(&b')') {
        return None;
    }
    let after_paren = skip_ws(bytes, after_body + 1);
    if bytes.get(after_paren) != Some(&b';') {
        return None;
    }
    let stmt_end = after_paren + 1;
    Some((
        ParsedDelazifiableDeclaration {
            binding: BindingName::new(binding_name),
            span: (start, stmt_end),
            value_expr,
        },
        stmt_end,
    ))
}

/// Walk `source` looking for every identifier reference that equals `binding`,
/// excluding the byte range `exclude_span` (which covers the declaration we are
/// considering rewriting). Each reference must be an immediate-call thunk
/// invocation `X()` — anything else (used as a value, exported, passed as an
/// argument, captured in a closure, etc.) is unsafe to rewrite. Returns
/// `Some(call_sites)` when every reference qualifies; otherwise `None`.
fn collect_safe_call_sites(
    source: &str,
    binding: &str,
    exclude_span: (usize, usize),
) -> Option<Vec<(usize, usize)>> {
    let bytes = source.as_bytes();
    let mut call_sites = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        if !is_identifier_start(bytes[cursor]) {
            cursor += 1;
            continue;
        }
        let start = cursor;
        cursor += 1;
        while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
            cursor += 1;
        }
        let ident = &source[start..cursor];
        if ident != binding {
            continue;
        }
        // Skip occurrences inside the declaration being rewritten.
        if start >= exclude_span.0 && cursor <= exclude_span.1 {
            continue;
        }
        // Property access `obj.X` / `obj#X` — `X` is a key, not a reference to
        // our binding. Safe to ignore.
        if let Some(prev) = previous_non_ws(bytes, start)
            && matches!(bytes[prev], b'.' | b'#')
        {
            continue;
        }
        // Any other context where `X` is not a value reference (object property
        // key, parameter declaration, assignment target on lhs) makes
        // delazification unsafe.
        if !identifier_occurrence_is_value_reference(source, start, cursor) {
            return None;
        }
        // The reference must be immediately followed by `()` and nothing else
        // that captures `X` as a function (e.g., `X.bind`, `X(arg)` is also
        // unsafe — the lazy thunk takes no args).
        let after = skip_ws(bytes, cursor);
        if bytes.get(after) != Some(&b'(') {
            return None;
        }
        let inner = skip_ws(bytes, after + 1);
        if bytes.get(inner) != Some(&b')') {
            return None;
        }
        call_sites.push((start, inner + 1));
        cursor = inner + 1;
    }
    Some(call_sites)
}

fn apply_delazify_rewrites(source: &str, candidates: &[DelazifyCandidate]) -> String {
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for candidate in candidates {
        edits.push((
            candidate.declaration_span.0,
            candidate.declaration_span.1,
            format!(
                "var {} = {};",
                candidate.binding.as_str(),
                candidate.value_expr
            ),
        ));
        for (start, end) in &candidate.call_sites {
            edits.push((*start, *end, candidate.binding.as_str().to_string()));
        }
    }
    edits.sort_by_key(|(start, _, _)| *start);
    let mut output = String::with_capacity(source.len());
    let mut cursor = 0;
    for (start, end, replacement) in &edits {
        debug_assert!(*start >= cursor, "delazify edits must be non-overlapping");
        output.push_str(&source[cursor..*start]);
        output.push_str(replacement);
        cursor = *end;
    }
    output.push_str(&source[cursor..]);
    output
}

/// Skip any non-code prefix at `cursor`: quoted strings, template literals,
/// line/block comments, or regex literals. Returns the byte position just past
/// the non-code span when something was skipped, or `None` when the current
/// byte begins a code token.
fn skip_non_code_at(source: &str, cursor: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let byte = *bytes.get(cursor)?;
    match byte {
        b'\'' | b'"' => Some(skip_quoted(bytes, cursor, byte)),
        b'`' => Some(skip_template_literal(bytes, cursor)),
        b'/' if bytes.get(cursor + 1) == Some(&b'/') => Some(skip_line_comment(bytes, cursor + 2)),
        b'/' if bytes.get(cursor + 1) == Some(&b'*') => Some(skip_block_comment(bytes, cursor + 2)),
        b'/' if looks_like_regex_literal(bytes, cursor) => Some(skip_regex_literal(bytes, cursor)),
        _ => None,
    }
}

#[derive(Debug, Clone)]
struct DecomposeCandidate {
    /// Byte range covering the full `var X = { ... };` declaration.
    declaration_span: (usize, usize),
    /// Object properties, in their source order. Every value is guaranteed
    /// to be a function expression, arrow expression, or class expression.
    properties: Vec<(String, String)>,
    /// Byte spans of every `X.<key>` access site, with the key that's
    /// being accessed. The full span (from the start of `X` to the end of
    /// the key identifier) gets replaced by the bare key.
    access_sites: Vec<(usize, usize, String)>,
}

/// Decompose `var X = { K1: FN1, K2: FN2, ... };` (where every value is a
/// function/class/arrow expression and X is accessed only as `X.<Ki>`) into
/// individual top-level bindings `var K1 = FN1; var K2 = FN2; ...` and
/// rewrite each `X.Ki` to bare `Ki`. This restores the ESM `export function`
/// shape that a CommonJS module of the form `exports.foo = fn; exports.bar = fn;`
/// would have been written as in idiomatic TypeScript.
///
/// Restricted to value bindings whose entries are *all* function-shape (so the
/// object is really an API namespace rather than a data record); primitive- or
/// mixed-valued objects keep their grouped form because the grouping is the
/// pre-bundle programmer's intent.
fn decompose_function_namespace_objects(
    source: &str,
    exported_bindings: &BTreeSet<BindingName>,
) -> (String, BTreeSet<BindingName>) {
    let mut raw = scan_function_namespace_declarations(source);
    // Cross-module gate (same reasoning as delazify): if the namespace object
    // binding is exported, consumers in other modules still address it as
    // `X.foo` against the pre-lowering namespace shape. Decomposing here
    // would orphan those consumers.
    raw.retain(|cand| !exported_bindings.contains(&cand.binding));
    if raw.is_empty() {
        return (source.to_string(), BTreeSet::new());
    }
    let existing_top_level = top_level_definitions_in_source(source);
    let candidates = filter_decompose_candidates(source, raw, &existing_top_level);
    if candidates.is_empty() {
        return (source.to_string(), BTreeSet::new());
    }
    let mut changed = BTreeSet::<BindingName>::new();
    for cand in &candidates {
        // The namespace object's binding is being removed entirely; its shape
        // changes from "Callable thunk" (IR view) to "doesn't exist".
        // Downstream callers treat the binding as reshaped to clear its
        // IR-inferred shape.
        for (key, _) in &cand.properties {
            changed.insert(BindingName::new(key));
        }
    }
    (apply_decompose_rewrites(source, &candidates), changed)
}

#[derive(Debug, Clone)]
struct RawDecomposeCandidate {
    binding: BindingName,
    declaration_span: (usize, usize),
    properties: Vec<(String, String)>,
}

/// Walk the source at module top level (depth-tracked so we don't pick up
/// `var X = { ... }` declarations nested inside functions or blocks) and
/// collect every `var X = { ... };` whose object literal has only
/// function-shape values.
fn scan_function_namespace_declarations(source: &str) -> Vec<RawDecomposeCandidate> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut cursor = 0;
    let mut brace_depth: i32 = 0;
    let mut paren_depth: i32 = 0;
    let mut bracket_depth: i32 = 0;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        match bytes[cursor] {
            b'{' => {
                brace_depth += 1;
                cursor += 1;
                continue;
            }
            b'}' => {
                brace_depth -= 1;
                cursor += 1;
                continue;
            }
            b'(' => {
                paren_depth += 1;
                cursor += 1;
                continue;
            }
            b')' => {
                paren_depth -= 1;
                cursor += 1;
                continue;
            }
            b'[' => {
                bracket_depth += 1;
                cursor += 1;
                continue;
            }
            b']' => {
                bracket_depth -= 1;
                cursor += 1;
                continue;
            }
            _ => {}
        }
        if brace_depth > 0 || paren_depth > 0 || bracket_depth > 0 {
            cursor += 1;
            continue;
        }
        if let Some((cand, after)) = try_parse_function_namespace_declaration(source, cursor) {
            out.push(cand);
            cursor = after;
        } else {
            cursor += 1;
        }
    }
    out
}

fn try_parse_function_namespace_declaration(
    source: &str,
    start: usize,
) -> Option<(RawDecomposeCandidate, usize)> {
    let bytes = source.as_bytes();
    let keyword = ["var", "let", "const"]
        .into_iter()
        .find(|kw| keyword_at(source, start, kw))?;
    let mut cursor = start + keyword.len();
    cursor = skip_ws(bytes, cursor);
    let (binding_name, after_binding) = parse_identifier(source, cursor)?;
    cursor = skip_ws(bytes, after_binding);
    if bytes.get(cursor) != Some(&b'=') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    if bytes.get(cursor) != Some(&b'{') {
        return None;
    }
    let object_open = cursor;
    let object_close = find_matching_brace(source, object_open)?;
    let inner = &source[object_open + 1..object_close];
    let after_object = skip_ws(bytes, object_close + 1);
    if bytes.get(after_object) != Some(&b';') {
        return None;
    }
    let stmt_end = after_object + 1;

    let properties = parse_function_namespace_properties(inner)?;
    Some((
        RawDecomposeCandidate {
            binding: BindingName::new(binding_name),
            declaration_span: (start, stmt_end),
            properties,
        },
        stmt_end,
    ))
}

/// Parse the inside of an object literal into a list of `(key, value)` pairs
/// — but only succeed when every entry is a `KEY: FUNCTION_SHAPE` pair with a
/// bare identifier key. Rejects:
///   * computed keys (`['x']: ...`),
///   * shorthand (`{ x }`),
///   * spread elements,
///   * primitive or other non-function values,
///   * any value that is not a function/class/arrow expression.
fn parse_function_namespace_properties(inner: &str) -> Option<Vec<(String, String)>> {
    let entries = split_top_level_commas(inner);
    let mut properties = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in entries {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let entry_bytes = entry.as_bytes();
        if !is_identifier_start(entry_bytes[0]) {
            return None;
        }
        let mut key_end = 1;
        while key_end < entry_bytes.len() && is_identifier_continue(entry_bytes[key_end]) {
            key_end += 1;
        }
        let key = &entry[..key_end];
        if !seen.insert(key.to_string()) {
            return None;
        }
        let after_key = entry[key_end..].trim_start();
        let after_colon = after_key.strip_prefix(':')?;
        let value = after_colon.trim();
        if value.is_empty() || !is_function_or_class_value(value) {
            return None;
        }
        properties.push((key.to_string(), value.to_string()));
    }
    if properties.is_empty() {
        return None;
    }
    Some(properties)
}

/// True iff `source` (after trimming) is a function expression, class
/// expression, or arrow function expression. Used to detect "API surface"
/// values for the namespace decomposition pass — only those decompose.
fn is_function_or_class_value(source: &str) -> bool {
    let trimmed = source.trim();
    if keyword_at(trimmed, 0, "class") {
        return pure_class_expression(trimmed);
    }
    if keyword_at(trimmed, 0, "function") {
        return true;
    }
    looks_like_arrow_function_expression(trimmed)
}

/// Split `source` on top-level `,` separators (those not nested inside
/// `(...)`, `[...]`, `{...}`, strings, templates, regex, or comments).
fn split_top_level_commas(source: &str) -> Vec<&str> {
    let bytes = source.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut cursor = 0;
    let mut paren: i32 = 0;
    let mut bracket: i32 = 0;
    let mut brace: i32 = 0;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        match bytes[cursor] {
            b'(' => {
                paren += 1;
                cursor += 1;
            }
            b')' => {
                paren -= 1;
                cursor += 1;
            }
            b'[' => {
                bracket += 1;
                cursor += 1;
            }
            b']' => {
                bracket -= 1;
                cursor += 1;
            }
            b'{' => {
                brace += 1;
                cursor += 1;
            }
            b'}' => {
                brace -= 1;
                cursor += 1;
            }
            b',' if paren == 0 && bracket == 0 && brace == 0 => {
                parts.push(&source[start..cursor]);
                cursor += 1;
                start = cursor;
            }
            _ => cursor += 1,
        }
    }
    parts.push(&source[start..]);
    parts
}

/// Filter raw candidates by the two cross-cutting safety checks:
///   * X must be referenced only as `X.<known_key>` (no other usage shape).
///   * None of the property keys may collide with an existing top-level
///     binding (other than the X about to be removed) or with a key already
///     introduced by a prior accepted candidate.
fn filter_decompose_candidates(
    source: &str,
    raw: Vec<RawDecomposeCandidate>,
    existing_top_level: &BTreeSet<BindingName>,
) -> Vec<DecomposeCandidate> {
    let mut introduced = BTreeSet::<BindingName>::new();
    let mut accepted = Vec::new();
    for cand in raw {
        let mut would_introduce = Vec::new();
        let mut safe = true;
        for (key, _) in &cand.properties {
            let key_binding = BindingName::new(key);
            // The binding X disappears after rewrite, so a key matching X is
            // harmless on its own — but skip it to keep things simple.
            if key_binding == cand.binding {
                continue;
            }
            if existing_top_level.contains(&key_binding) || introduced.contains(&key_binding) {
                safe = false;
                break;
            }
            would_introduce.push(key_binding);
        }
        if !safe {
            continue;
        }
        let Some(access_sites) = collect_member_access_only(
            source,
            cand.binding.as_str(),
            cand.declaration_span,
            &cand.properties,
        ) else {
            continue;
        };
        // Refuse no-op decompositions: if X is declared but never accessed,
        // dropping the declaration would discard the function/class
        // definitions entirely (they might still be observable through
        // side effects if the binding is exported, but the export check
        // happens via `collect_member_access_only` returning None for any
        // non-`X.<key>` reference, including export specifiers).
        if access_sites.is_empty() {
            continue;
        }
        introduced.extend(would_introduce);
        accepted.push(DecomposeCandidate {
            declaration_span: cand.declaration_span,
            properties: cand.properties,
            access_sites,
        });
    }
    accepted
}

/// Walk `source` and verify every reference to `binding` (outside the
/// candidate's own declaration span) is a `binding.<key>` member access whose
/// key is one of `properties`' keys. Returns the list of access-site spans
/// when every reference qualifies, or `None` when any reference is unsafe
/// (export specifier, value passed as argument, used as `typeof`, accessed
/// with a different key, etc.).
fn collect_member_access_only(
    source: &str,
    binding: &str,
    exclude_span: (usize, usize),
    properties: &[(String, String)],
) -> Option<Vec<(usize, usize, String)>> {
    let valid_keys: BTreeSet<&str> = properties.iter().map(|(k, _)| k.as_str()).collect();
    let bytes = source.as_bytes();
    let mut sites = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        if !is_identifier_start(bytes[cursor]) {
            cursor += 1;
            continue;
        }
        let start = cursor;
        cursor += 1;
        while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
            cursor += 1;
        }
        let ident = &source[start..cursor];
        if ident != binding {
            continue;
        }
        if start >= exclude_span.0 && cursor <= exclude_span.1 {
            continue;
        }
        // Property access `obj.X` / `obj#X` — X here is a property key, not
        // our binding. Skip.
        if let Some(prev) = previous_non_ws(bytes, start)
            && matches!(bytes[prev], b'.' | b'#')
        {
            continue;
        }
        if !identifier_occurrence_is_value_reference(source, start, cursor) {
            return None;
        }
        let after = skip_ws(bytes, cursor);
        if bytes.get(after) != Some(&b'.') {
            return None;
        }
        let key_start = skip_ws(bytes, after + 1);
        if key_start >= bytes.len() || !is_identifier_start(bytes[key_start]) {
            return None;
        }
        let mut key_end = key_start + 1;
        while key_end < bytes.len() && is_identifier_continue(bytes[key_end]) {
            key_end += 1;
        }
        let accessed_key = &source[key_start..key_end];
        if !valid_keys.contains(accessed_key) {
            return None;
        }
        sites.push((start, key_end, accessed_key.to_string()));
        cursor = key_end;
    }
    Some(sites)
}

fn apply_decompose_rewrites(source: &str, candidates: &[DecomposeCandidate]) -> String {
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for candidate in candidates {
        let mut new_decl = String::new();
        for (i, (key, value)) in candidate.properties.iter().enumerate() {
            if i > 0 {
                new_decl.push('\n');
            }
            new_decl.push_str(&format!("var {key} = {value};"));
        }
        edits.push((
            candidate.declaration_span.0,
            candidate.declaration_span.1,
            new_decl,
        ));
        for (start, end, key) in &candidate.access_sites {
            edits.push((*start, *end, key.clone()));
        }
    }
    edits.sort_by_key(|(start, _, _)| *start);
    let mut output = String::with_capacity(source.len());
    let mut cursor = 0;
    for (start, end, replacement) in &edits {
        debug_assert!(*start >= cursor, "decompose edits must not overlap");
        output.push_str(&source[cursor..*start]);
        output.push_str(replacement);
        cursor = *end;
    }
    output.push_str(&source[cursor..]);
    output
}

/// Whether `source` contains an immediate-call reference `<name>(...)` that
/// isn't inside a string, comment, template, or regex literal. Used to
/// determine whether the helper module still needs to export a runtime helper
/// after the delazify pass has had a chance to remove lazy thunks.
fn source_contains_top_level_call(source: &str, name: &str) -> bool {
    let bytes = source.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        if !is_identifier_start(bytes[cursor]) {
            cursor += 1;
            continue;
        }
        let start = cursor;
        cursor += 1;
        while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
            cursor += 1;
        }
        if &source[start..cursor] != name {
            continue;
        }
        let after = skip_ws(bytes, cursor);
        if bytes.get(after) == Some(&b'(') {
            return true;
        }
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelperDeclarationKind {
    CommonJsWrapper,
    LazyInitializer,
}

fn lower_helper_declarations(
    source: &str,
    helper: &str,
    kind: HelperDeclarationKind,
) -> Option<String> {
    let mut output = String::new();
    let mut index = 0;
    let mut changed = false;
    while let Some(declaration) = find_helper_declaration(source, index, helper, kind) {
        output.push_str(&source[index..declaration.start]);
        output.push_str(declaration.replacement.as_str());
        index = declaration.end;
        changed = true;
    }
    if changed {
        output.push_str(&source[index..]);
        Some(output)
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HelperDeclaration {
    start: usize,
    end: usize,
    replacement: String,
}

fn find_helper_declaration(
    source: &str,
    from: usize,
    helper: &str,
    kind: HelperDeclarationKind,
) -> Option<HelperDeclaration> {
    let bytes = source.as_bytes();
    let mut index = from;
    while index < bytes.len() {
        let (start, keyword) = find_declaration_keyword(source, index)?;
        let mut cursor = start + keyword.len();
        cursor = skip_ws(bytes, cursor);
        let Some((binding, after_binding)) = parse_identifier(source, cursor) else {
            index = start + keyword.len();
            continue;
        };
        cursor = skip_ws(bytes, after_binding);
        if bytes.get(cursor) != Some(&b'=') {
            index = start + keyword.len();
            continue;
        }
        cursor = skip_ws(bytes, cursor + 1);
        let Some((callee, after_callee)) = parse_identifier(source, cursor) else {
            index = cursor.saturating_add(1);
            continue;
        };
        if callee != helper {
            index = after_callee;
            continue;
        }
        cursor = skip_ws(bytes, after_callee);
        if bytes.get(cursor) != Some(&b'(') {
            index = after_callee;
            continue;
        }
        let parsed = match kind {
            HelperDeclarationKind::CommonJsWrapper => {
                parse_commonjs_wrapper_replacement(source, cursor + 1, binding)
            }
            HelperDeclarationKind::LazyInitializer => {
                parse_lazy_initializer_replacement(source, cursor + 1, binding)
            }
        };
        let Some((replacement, end)) = parsed else {
            index = after_callee;
            continue;
        };
        return Some(HelperDeclaration {
            start,
            end,
            replacement,
        });
    }
    None
}

fn parse_commonjs_wrapper_replacement(
    source: &str,
    mut cursor: usize,
    binding: &str,
) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'(') {
        return None;
    }
    let params_end = find_byte(bytes, cursor + 1, b')')?;
    let params = source[cursor + 1..params_end]
        .split(',')
        .map(str::trim)
        .filter(|param| !param.is_empty())
        .collect::<Vec<_>>();
    if params.is_empty() {
        return None;
    }
    cursor = skip_ws(bytes, params_end + 1);
    cursor = expect_arrow(bytes, cursor)?;
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'{') {
        return None;
    }
    let body_end = find_matching_brace(source, cursor)?;
    let body = source[cursor + 1..body_end].trim();
    let end = parse_helper_call_end(bytes, body_end + 1)?;
    let exports = params[0];
    let module_alias = params.get(1).copied();
    let parameter_list = match module_alias {
        Some(module) => format!("({exports}, {module})"),
        None => format!("({exports})"),
    };
    Some((
        format!("var {binding} = lazyModule({parameter_list} => {{\n{body}\n}});"),
        end,
    ))
}

fn parse_lazy_initializer_replacement(
    source: &str,
    mut cursor: usize,
    binding: &str,
) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'(') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    if bytes.get(cursor) != Some(&b')') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    cursor = expect_arrow(bytes, cursor)?;
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'{') {
        return None;
    }
    let body_end = find_matching_brace(source, cursor)?;
    let body = source[cursor + 1..body_end].trim();
    let end = parse_helper_call_end(bytes, body_end + 1)?;
    Some((
        format!("var {binding} = lazyValue(() => {{\n{body}\n}});"),
        end,
    ))
}

fn parse_helper_call_end(bytes: &[u8], mut cursor: usize) -> Option<usize> {
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b')') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    if bytes.get(cursor) == Some(&b';') {
        cursor += 1;
    }
    Some(cursor)
}

fn find_declaration_keyword(source: &str, from: usize) -> Option<(usize, &'static str)> {
    ["var", "let", "const"]
        .into_iter()
        .filter_map(|keyword| find_keyword(source, keyword, from).map(|index| (index, keyword)))
        .min_by_key(|(index, _)| *index)
}

fn find_keyword(source: &str, keyword: &str, from: usize) -> Option<usize> {
    let mut offset = from;
    while let Some(relative) = source[offset..].find(keyword) {
        let absolute = offset + relative;
        let before = absolute
            .checked_sub(1)
            .and_then(|index| source.as_bytes().get(index))
            .copied();
        let after = source.as_bytes().get(absolute + keyword.len()).copied();
        if before.is_none_or(|byte| !is_identifier_continue(byte))
            && after.is_none_or(|byte| !is_identifier_continue(byte))
        {
            return Some(absolute);
        }
        offset = absolute + keyword.len();
    }
    None
}

fn parse_identifier(source: &str, start: usize) -> Option<(&str, usize)> {
    let bytes = source.as_bytes();
    let first = *bytes.get(start)?;
    if !is_identifier_start(first) {
        return None;
    }
    let mut end = start + 1;
    while end < bytes.len() && is_identifier_continue(bytes[end]) {
        end += 1;
    }
    Some((&source[start..end], end))
}

fn expect_arrow(bytes: &[u8], cursor: usize) -> Option<usize> {
    (bytes.get(cursor) == Some(&b'=') && bytes.get(cursor + 1) == Some(&b'>')).then_some(cursor + 2)
}

fn find_byte(bytes: &[u8], mut cursor: usize, target: u8) -> Option<usize> {
    while cursor < bytes.len() {
        if bytes[cursor] == target {
            return Some(cursor);
        }
        cursor += 1;
    }
    None
}

fn find_matching_brace(source: &str, open: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut cursor = open;
    let mut depth = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            b'{' => {
                depth += 1;
                cursor += 1;
            }
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(cursor);
                }
                cursor += 1;
            }
            _ => cursor += 1,
        }
    }
    None
}

fn find_matching_bracket(source: &str, open: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut cursor = open;
    let mut depth = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            b'[' => {
                depth += 1;
                cursor += 1;
            }
            b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(cursor);
                }
                cursor += 1;
            }
            _ => cursor += 1,
        }
    }
    None
}

fn find_matching_paren(source: &str, open: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut cursor = open;
    let mut depth = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            b'(' => {
                depth += 1;
                cursor += 1;
            }
            b')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(cursor);
                }
                cursor += 1;
            }
            _ => cursor += 1,
        }
    }
    None
}

fn identifiers_in_source(source: &str) -> BTreeSet<String> {
    value_identifiers_in_source(source)
}

fn top_level_definitions_in_source(source: &str) -> BTreeSet<BindingName> {
    let mut definitions = BTreeSet::new();
    let bytes = source.as_bytes();
    let mut cursor = 0;
    let mut depth = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            b'{' => {
                depth += 1;
                cursor += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                cursor += 1;
            }
            _ if depth == 0 && keyword_at(source, cursor, "function") => {
                if let Some((binding, next)) =
                    parse_identifier_after_keyword(source, cursor, "function")
                {
                    definitions.insert(BindingName::new(binding));
                    cursor = next;
                } else {
                    cursor += "function".len();
                }
            }
            _ if depth == 0 && keyword_at(source, cursor, "class") => {
                if let Some((binding, next)) =
                    parse_identifier_after_keyword(source, cursor, "class")
                {
                    definitions.insert(BindingName::new(binding));
                    cursor = next;
                } else {
                    cursor += "class".len();
                }
            }
            _ if depth == 0 && keyword_at(source, cursor, "var") => {
                cursor = collect_variable_declaration_definitions(
                    source,
                    cursor + "var".len(),
                    &mut definitions,
                );
            }
            _ if depth == 0 && keyword_at(source, cursor, "let") => {
                cursor = collect_variable_declaration_definitions(
                    source,
                    cursor + "let".len(),
                    &mut definitions,
                );
            }
            _ if depth == 0 && keyword_at(source, cursor, "const") => {
                cursor = collect_variable_declaration_definitions(
                    source,
                    cursor + "const".len(),
                    &mut definitions,
                );
            }
            _ => cursor += 1,
        }
    }
    definitions
}

fn implicit_global_declarations_for_module(
    source: &str,
    source_definitions: &BTreeSet<BindingName>,
    source_imports: &BTreeSet<BindingName>,
    planned_bindings: &BTreeSet<BindingName>,
) -> BTreeSet<BindingName> {
    let top_level_definitions = top_level_definitions_in_source(source);
    implicit_global_writes_in_source(source)
        .into_iter()
        .filter(|binding| !top_level_definitions.contains(binding))
        .filter(|binding| !source_definitions.contains(binding))
        .filter(|binding| !source_imports.contains(binding))
        .filter(|binding| !planned_bindings.contains(binding))
        .filter(|binding| !is_planner_synthetic_binding(binding.as_str()))
        .collect()
}

/// Identifies binding names that the planner emits as synthetic scaffolding
/// (lazy wrap temporaries, cross-module setters, createRequire alias). Such
/// names must never be treated as user-defined bindings during import or
/// implicit-write analysis. `__reverts_*` covers module-scope synthetics
/// (setters, createRequire alias); `_$*` covers closure-local temporaries
/// inside lazy wraps and update/destructure lowerings.
fn is_planner_synthetic_binding(name: &str) -> bool {
    name.starts_with("__reverts_") || name.starts_with("_$")
}

fn implicit_global_writes_in_source(source: &str) -> BTreeSet<BindingName> {
    let mut writes = BTreeSet::new();
    let declaration_bindings = variable_declaration_binding_starts(source);
    let class_field_bindings = class_field_bindings_in_source(source);
    let bytes = source.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            b'+' | b'-' if update_operator_at(bytes, cursor).is_some() => {
                let target_start = skip_ws(bytes, cursor + 2);
                if let Some((identifier, target_end)) = parse_identifier(source, target_start)
                    && is_simple_update_target(source, target_start, target_end)
                {
                    writes.insert(BindingName::new(identifier));
                }
                cursor += 1;
            }
            b'{' => {
                if let Some((end, bindings)) =
                    object_destructuring_assignment_writes(source, cursor)
                {
                    writes.extend(bindings);
                    cursor = end;
                } else {
                    cursor += 1;
                }
            }
            b'[' => {
                if let Some((end, bindings)) = array_destructuring_assignment_writes(source, cursor)
                {
                    writes.extend(bindings);
                    cursor = end;
                } else {
                    cursor += 1;
                }
            }
            byte if is_identifier_start(byte) => {
                let start = cursor;
                cursor += 1;
                while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
                    cursor += 1;
                }
                if declaration_bindings.contains(&start) {
                    continue;
                }
                let identifier = &source[start..cursor];
                if !is_js_keyword(identifier)
                    && start
                        .checked_sub(1)
                        .and_then(|index| bytes.get(index))
                        .is_none_or(|byte| !matches!(*byte, b'.' | b'#'))
                    && !class_field_bindings.contains_key(&start)
                {
                    let after = skip_ws(bytes, cursor);
                    if (bytes.get(after) == Some(&b'=')
                        && bytes.get(after + 1) != Some(&b'=')
                        && bytes.get(after + 1) != Some(&b'>'))
                        || update_operator_at(bytes, after).is_some()
                    {
                        writes.insert(BindingName::new(identifier));
                    }
                }
            }
            _ => cursor += 1,
        }
    }
    writes
}

fn parse_identifier_after_keyword<'a>(
    source: &'a str,
    cursor: usize,
    keyword: &str,
) -> Option<(&'a str, usize)> {
    parse_identifier(source, skip_ws(source.as_bytes(), cursor + keyword.len()))
}

fn keyword_at(source: &str, cursor: usize, keyword: &str) -> bool {
    source
        .get(cursor..)
        .is_some_and(|tail| tail.starts_with(keyword))
        && cursor
            .checked_sub(1)
            .and_then(|index| source.as_bytes().get(index))
            .is_none_or(|byte| !is_identifier_continue(*byte))
        && source
            .as_bytes()
            .get(cursor + keyword.len())
            .is_none_or(|byte| !is_identifier_continue(*byte))
}

fn collect_variable_declaration_definitions(
    source: &str,
    mut cursor: usize,
    definitions: &mut BTreeSet<BindingName>,
) -> usize {
    let bytes = source.as_bytes();
    cursor = skip_ws(bytes, cursor);
    if let Some((binding, next)) = parse_identifier(source, cursor) {
        definitions.insert(BindingName::new(binding));
        cursor = next;
    }

    let mut nested = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            b'(' | b'[' | b'{' => {
                nested += 1;
                cursor += 1;
            }
            b')' | b']' | b'}' => {
                nested = nested.saturating_sub(1);
                cursor += 1;
            }
            b',' if nested == 0 => {
                cursor = skip_ws(bytes, cursor + 1);
                if let Some((binding, next)) = parse_identifier(source, cursor) {
                    definitions.insert(BindingName::new(binding));
                    cursor = next;
                }
            }
            b';' if nested == 0 => return cursor + 1,
            _ => cursor += 1,
        }
    }
    cursor
}

fn variable_declaration_binding_starts(source: &str) -> BTreeSet<usize> {
    let mut starts = BTreeSet::new();
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            _ if keyword_at(source, cursor, "var") => {
                cursor = collect_variable_declaration_binding_starts(
                    source,
                    cursor + "var".len(),
                    &mut starts,
                );
            }
            _ if keyword_at(source, cursor, "let") => {
                cursor = collect_variable_declaration_binding_starts(
                    source,
                    cursor + "let".len(),
                    &mut starts,
                );
            }
            _ if keyword_at(source, cursor, "const") => {
                cursor = collect_variable_declaration_binding_starts(
                    source,
                    cursor + "const".len(),
                    &mut starts,
                );
            }
            _ => cursor += 1,
        }
    }
    starts
}

fn collect_variable_declaration_binding_starts(
    source: &str,
    mut cursor: usize,
    starts: &mut BTreeSet<usize>,
) -> usize {
    let bytes = source.as_bytes();
    cursor = skip_ws(bytes, cursor);
    if parse_identifier(source, cursor).is_some() {
        starts.insert(cursor);
    }

    let mut nested = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            b'(' | b'[' | b'{' => {
                nested += 1;
                cursor += 1;
            }
            b')' if nested == 0 => return cursor + 1,
            b')' | b']' | b'}' => {
                nested = nested.saturating_sub(1);
                cursor += 1;
            }
            b',' if nested == 0 => {
                cursor = skip_ws(bytes, cursor + 1);
                if parse_identifier(source, cursor).is_some() {
                    starts.insert(cursor);
                }
            }
            b';' if nested == 0 => return cursor + 1,
            _ => cursor += 1,
        }
    }
    cursor
}

fn object_destructuring_assignment_writes(
    source: &str,
    object_start: usize,
) -> Option<(usize, Vec<BindingName>)> {
    let object_end = find_matching_brace(source, object_start)?;
    let equals = skip_ws(source.as_bytes(), object_end + 1);
    if source.as_bytes().get(equals) != Some(&b'=') {
        return None;
    }
    let rhs_start = skip_ws(source.as_bytes(), equals + 1);
    let rhs_end = find_assignment_rhs_end(source, rhs_start);
    let bindings = parse_object_pattern_bindings(&source[object_start + 1..object_end])
        .into_iter()
        .map(|(_, binding)| binding)
        .collect::<Vec<_>>();
    (!bindings.is_empty()).then_some((rhs_end, bindings))
}

fn array_destructuring_assignment_writes(
    source: &str,
    array_start: usize,
) -> Option<(usize, Vec<BindingName>)> {
    if bracket_starts_member_access(source, array_start) {
        return None;
    }
    let array_end = find_matching_bracket(source, array_start)?;
    let equals = skip_ws(source.as_bytes(), array_end + 1);
    if source.as_bytes().get(equals) != Some(&b'=')
        || source.as_bytes().get(equals + 1) == Some(&b'=')
        || source.as_bytes().get(equals + 1) == Some(&b'>')
    {
        return None;
    }
    let rhs_start = skip_ws(source.as_bytes(), equals + 1);
    let rhs_end = find_assignment_rhs_end(source, rhs_start);
    let bindings = parse_array_pattern_bindings(&source[array_start + 1..array_end])
        .into_iter()
        .map(|(_, binding)| binding)
        .collect::<Vec<_>>();
    (!bindings.is_empty()).then_some((rhs_end, bindings))
}

fn rewrite_object_destructuring_helper_writes(
    source: &str,
    object_start: usize,
    helpers: &BTreeSet<BindingName>,
) -> Option<(usize, String)> {
    let object_end = find_matching_brace(source, object_start)?;
    let equals = skip_ws(source.as_bytes(), object_end + 1);
    if source.as_bytes().get(equals) != Some(&b'=') {
        return None;
    }
    let rhs_start = skip_ws(source.as_bytes(), equals + 1);
    let rhs_end = find_assignment_rhs_end(source, rhs_start);
    let setters = parse_object_pattern_bindings(&source[object_start + 1..object_end])
        .into_iter()
        .filter(|(_, binding)| helpers.contains(binding))
        .collect::<Vec<_>>();
    if setters.is_empty() {
        return None;
    }
    let rhs = &source[rhs_start..rhs_end];
    let assignments = setters
        .into_iter()
        .map(|(key, binding)| {
            format!(
                "{}(_$t{})",
                runtime_helper_setter_name(&binding),
                property_access_source(key.as_str())
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Some((
        rhs_end,
        format!("(() => {{ const _$t = {rhs}; {assignments}; return _$t; }})()"),
    ))
}

fn rewrite_array_destructuring_helper_writes(
    source: &str,
    array_start: usize,
    helpers: &BTreeSet<BindingName>,
) -> Option<(usize, String)> {
    if bracket_starts_member_access(source, array_start) {
        return None;
    }
    let array_end = find_matching_bracket(source, array_start)?;
    let equals = skip_ws(source.as_bytes(), array_end + 1);
    if source.as_bytes().get(equals) != Some(&b'=')
        || source.as_bytes().get(equals + 1) == Some(&b'=')
        || source.as_bytes().get(equals + 1) == Some(&b'>')
    {
        return None;
    }
    let rhs_start = skip_ws(source.as_bytes(), equals + 1);
    let rhs_end = find_assignment_rhs_end(source, rhs_start);
    let setters = parse_array_pattern_bindings(&source[array_start + 1..array_end])
        .into_iter()
        .filter(|(_, binding)| helpers.contains(binding))
        .collect::<Vec<_>>();
    if setters.is_empty() {
        return None;
    }
    let rhs = &source[rhs_start..rhs_end];
    let assignments = setters
        .into_iter()
        .map(|(index, binding)| format!("{}(_$t[{index}])", runtime_helper_setter_name(&binding)))
        .collect::<Vec<_>>()
        .join("; ");
    Some((
        rhs_end,
        format!("(() => {{ const _$t = {rhs}; {assignments}; return _$t; }})()"),
    ))
}

fn bracket_starts_member_access(source: &str, bracket_start: usize) -> bool {
    source
        .as_bytes()
        .get(..bracket_start)
        .and_then(|prefix| {
            prefix
                .iter()
                .rposition(|byte| !byte.is_ascii_whitespace())
                .map(|position| prefix[position])
        })
        .is_some_and(|previous| {
            is_identifier_continue(previous) || matches!(previous, b')' | b']' | b'}' | b'.')
        })
}

fn split_top_level_properties(source: &str) -> Vec<&str> {
    let mut properties = Vec::new();
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    let mut start = 0usize;
    let mut depth = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'(' | b'[' | b'{' => {
                depth += 1;
                cursor += 1;
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                cursor += 1;
            }
            b',' if depth == 0 => {
                let property = source[start..cursor].trim();
                if !property.is_empty() {
                    properties.push(property);
                }
                cursor += 1;
                start = cursor;
            }
            _ => cursor += 1,
        }
    }
    let property = source[start..].trim();
    if !property.is_empty() {
        properties.push(property);
    }
    properties
}

fn parse_object_pattern_bindings(source: &str) -> Vec<(String, BindingName)> {
    split_top_level_properties(source)
        .into_iter()
        .filter_map(|property| {
            let property = property.trim();
            let (key, binding) = if let Some(colon) = property.find(':') {
                let key = property[..colon].trim().trim_matches(['"', '\'']);
                let binding = parse_pattern_binding_identifier(property[colon + 1..].trim())?;
                (key.to_string(), binding)
            } else {
                let binding = parse_pattern_binding_identifier(property)?;
                (binding.as_str().to_string(), binding)
            };
            Some((key, binding))
        })
        .collect()
}

fn parse_array_pattern_bindings(source: &str) -> Vec<(usize, BindingName)> {
    split_top_level_properties(source)
        .into_iter()
        .enumerate()
        .filter_map(|(index, element)| {
            let binding = parse_pattern_binding_identifier(element)?;
            Some((index, binding))
        })
        .collect()
}

fn parse_pattern_binding_identifier(source: &str) -> Option<BindingName> {
    let trimmed = source.trim();
    let target = trimmed
        .strip_prefix("...")
        .unwrap_or(trimmed)
        .split('=')
        .next()?
        .trim();
    let (binding, next) = parse_identifier(target, 0)?;
    if is_js_keyword(binding) || skip_ws(target.as_bytes(), next) != target.len() {
        return None;
    }
    Some(BindingName::new(binding))
}

fn property_access_source(key: &str) -> String {
    if key
        .as_bytes()
        .first()
        .is_some_and(|byte| is_identifier_start(*byte))
        && key.as_bytes()[1..]
            .iter()
            .all(|byte| is_identifier_continue(*byte))
        && !is_js_keyword(key)
    {
        format!(".{key}")
    } else {
        format!("[{key:?}]")
    }
}

#[derive(Clone, Copy)]
enum UpdateOperator {
    Increment,
    Decrement,
}

impl UpdateOperator {
    fn source(self) -> &'static str {
        match self {
            Self::Increment => "++",
            Self::Decrement => "--",
        }
    }
}

fn update_operator_at(bytes: &[u8], cursor: usize) -> Option<UpdateOperator> {
    match (bytes.get(cursor), bytes.get(cursor + 1)) {
        (Some(b'+'), Some(b'+')) => Some(UpdateOperator::Increment),
        (Some(b'-'), Some(b'-')) => Some(UpdateOperator::Decrement),
        _ => None,
    }
}

fn is_simple_update_target(source: &str, start: usize, end: usize) -> bool {
    let bytes = source.as_bytes();
    let identifier = &source[start..end];
    if is_js_keyword(identifier) {
        return false;
    }
    if start
        .checked_sub(1)
        .and_then(|index| bytes.get(index))
        .is_some_and(|byte| matches!(*byte, b'#' | b'.'))
    {
        return false;
    }
    let after = skip_ws(bytes, end);
    !matches!(bytes.get(after), Some(b'.' | b'['))
}

fn rewrite_runtime_helper_writes(source: &str, helpers: &BTreeSet<BindingName>) -> String {
    let mut output = String::new();
    let mut last = 0usize;
    let mut changed = false;
    let declaration_bindings = variable_declaration_binding_starts(source);
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            b'+' | b'-' => {
                let Some(operator) = update_operator_at(bytes, cursor) else {
                    cursor += 1;
                    continue;
                };
                let target_start = skip_ws(bytes, cursor + 2);
                let Some((identifier, target_end)) = parse_identifier(source, target_start) else {
                    cursor += 1;
                    continue;
                };
                if declaration_bindings.contains(&target_start)
                    || !is_simple_update_target(source, target_start, target_end)
                {
                    cursor += 1;
                    continue;
                }
                let Some(helper) = helpers
                    .iter()
                    .find(|binding| binding.as_str() == identifier)
                else {
                    cursor += 1;
                    continue;
                };
                output.push_str(&source[last..cursor]);
                output.push_str(
                    runtime_helper_update_expression(helper, operator, UpdatePosition::Prefix)
                        .as_str(),
                );
                last = target_end;
                cursor = target_end;
                changed = true;
            }
            b'{' => {
                if let Some((end, replacement)) =
                    rewrite_object_destructuring_helper_writes(source, cursor, helpers)
                {
                    output.push_str(&source[last..cursor]);
                    output.push_str(replacement.as_str());
                    last = end;
                    cursor = end;
                    changed = true;
                } else {
                    cursor += 1;
                }
            }
            b'[' => {
                if let Some((end, replacement)) =
                    rewrite_array_destructuring_helper_writes(source, cursor, helpers)
                {
                    output.push_str(&source[last..cursor]);
                    output.push_str(replacement.as_str());
                    last = end;
                    cursor = end;
                    changed = true;
                } else {
                    cursor += 1;
                }
            }
            byte if is_identifier_start(byte) => {
                let start = cursor;
                cursor += 1;
                while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
                    cursor += 1;
                }
                if declaration_bindings.contains(&start) {
                    continue;
                }
                let identifier = &source[start..cursor];
                let Some(helper) = helpers
                    .iter()
                    .find(|binding| binding.as_str() == identifier)
                else {
                    continue;
                };
                if start
                    .checked_sub(1)
                    .and_then(|index| bytes.get(index))
                    .is_some_and(|byte| matches!(*byte, b'#' | b'.'))
                {
                    continue;
                }
                let equals = skip_ws(bytes, cursor);
                if let Some(operator) = update_operator_at(bytes, equals) {
                    output.push_str(&source[last..start]);
                    output.push_str(
                        runtime_helper_update_expression(helper, operator, UpdatePosition::Postfix)
                            .as_str(),
                    );
                    last = equals + 2;
                    cursor = equals + 2;
                    changed = true;
                    continue;
                }
                if bytes.get(equals) != Some(&b'=')
                    || bytes.get(equals + 1) == Some(&b'=')
                    || bytes.get(equals + 1) == Some(&b'>')
                {
                    continue;
                }
                let rhs_start = skip_ws(bytes, equals + 1);
                let rhs_end = find_assignment_rhs_end(source, rhs_start);
                let mut nested_helpers = helpers.clone();
                nested_helpers.remove(helper);
                let rhs =
                    rewrite_runtime_helper_writes(&source[rhs_start..rhs_end], &nested_helpers);
                output.push_str(&source[last..start]);
                output.push_str(runtime_helper_setter_name(helper).as_str());
                output.push('(');
                output.push_str(rhs.as_str());
                output.push(')');
                last = rhs_end;
                cursor = rhs_end;
                changed = true;
            }
            _ => cursor += 1,
        }
    }
    if !changed {
        return source.to_string();
    }
    output.push_str(&source[last..]);
    output
}

#[derive(Clone, Copy)]
enum UpdatePosition {
    Prefix,
    Postfix,
}

fn runtime_helper_update_expression(
    binding: &BindingName,
    operator: UpdateOperator,
    position: UpdatePosition,
) -> String {
    let binding_name = binding.as_str();
    let setter = runtime_helper_setter_name(binding);
    match position {
        UpdatePosition::Prefix => format!(
            "(() => {{ let _$u = {binding_name}; let _$n = {operator}_$u; {setter}(_$u); return _$n; }})()",
            operator = operator.source()
        ),
        UpdatePosition::Postfix => format!(
            "(() => {{ let _$u = {binding_name}; let _$p = _$u{operator}; {setter}(_$u); return _$p; }})()",
            operator = operator.source()
        ),
    }
}

fn find_assignment_rhs_end(source: &str, mut cursor: usize) -> usize {
    let bytes = source.as_bytes();
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            b'(' => {
                paren_depth += 1;
                cursor += 1;
            }
            b'[' => {
                bracket_depth += 1;
                cursor += 1;
            }
            b'{' => {
                brace_depth += 1;
                cursor += 1;
            }
            b')' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => return cursor,
            b']' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => return cursor,
            b'}' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => return cursor,
            b')' => {
                paren_depth = paren_depth.saturating_sub(1);
                cursor += 1;
            }
            b']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                cursor += 1;
            }
            b'}' => {
                brace_depth = brace_depth.saturating_sub(1);
                cursor += 1;
            }
            b',' | b';' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => {
                return cursor;
            }
            _ => cursor += 1,
        }
    }
    cursor
}

fn contains_call_to_identifier(source: &str, identifier: &str) -> bool {
    identifier_read_facts_in_source(source)
        .into_iter()
        .any(|fact| fact.name == identifier && fact.is_call_callee)
}

fn contains_identifier_reference(source: &str, identifier: &str) -> bool {
    identifier_read_facts_in_source(source)
        .into_iter()
        .any(|fact| fact.name == identifier)
}

#[cfg(test)]
fn identifier_reference_positions<'a>(
    source: &'a str,
    identifier: &'a str,
) -> impl Iterator<Item = usize> + 'a {
    identifier_read_facts_in_source(source)
        .into_iter()
        .filter(move |fact| fact.name == identifier)
        .map(|fact| fact.byte_end)
}

fn skip_ws(bytes: &[u8], mut cursor: usize) -> usize {
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    cursor
}

fn skip_ws_and_comments(bytes: &[u8], mut cursor: usize, limit: usize) -> usize {
    let limit = limit.min(bytes.len());
    loop {
        while cursor < limit && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor + 1 < limit && bytes[cursor] == b'/' && bytes[cursor + 1] == b'/' {
            cursor = skip_line_comment(bytes, cursor + 2).min(limit);
            continue;
        }
        if cursor + 1 < limit && bytes[cursor] == b'/' && bytes[cursor + 1] == b'*' {
            cursor = skip_block_comment(bytes, cursor + 2).min(limit);
            continue;
        }
        return cursor;
    }
}

fn skip_quoted(bytes: &[u8], start: usize, quote: u8) -> usize {
    if quote == b'`' {
        return skip_template_literal(bytes, start);
    }
    let mut cursor = start + 1;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\\' {
            cursor += 2;
            continue;
        }
        if bytes[cursor] == quote {
            return cursor + 1;
        }
        cursor += 1;
    }
    bytes.len()
}

fn skip_template_literal(bytes: &[u8], start: usize) -> usize {
    let mut cursor = start + 1;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor += 2,
            b'`' => return cursor + 1,
            b'$' if bytes.get(cursor + 1) == Some(&b'{') => {
                cursor = skip_template_expression(bytes, cursor + 2);
            }
            _ => cursor += 1,
        }
    }
    bytes.len()
}

fn skip_template_expression(bytes: &[u8], mut cursor: usize) -> usize {
    let mut depth = 1usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            b'{' => {
                depth += 1;
                cursor += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                cursor += 1;
                if depth == 0 {
                    return cursor;
                }
            }
            _ => cursor += 1,
        }
    }
    bytes.len()
}

fn looks_like_regex_literal(bytes: &[u8], slash: usize) -> bool {
    if bytes.get(slash + 1).is_none() {
        return false;
    }
    let mut cursor = slash;
    while cursor > 0 {
        cursor -= 1;
        if bytes[cursor].is_ascii_whitespace() {
            continue;
        }
        if is_identifier_continue(bytes[cursor]) {
            let end = cursor + 1;
            while cursor > 0 && is_identifier_continue(bytes[cursor - 1]) {
                cursor -= 1;
            }
            return matches!(
                std::str::from_utf8(&bytes[cursor..end]).unwrap_or_default(),
                "return" | "throw" | "case" | "yield" | "delete" | "void" | "typeof"
            );
        }
        return matches!(
            bytes[cursor],
            b'(' | b'='
                | b':'
                | b'['
                | b','
                | b'!'
                | b'?'
                | b'{'
                | b';'
                | b'&'
                | b'|'
                | b'+'
                | b'-'
                | b'*'
                | b'%'
                | b'~'
                | b'^'
                | b'<'
                | b'>'
        );
    }
    true
}

fn skip_regex_literal(bytes: &[u8], start: usize) -> usize {
    let mut cursor = start + 1;
    let mut in_class = false;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor += 2,
            b'[' => {
                in_class = true;
                cursor += 1;
            }
            b']' => {
                in_class = false;
                cursor += 1;
            }
            b'/' if !in_class => {
                cursor += 1;
                while cursor < bytes.len() && bytes[cursor].is_ascii_alphabetic() {
                    cursor += 1;
                }
                return cursor;
            }
            _ => cursor += 1,
        }
    }
    bytes.len()
}

fn runtime_helpers_path(source_file_id: u32) -> String {
    format!("modules/runtime/source-{source_file_id}-helpers.ts")
}

fn named_import_statement<'a>(
    bindings: impl Iterator<Item = &'a BindingName>,
    specifier: &str,
) -> String {
    let names = bindings
        .map(BindingName::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    format!("import {{ {names} }} from '{specifier}';")
}

fn named_import_alias_statement<'a>(
    specifiers: impl Iterator<Item = (&'a str, &'a BindingName)>,
    source: &str,
) -> String {
    let names = specifiers
        .map(|(imported, local)| {
            if imported == local.as_str() {
                local.as_str().to_string()
            } else {
                format!("{imported} as {local}", local = local.as_str())
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("import {{ {names} }} from '{source}';")
}

fn default_import_statement(binding: &BindingName, source: &str) -> String {
    format!("import {} from '{source}';", binding.as_str())
}

fn namespace_import_statement(binding: &BindingName, source: &str) -> String {
    format!("import * as {} from '{source}';", binding.as_str())
}

fn parse_generated_named_import_statement(source: &str) -> Option<(BTreeSet<BindingName>, String)> {
    let source = source.trim();
    let rest = source.strip_prefix("import { ")?;
    let (names, rest) = rest.split_once(" } from '")?;
    let specifier = rest.strip_suffix("';")?;
    if names.trim().is_empty() {
        return None;
    }
    let bindings = names
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(BindingName::new)
        .collect::<BTreeSet<_>>();
    if bindings.is_empty() {
        None
    } else {
        Some((bindings, specifier.to_string()))
    }
}

fn parse_generated_named_reexport_statement(
    source: &str,
) -> Option<(BTreeSet<BindingName>, String)> {
    let source = source.trim();
    let rest = source.strip_prefix("export { ")?;
    let (names, rest) = rest.split_once(" } from '")?;
    let specifier = rest.strip_suffix("';")?;
    if names.trim().is_empty() {
        return None;
    }
    let bindings = names
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(BindingName::new)
        .collect::<BTreeSet<_>>();
    if bindings.is_empty() {
        None
    } else {
        Some((bindings, specifier.to_string()))
    }
}

fn runtime_helper_import_statement(
    bindings: &BTreeSet<BindingName>,
    setter_bindings: &BTreeSet<BindingName>,
    lazy_helper_names: &[&'static str],
    specifier: &str,
) -> String {
    let mut names = bindings
        .iter()
        .map(|binding| binding.as_str().to_string())
        .collect::<Vec<_>>();
    names.extend(setter_bindings.iter().map(runtime_helper_setter_name));
    names.extend(lazy_helper_names.iter().map(|name| (*name).to_string()));
    format!("import {{ {} }} from '{specifier}';", names.join(", "))
}

fn lazy_helper_import_names_for_source(source: &LoweredRuntimeModuleSource) -> Vec<&'static str> {
    let mut names = Vec::new();
    if source.uses_lazy_module {
        names.push("lazyModule");
    }
    if source.uses_lazy_value {
        names.push("lazyValue");
    }
    names
}

fn lazy_module_helper_source() -> &'static str {
    "function lazyModule(factory) {\n  \
        let _$cached;\n  \
        return () => {\n    \
            if (_$cached) return _$cached.exports;\n    \
            var _$module = _$cached = { exports: {} };\n    \
            factory(_$module.exports, _$module);\n    \
            return _$module.exports;\n  \
        };\n\
    }"
}

fn lazy_value_helper_source() -> &'static str {
    "function lazyValue(factory) {\n  \
        let _$init = false;\n  \
        let _$val;\n  \
        return () => {\n    \
            if (!_$init) {\n      \
                _$init = true;\n      \
                _$val = factory();\n    \
            }\n    \
            return _$val;\n  \
        };\n\
    }"
}

fn runtime_helper_setter_name(binding: &BindingName) -> String {
    format!("__reverts_set_{}", binding.as_str())
}

/// Decide whether a prelude var declaration is safe to migrate, and
/// extract its initializer expression when present.
///
/// Returns:
///   * `None` — declaration is not a single-binding `var/let/const X[ =
///     init];` statement, or the initializer is too complex to safely
///     copy to a writer module (calls, member access, identifier
///     references, etc.).
///   * `Some(None)` — bare `var X;` declaration with no initializer.
///   * `Some(Some(literal))` — `var X = LITERAL;` where LITERAL is a
///     side-effect-free literal that can be transplanted as-is.
fn classify_migratable_var_declaration<'a>(
    snippet: &'a str,
    binding: &str,
) -> Option<Option<&'a str>> {
    let trimmed = snippet.trim();
    for keyword in ["var", "let", "const"] {
        if let Some(rest) = trimmed.strip_prefix(keyword)
            && rest.starts_with(|c: char| c.is_ascii_whitespace())
        {
            let rest = rest.trim_start();
            let Some(rest) = rest.strip_suffix(';') else {
                continue;
            };
            let rest = rest.trim();
            // Bare `var X;`
            if rest == binding {
                return Some(None);
            }
            // `var X = INIT;`
            let mut splitter = rest.splitn(2, '=');
            let lhs = splitter.next()?.trim();
            let rhs = splitter.next()?.trim();
            if lhs != binding {
                continue;
            }
            if is_pure_literal_initializer(rhs) {
                return Some(Some(rhs));
            }
        }
    }
    None
}

/// Recognize the small set of literal initializers that are safe to
/// move from the runtime helpers file to a writer module without
/// dragging along references to other bindings. Limited to truly
/// side-effect-free shapes — anything that involves calls, member
/// access, or identifier references is rejected to keep the migration
/// from accidentally introducing forward references in the writer.
fn is_pure_literal_initializer(text: &str) -> bool {
    let text = text.trim();
    if matches!(text, "void 0") {
        return true;
    }
    is_literal_expression(text)
}

/// Remove `var X;` or `var X = LITERAL;` declarations for each binding
/// in `bindings` from the runtime helper source. Used after the Phase
/// 10b/10c migration plan moves the declaration (and any literal
/// initializer) to a new owner module — the declaration is no longer
/// needed in the runtime, and leaving it would either create a
/// duplicate-declaration audit failure or shadow the re-exported
/// binding from the owner module.
fn strip_runtime_var_declarations<'a>(
    source: &str,
    bindings: impl IntoIterator<Item = &'a BindingName>,
) -> String {
    let drop_set: BTreeSet<&str> = bindings.into_iter().map(BindingName::as_str).collect();
    if drop_set.is_empty() {
        return source.to_string();
    }
    let mut out = String::with_capacity(source.len());
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        let stripped = trimmed.trim();
        let matched = stripped
            .strip_prefix("var ")
            .and_then(|rest| rest.strip_suffix(';'))
            .map(|body| {
                // Strip everything past `=` for declarations with an
                // initializer; the migration's gate already verified the
                // initializer is a side-effect-free literal that the
                // writer carries verbatim, so the runtime line is safe
                // to drop entirely.
                body.split('=').next().unwrap_or(body).trim()
            })
            .is_some_and(|name| drop_set.contains(name));
        if matched {
            continue;
        }
        out.push_str(line);
    }
    out
}

fn runtime_helper_setter_declaration(binding: &BindingName) -> String {
    let setter = runtime_helper_setter_name(binding);
    let binding = binding.as_str();
    format!("function {setter}(value) {{ {binding} = value; return value; }}")
}

/// Inline same-module setter calls inside the runtime helpers module.
/// `__reverts_set_X(<arg>)` becomes `(X = <arg>)` whenever:
///   * The identifier appears in call position (not as a value reference
///     or member access).
///   * The argument list contains exactly one expression (no top-level
///     comma).
///
/// The two forms are observationally equivalent for single-argument
/// invocations: both evaluate the argument once, write the binding, and
/// produce the argument's value as the expression's result. The cross-
/// module mutation channel (the setter function) is still defined and
/// exported afterwards — only the runtime's own internal calls collapse.
fn inline_internal_setter_calls(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            out.push_str(&source[cursor..next]);
            cursor = next;
            continue;
        }
        if !is_identifier_start(bytes[cursor]) {
            // Push a single UTF-8 codepoint to keep the byte stream valid.
            // ASCII fits in one byte; multi-byte sequences (continuation
            // bytes match 10xxxxxx) need the full run to stay paired.
            let mut next = cursor + 1;
            if bytes[cursor] >= 0x80 {
                while next < bytes.len() && (bytes[next] & 0xC0) == 0x80 {
                    next += 1;
                }
            }
            out.push_str(&source[cursor..next]);
            cursor = next;
            continue;
        }
        let start = cursor;
        cursor += 1;
        while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
            cursor += 1;
        }
        let ident = &source[start..cursor];
        let Some(target) = ident.strip_prefix("__reverts_set_") else {
            out.push_str(ident);
            continue;
        };
        if target.is_empty() {
            out.push_str(ident);
            continue;
        }
        let prev = start
            .checked_sub(1)
            .and_then(|index| bytes.get(index))
            .copied();
        if matches!(prev, Some(b'.') | Some(b'#')) {
            out.push_str(ident);
            continue;
        }
        let arg_open = skip_ws(bytes, cursor);
        if bytes.get(arg_open) != Some(&b'(') {
            out.push_str(ident);
            continue;
        }
        let Some(arg_close) = find_matching_paren(source, arg_open) else {
            out.push_str(ident);
            continue;
        };
        let arg_slice = &source[arg_open + 1..arg_close];
        if !arg_text_is_single_expression(arg_slice) {
            out.push_str(ident);
            continue;
        }
        // The trim avoids leading/trailing whitespace cluttering the
        // assignment form; the byte-for-byte interior is otherwise
        // preserved so embedded comments / newlines stay intact.
        out.push('(');
        out.push_str(target);
        out.push_str(" = ");
        out.push_str(arg_slice.trim());
        out.push(')');
        cursor = arg_close + 1;
    }
    out
}

/// Walk `source` (the bytes between a call's `(` and `)`) and return
/// `true` iff it contains exactly one top-level expression — i.e., no
/// top-level comma outside nested parens/brackets/braces/strings. Used
/// to gate the setter-call inliner: multi-argument setter calls have
/// different semantics from a comma-folded assignment expression, so
/// they stay as function calls.
fn arg_text_is_single_expression(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            b'(' => {
                let Some(close) = find_matching_paren(source, cursor) else {
                    return false;
                };
                cursor = close + 1;
            }
            b'[' => {
                let Some(close) = find_matching_bracket(source, cursor) else {
                    return false;
                };
                cursor = close + 1;
            }
            b'{' => {
                let Some(close) = find_matching_brace(source, cursor) else {
                    return false;
                };
                cursor = close + 1;
            }
            b',' => return false,
            _ => cursor += 1,
        }
    }
    true
}

fn runtime_namespace_export_statement(namespace_export: &RuntimeNamespaceExport) -> String {
    let properties = namespace_export
        .exports
        .iter()
        .map(|(export_name, binding)| {
            format!(
                "{}: {{ enumerable: true, get: () => {} }}",
                property_key_source(export_name),
                binding.as_str()
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Object.defineProperties({}, {{ {} }});",
        namespace_export.namespace.as_str(),
        properties
    )
}

fn property_key_source(key: &str) -> String {
    if key
        .as_bytes()
        .first()
        .is_some_and(|byte| is_identifier_start(*byte))
        && key.as_bytes()[1..]
            .iter()
            .all(|byte| is_identifier_continue(*byte))
        && !is_js_keyword(key)
    {
        key.to_string()
    } else {
        format!("{key:?}")
    }
}

fn node_require_prelude_statement() -> String {
    "import { createRequire } from 'node:module';\nvar require = createRequire(import.meta.url);"
        .to_string()
}

fn named_export_statement<'a>(bindings: impl Iterator<Item = &'a BindingName>) -> String {
    let names = bindings
        .map(BindingName::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    format!("export {{ {names} }};")
}

fn parse_generated_named_export_statement(source: &str) -> Option<BTreeSet<BindingName>> {
    let source = source.trim();
    let names = source.strip_prefix("export { ")?.strip_suffix(" };")?;
    if names.trim().is_empty() || names.contains(" from ") || names.contains(" as ") {
        return None;
    }
    let mut bindings = BTreeSet::<BindingName>::new();
    for name in names.split(',').map(str::trim) {
        if name.is_empty() {
            return None;
        }
        let (identifier, end) = parse_identifier(name, 0)?;
        if end != name.len() || is_js_keyword(identifier) {
            return None;
        }
        bindings.insert(BindingName::new(identifier));
    }
    (!bindings.is_empty()).then_some(bindings)
}

fn named_reexport_statement<'a>(
    bindings: impl Iterator<Item = &'a BindingName>,
    specifier: &str,
) -> String {
    let names = bindings
        .map(BindingName::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    format!("export {{ {names} }} from '{specifier}';")
}

fn variable_declaration_statement<'a>(bindings: impl Iterator<Item = &'a BindingName>) -> String {
    let names = bindings
        .map(BindingName::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    format!("var {names};")
}

fn coalesce_consecutive_uninitialized_var_declarations(source: &str) -> String {
    let mut output = Vec::<String>::new();
    let mut pending = Vec::<String>::new();
    for line in source.lines() {
        if let Some(binding) = parse_single_uninitialized_var_line(line) {
            pending.push(binding.to_string());
            continue;
        }
        flush_uninitialized_var_run(&mut output, &mut pending);
        output.push(line.to_string());
    }
    flush_uninitialized_var_run(&mut output, &mut pending);
    if source.ends_with('\n') {
        format!("{}\n", output.join("\n"))
    } else {
        output.join("\n")
    }
}

fn flush_uninitialized_var_run(output: &mut Vec<String>, pending: &mut Vec<String>) {
    if pending.is_empty() {
        return;
    }
    if pending.len() == 1 {
        output.push(format!("var {};", pending[0]));
    } else {
        output.push(format!("var {};", pending.join(", ")));
    }
    pending.clear();
}

fn parse_single_uninitialized_var_line(line: &str) -> Option<&str> {
    let line = line.trim();
    let rest = line.strip_prefix("var ")?;
    let name = rest.strip_suffix(';')?;
    let bytes = name.as_bytes();
    if bytes.is_empty() || !is_identifier_start(bytes[0]) {
        return None;
    }
    if bytes[1..].iter().all(|byte| is_identifier_continue(*byte)) {
        Some(name)
    } else {
        None
    }
}

fn relative_import_specifier(from_file: &str, to_file: &str) -> String {
    let from_dir = path_dir_segments(from_file);
    let to_segments = path_file_segments_with_js_extension(to_file);
    let common = common_prefix_len(&from_dir, &to_segments);
    let mut relative = Vec::new();
    relative.extend(std::iter::repeat_n(
        "..".to_string(),
        from_dir.len().saturating_sub(common),
    ));
    relative.extend(to_segments[common..].iter().cloned());
    let joined = relative.join("/");
    if joined.starts_with("..") {
        joined
    } else {
        format!("./{joined}")
    }
}

fn path_dir_segments(path: &str) -> Vec<String> {
    let mut segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    segments.pop();
    segments
}

fn path_file_segments_with_js_extension(path: &str) -> Vec<String> {
    let mut segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if let Some(last) = segments.last_mut()
        && let Some(stripped) = last.strip_suffix(".ts")
    {
        *last = format!("{stripped}.js");
    }
    segments
}

fn common_prefix_len(left: &[String], right: &[String]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    UnparseableSource {
        module_id: ModuleId,
        path: String,
        message: String,
    },
    UnresolvedRuntimeHelperReferences {
        path: String,
        bindings: Vec<BindingName>,
    },
}

impl fmt::Display for PlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnparseableSource {
                module_id,
                path,
                message,
            } => write!(
                formatter,
                "module {} source {} failed normalization: {message}",
                module_id.0, path
            ),
            Self::UnresolvedRuntimeHelperReferences { path, bindings } => {
                let bindings = bindings
                    .iter()
                    .map(BindingName::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    formatter,
                    "runtime helper {path} has unresolved references: {bindings}"
                )
            }
        }
    }
}

impl Error for PlanError {}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use reverts_graph::{RuntimePrelude, RuntimePreludeBindingKind};
    use reverts_input::{
        InputBundle, InputRows, ModuleDependencyInput, ModuleDependencyTarget, ModuleInput,
        PackageAttributionInput, PackageAttributionStatus, PackageSurfaceInput, ProjectInput,
        SourceFileInput, SourceSpan, SymbolInput,
    };
    use reverts_ir::{BindingName, BindingShape, BindingShapeSolution, ModuleId};
    use reverts_model::{
        CompilerEvidence, CompilerKind, CompilerProfile, EnrichedProgram, ModuleCompilerProfile,
        ProgramModel,
    };

    use super::{
        CompilerRecoveryAction, EmitPlan, ImportExportPlanner, PlannedFile,
        RuntimeSetterMigrationBlockerReason, SourceCompilerStrategy, inline_internal_setter_calls,
        inline_remaining_lazy_value_wrappers_allowing_assignments, lower_runtime_helpers,
        parse_generated_named_export_statement, purify_private_runtime_lazy_initializers,
    };

    fn enriched_from_rows(rows: InputRows) -> EnrichedProgram {
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        )
    }

    fn plan_from_rows(rows: InputRows) -> EmitPlan {
        ImportExportPlanner
            .plan_enriched_program(&enriched_from_rows(rows))
            .expect("fixture should normalize")
    }

    fn planned_source(plan: &EmitPlan, path: &str) -> String {
        plan.files
            .iter()
            .find(|file| file.path == path)
            .unwrap_or_else(|| panic!("{path} should be planned"))
            .body
            .join("\n")
    }

    fn planned_source_opt(plan: &EmitPlan, path: &str) -> Option<String> {
        plan.files
            .iter()
            .find(|file| file.path == path)
            .map(|file| file.body.join("\n"))
    }

    #[test]
    fn emit_plan_coalesces_duplicate_generated_named_imports() {
        let mut file = PlannedFile::new("modules/consumer.ts");
        file.push_source("import { beta } from './runtime/source-1-helpers.js';");
        file.push_source("import { alpha } from './runtime/source-1-helpers.js';");
        file.push_source("import { local } from './local.js';");
        file.push_source("console.log(alpha, beta, local);");

        let mut plan = EmitPlan::default();
        plan.push_file(file);
        let source = planned_source(&plan, "modules/consumer.ts");

        assert!(source.contains("import { alpha, beta } from './runtime/source-1-helpers.js';"));
        assert_eq!(
            source
                .matches("from './runtime/source-1-helpers.js'")
                .count(),
            1
        );
        assert!(source.contains("import { local } from './local.js';"));
    }

    #[test]
    fn emit_plan_coalesces_generated_named_exports() {
        let mut file = PlannedFile::new("modules/consumer.ts");
        file.push_source("const keep = 1;");
        file.push_source("export { beta };");
        file.push_source("console.log(keep);");
        file.push_source("export { alpha };");
        file.push_source("export { beta };");

        let mut plan = EmitPlan::default();
        plan.push_file(file);
        let source = planned_source(&plan, "modules/consumer.ts");

        assert!(source.contains("export { alpha, beta };"));
        assert_eq!(source.matches("export {").count(), 1);
        assert!(!source.contains("export { beta };\nconsole.log"));
        assert!(source.contains("console.log(keep);"));
    }

    #[test]
    fn emit_plan_keeps_reexports_and_alias_exports_separate() {
        let mut file = PlannedFile::new("modules/consumer.ts");
        file.push_source("export { beta };");
        file.push_source("export { alpha as renamed };");
        file.push_source("export { gamma } from './gamma.js';");
        file.push_source("export { alpha };");

        let mut plan = EmitPlan::default();
        plan.push_file(file);
        let source = planned_source(&plan, "modules/consumer.ts");

        assert!(source.contains("export { alpha, beta };"));
        assert!(source.contains("export { alpha as renamed };"));
        assert!(source.contains("export { gamma } from './gamma.js';"));
        assert_eq!(source.matches("export {").count(), 3);
    }

    #[test]
    fn emit_plan_coalesces_only_consecutive_plain_var_declarations() {
        let mut file = PlannedFile::new("modules/runtime/source-1-helpers.ts");
        file.push_source(concat!(
            "var alpha;\n",
            "var beta;\n",
            "var gamma = 1;\n",
            "var delta;\n",
            "// barrier\n",
            "var epsilon;\n",
            "var zeta;\n",
            "function run() {\n",
            "  var localA;\n",
            "  var localB;\n",
            "  var keep = localA;\n",
            "}\n",
            "var eta;\n",
            "var theta;\n",
        ));

        let mut plan = EmitPlan::default();
        plan.push_file(file);
        let source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

        assert!(source.contains("var alpha, beta;"));
        assert!(source.contains("var gamma = 1;"));
        assert!(source.contains("var delta;\n// barrier\nvar epsilon, zeta;"));
        assert!(source.contains("var localA, localB;"));
        assert!(source.contains("var keep = localA;"));
        assert!(source.contains("var eta, theta;"));
        assert!(!source.contains("var alpha;\nvar beta;"));
        assert!(!source.contains("var epsilon;\nvar zeta;"));
    }

    #[test]
    fn inline_internal_setter_calls_collapses_single_arg_call() {
        let input = "function foo() { __reverts_set_X(42); }";
        let expected = "function foo() { (X = 42); }";
        assert_eq!(inline_internal_setter_calls(input), expected);
    }

    #[test]
    fn inline_internal_setter_calls_preserves_multi_arg_call() {
        // Multi-argument setter call has different semantics from a
        // comma-folded assignment — the comma expression's value would
        // be the LAST operand, but a setter call returns the FIRST. Stay
        // conservative.
        let input = "__reverts_set_X(a, b);";
        assert_eq!(inline_internal_setter_calls(input), input);
    }

    #[test]
    fn inline_internal_setter_calls_preserves_member_access() {
        // `obj.__reverts_set_X` is not a top-level setter reference;
        // member access must be left alone.
        let input = "obj.__reverts_set_X(v);";
        assert_eq!(inline_internal_setter_calls(input), input);
    }

    #[test]
    fn inline_internal_setter_calls_handles_non_ascii_identifiers() {
        // Greek π (U+03C0) is a two-byte UTF-8 sequence; the rewriter
        // must advance through the whole codepoint to preserve the byte
        // stream. Without this, the first byte (0xCF) would be emitted
        // alone and produce `Ï` instead of `π`.
        let input = "var keymap = { π: 'alt+p' }; __reverts_set_X(1);";
        let expected = "var keymap = { π: 'alt+p' }; (X = 1);";
        assert_eq!(inline_internal_setter_calls(input), expected);
    }

    #[test]
    fn inline_internal_setter_calls_skips_setter_inside_string_literal() {
        // A setter-shaped identifier inside a string literal is not a
        // call expression — `skip_non_code_at` jumps over it.
        let input = r#"var s = "__reverts_set_X(1)"; __reverts_set_X(2);"#;
        let expected = r#"var s = "__reverts_set_X(1)"; (X = 2);"#;
        assert_eq!(inline_internal_setter_calls(input), expected);
    }

    #[test]
    fn identifier_reference_positions_treats_spread_as_read() {
        // `obj.X` is property access and must NOT count as a reference,
        // but `...X` (spread) IS a read of the binding X and must count.
        // The check looks back two bytes to distinguish single `.` from
        // the trailing dot of `...`.
        let source = "function f(a) { return [...X, a]; }";
        let positions: Vec<usize> = super::identifier_reference_positions(source, "X").collect();
        assert_eq!(
            positions.len(),
            1,
            "spread `...X` must register as a single read"
        );
    }

    #[test]
    fn identifier_reference_positions_skips_member_access() {
        let source = "function f(obj) { return obj.X + obj.X.Y; }";
        let positions: Vec<usize> = super::identifier_reference_positions(source, "X").collect();
        assert_eq!(
            positions.len(),
            0,
            "property access `obj.X` must NOT be a binding reference"
        );
    }

    /// Build an `EnrichedProgram` with the binding-shape solution derived from the
    /// def-use graph. Use this in tests where planner output should observe real
    /// shapes; existing tests that explicitly construct
    /// `BindingShapeSolution::default()` are intentionally shape-agnostic.
    fn enriched_with_solved_shapes(input: InputBundle) -> reverts_model::EnrichedProgram {
        let model = ProgramModel::from_input(input);
        let binding_shapes = BindingShapeSolution::from_def_use_graph(model.graph().def_use());
        reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            binding_shapes,
        )
    }

    #[test]
    fn enriched_program_plans_real_source_without_synthetic_declarations() {
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("export const answer = 42;".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");

        assert_eq!(
            plan.files[0].body[0].trim_end(),
            "export const answer = 42;"
        );
    }

    #[test]
    fn lowers_one_parameter_commonjs_wrapper_without_fixed_helper_name() {
        let source = "var hFA = U((uO) => { var U = 1; uO.value = U; });\nhFA();";
        let helper_kinds = BTreeMap::from([(
            BindingName::new("U"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        assert!(lowered.source.contains("var hFA = (() => {"));
        assert!(lowered.source.contains("let _$cached;"));
        assert!(lowered.source.contains("var U = 1; uO.value = U;"));
        assert!(!lowered.source.contains("hFA = U("));
        assert!(!lowered.uses_lazy_module);
        assert!(lowered.lowered_helpers.contains(&BindingName::new("U")));
    }

    #[test]
    fn end_to_end_lowering_chain_recovers_idiomatic_esm_from_real_bundle_shape() {
        // A realistic esbuild-style bundle that exercises every recovery
        // pass in one go:
        //   - `require_const` — single-value CJS `module.exports = 42` →
        //     Phase 6a collapses to a direct number binding.
        //   - `require_config` — multi-primitive `exports.K = primitive` →
        //     Phase 6b collapses to a grouped object literal (config record).
        //   - `require_api` — multi-function `exports.K = function/arrow` →
        //     Phase 6b collapses to an object literal, then Phase 7 explodes
        //     it into three top-level function bindings (API namespace).
        //   - `get_palette` — pure `__lazy(() => { return { ... }; })` →
        //     Phase 5 collapses to a direct object binding.
        let source = concat!(
            "var require_const = $wrap((exports, module) => { module.exports = 42; });\n",
            "var require_config = $wrap((exports, module) => { exports.port = 8080; exports.host = \"localhost\"; });\n",
            "var require_api = $wrap((exports, module) => { exports.parse = function(s) { return JSON.parse(s); }; exports.stringify = function(o) { return JSON.stringify(o); }; exports.identity = function(x) { return x; }; });\n",
            "var get_palette = $lazy(() => { return { primary: \"#abc\", secondary: \"#def\" }; });\n",
            "console.log(\"const:\", require_const());\n",
            "console.log(\"port:\", require_config().port);\n",
            "console.log(\"host:\", require_config().host);\n",
            "console.log(\"parsed:\", require_api().parse('{\"x\":1}').x);\n",
            "console.log(\"stringified:\", require_api().stringify({ ok: true }));\n",
            "console.log(\"identity:\", require_api().identity(99));\n",
            "console.log(\"primary:\", get_palette().primary);\n",
            "console.log(\"secondary:\", get_palette().secondary);\n",
        );
        let helper_kinds = BTreeMap::from([
            (
                BindingName::new("$wrap"),
                RuntimePreludeBindingKind::CommonJsWrapper,
            ),
            (
                BindingName::new("$lazy"),
                RuntimePreludeBindingKind::LazyInitializer,
            ),
        ]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        // Phase 6a: pure-value module.exports collapsed.
        assert!(lowered.source.contains("var require_const = 42;"));
        // Phase 6b: primitive-valued namespace stays grouped.
        assert!(
            lowered
                .source
                .contains("var require_config = { port: 8080, host: \"localhost\" };")
        );
        // Phase 7: function-valued namespace decomposed into bare bindings.
        assert!(!lowered.source.contains("var require_api"));
        assert!(
            lowered
                .source
                .contains("var parse = function(s) { return JSON.parse(s); };")
        );
        assert!(
            lowered
                .source
                .contains("var stringify = function(o) { return JSON.stringify(o); };")
        );
        assert!(
            lowered
                .source
                .contains("var identity = function(x) { return x; };")
        );
        // Phase 5: lazy value with pure object body collapsed.
        assert!(
            lowered
                .source
                .contains("var get_palette = { primary: \"#abc\", secondary: \"#def\" };")
        );

        // Every consumer call site dropped the dead `()` invocation, attaching
        // member access (or argument lists) directly to the bare identifier.
        assert!(
            lowered
                .source
                .contains("console.log(\"const:\", require_const);")
        );
        assert!(
            lowered
                .source
                .contains("console.log(\"port:\", require_config.port);")
        );
        assert!(
            lowered
                .source
                .contains("console.log(\"parsed:\", parse('{\"x\":1}').x);")
        );
        assert!(
            lowered
                .source
                .contains("console.log(\"stringified:\", stringify({ ok: true }));")
        );
        assert!(
            lowered
                .source
                .contains("console.log(\"identity:\", identity(99));")
        );
        assert!(
            lowered
                .source
                .contains("console.log(\"primary:\", get_palette.primary);")
        );

        // No synthetic scaffolding survives in the lowered source.
        assert!(!lowered.source.contains("lazyModule("));
        assert!(!lowered.source.contains("= lazyValue("));
        assert!(!lowered.source.contains("_$"));
        assert!(!lowered.source.contains("__reverts_"));
        // Every reshape was tracked so the planner can downgrade the
        // IR-inferred shape before the audit checks declaration callability.
        for name in [
            "require_const",
            "require_config",
            "get_palette",
            "parse",
            "stringify",
            "identity",
        ] {
            assert!(
                lowered.reshaped_bindings.contains(&BindingName::new(name)),
                "reshaped_bindings missing {name}; got {:?}",
                lowered.reshaped_bindings
            );
        }
    }

    #[test]
    fn delazify_collapses_pure_object_literal_lazy_value() {
        let source = concat!(
            "var palette = L(() => { return { primary: '#abc', secondary: '#def' }; });\n",
            "console.log(palette().primary);\n",
            "use(palette().secondary);\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("L"),
            RuntimePreludeBindingKind::LazyInitializer,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        // Declaration collapsed back to a direct value, callers stripped of `()`.
        assert!(
            lowered
                .source
                .contains("var palette = { primary: '#abc', secondary: '#def' };")
        );
        assert!(lowered.source.contains("console.log(palette.primary);"));
        assert!(lowered.source.contains("use(palette.secondary);"));
        assert!(!lowered.source.contains("= lazyValue("));
        assert!(!lowered.uses_lazy_value);
    }

    #[test]
    fn delazify_skips_lazy_value_used_as_first_class_function() {
        let source = concat!(
            "var thunk = L(() => { return 42; });\n",
            // `thunk` captured as value — not safe to inline because the
            // resulting binding has a different identity (a value vs a
            // function returning the value).
            "register(thunk);\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("L"),
            RuntimePreludeBindingKind::LazyInitializer,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        assert!(lowered.source.contains("var _$l"));
        assert!(lowered.source.contains("var thunk = _$l(() => {"));
        assert!(lowered.source.contains("return 42;"));
        assert!(lowered.source.contains("register(thunk);"));
        assert!(!lowered.source.contains("= lazyValue("));
        assert!(!lowered.uses_lazy_value);
    }

    #[test]
    fn delazify_skips_exported_lazy_value() {
        let source = concat!(
            "var settings = L(() => { return { ready: true }; });\n",
            "if (settings().ready) {}\n",
            "export { settings };\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("L"),
            RuntimePreludeBindingKind::LazyInitializer,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        // `export { settings };` references `settings` as a value, not a call —
        // delazifying would change consumer semantics across the module
        // boundary. Keep the lazy thunk shape, but inline the tiny memoizer
        // locally instead of importing the shared runtime helper.
        assert!(lowered.source.contains("var _$l"));
        assert!(lowered.source.contains("var settings = _$l(() => {"));
        assert!(lowered.source.contains("return { ready: true };"));
        assert!(lowered.source.contains("export { settings };"));
        assert!(!lowered.source.contains("= lazyValue("));
        assert!(!lowered.uses_lazy_value);
    }

    #[test]
    fn delazify_skips_lazy_value_with_side_effect_body() {
        let source = concat!("var init = L(() => { setup(); });\n", "init();\n",);
        let helper_kinds = BTreeMap::from([(
            BindingName::new("L"),
            RuntimePreludeBindingKind::LazyInitializer,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        // Body has a side-effect call but no `return EXPR;` — collapsing to
        // `var init = ...` would evaluate the side effect at module load.
        // Keep the lazy thunk semantics via a local memoizer.
        assert!(lowered.source.contains("var _$l"));
        assert!(lowered.source.contains("var init = _$l(() => {"));
        assert!(lowered.source.contains("setup();"));
        assert!(!lowered.source.contains("= lazyValue("));
        assert!(!lowered.uses_lazy_value);
    }

    #[test]
    fn delazify_skips_lazy_value_with_impure_returned_expression() {
        let source = concat!(
            "var lazy = L(() => { return loadConfig(); });\n",
            "use(lazy());\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("L"),
            RuntimePreludeBindingKind::LazyInitializer,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        // `loadConfig()` is a function call — could have side effects or
        // depend on later state. Don't change evaluation timing.
        assert!(lowered.source.contains("var _$l"));
        assert!(lowered.source.contains("var lazy = _$l(() => {"));
        assert!(lowered.source.contains("return loadConfig();"));
        assert!(!lowered.source.contains("= lazyValue("));
        assert!(!lowered.uses_lazy_value);
    }

    #[test]
    fn inline_remaining_lazy_value_skips_assignment_factories() {
        let source = concat!(
            "var target;\n",
            "var init = L(() => { target ||= makeValue(); });\n",
            "register(init);\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("L"),
            RuntimePreludeBindingKind::LazyInitializer,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        // Assignment bodies are the writer side for runtime-var folding. Keep
        // the canonical lazyValue shape so later phases can still see the
        // assignment and move it with the runtime binding.
        assert!(!lowered.source.contains("var _$l"));
        assert!(lowered.source.contains("var init = lazyValue(() => {"));
        assert!(lowered.source.contains("target ||= makeValue();"));
        assert!(lowered.uses_lazy_value);
    }

    #[test]
    fn final_lazy_value_localization_allows_assignment_factories() {
        let source = concat!(
            "var target;\n",
            "var init = lazyValue(() => { target ||= makeValue(); });\n",
            "register(init);\n",
        );

        let (localized, changed) =
            inline_remaining_lazy_value_wrappers_allowing_assignments(source);

        assert!(changed);
        assert!(localized.contains("var _$l"));
        assert!(localized.contains("var init = _$l(() => {"));
        assert!(localized.contains("target ||= makeValue();"));
        assert!(!localized.contains("= lazyValue("));
    }

    #[test]
    fn inline_remaining_lazy_value_ignores_assignment_lookalikes() {
        let source = concat!(
            "var init = L(() => {\n",
            "  // text: target = value\n",
            "  if (\"target = value\" === 'target = value') setup();\n",
            "  return () => \"target = value\";\n",
            "});\n",
            "register(init);\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("L"),
            RuntimePreludeBindingKind::LazyInitializer,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        assert!(lowered.source.contains("var _$l"));
        assert!(lowered.source.contains("var init = _$l(() => {"));
        assert!(
            lowered
                .source
                .contains("\"target = value\" === 'target = value'")
        );
        assert!(lowered.source.contains("return () => \"target = value\";"));
        assert!(!lowered.source.contains("= lazyValue("));
        assert!(!lowered.uses_lazy_value);
    }

    #[test]
    fn inline_remaining_lazy_value_uses_collision_free_helper_name() {
        let source = concat!(
            "var _$l = 'user binding';\n",
            "var thunk = L(() => { setup(); });\n",
            "register(thunk);\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("L"),
            RuntimePreludeBindingKind::LazyInitializer,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        assert!(
            lowered.source.contains("var _$l1"),
            "expected synthesized helper to avoid the existing user binding:\n{}",
            lowered.source
        );
        assert!(lowered.source.contains("var _$l = 'user binding';"));
        assert!(lowered.source.contains("var thunk = _$l1(() => {"));
        assert!(!lowered.source.contains("= lazyValue("));
        assert!(!lowered.uses_lazy_value);
    }

    #[test]
    fn inline_remaining_lazy_value_keeps_shared_helper_when_runtime_import_remains() {
        let source = concat!(
            "runtimeDep();\n",
            "var thunk = L(() => { setup(); });\n",
            "register(thunk);\n",
        );
        let helper_kinds = BTreeMap::from([
            (
                BindingName::new("L"),
                RuntimePreludeBindingKind::LazyInitializer,
            ),
            (
                BindingName::new("runtimeDep"),
                RuntimePreludeBindingKind::SourceBacked,
            ),
        ]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        assert!(!lowered.source.contains("var _$l"));
        assert!(lowered.source.contains("var thunk = lazyValue(() => {"));
        assert!(lowered.source.contains("runtimeDep();"));
        assert!(lowered.uses_lazy_value);
        assert!(
            lowered
                .remaining_helpers
                .contains(&BindingName::new("runtimeDep")),
            "source-backed runtime binding should still be imported"
        );
    }

    #[test]
    fn delazify_skips_lazy_value_called_with_arguments() {
        let source = concat!(
            "var thunk = L(() => { return [1, 2, 3]; });\n",
            // Calling the thunk with an argument is meaningless under lazy
            // semantics (the factory takes no args), but our delazify pass
            // shouldn't touch it — the binding shape is wrong for inlining.
            "thunk(0);\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("L"),
            RuntimePreludeBindingKind::LazyInitializer,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        assert!(lowered.source.contains("var _$l"));
        assert!(lowered.source.contains("var thunk = _$l(() => {"));
        assert!(lowered.source.contains("thunk(0);"));
        assert!(!lowered.source.contains("= lazyValue("));
        assert!(!lowered.uses_lazy_value);
    }

    #[test]
    fn delazify_preserves_member_access_after_collapsed_call_site() {
        let source = concat!(
            "var theme = L(() => { return { color: 'red' }; });\n",
            "render(theme().color, theme()['palette']);\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("L"),
            RuntimePreludeBindingKind::LazyInitializer,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        // After delazify both `theme()` call sites collapse and member access
        // continues to work naturally — `.color` and `['palette']` attach to
        // the bare identifier.
        assert!(lowered.source.contains("var theme = { color: 'red' };"));
        assert!(
            lowered
                .source
                .contains("render(theme.color, theme['palette']);")
        );
        assert!(!lowered.uses_lazy_value);
    }

    #[test]
    fn delazify_collapses_function_expression_value() {
        let source = concat!(
            "var handler = L(() => { return function(req) { return req.path; }; });\n",
            "register(handler());\n",
            // handler() returns a function; consumers do `handler()(arg)` to
            // call the returned function. After delazify: `handler` IS the
            // function, `handler(arg)` calls it directly.
            "var result = handler()(req);\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("L"),
            RuntimePreludeBindingKind::LazyInitializer,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        assert!(
            lowered
                .source
                .contains("var handler = function(req) { return req.path; };")
        );
        // `register(handler())` collapses to `register(handler)` because the
        // consumer was using the lazy-returned function as a value.
        assert!(lowered.source.contains("register(handler);"));
        // `handler()(req)` collapses to `handler(req)` — the chained call
        // attaches naturally to the now-bare identifier.
        assert!(lowered.source.contains("var result = handler(req);"));
        assert!(!lowered.uses_lazy_value);
    }

    #[test]
    fn delazify_collapses_lazy_module_with_single_value_export() {
        let source = concat!(
            "var entry = U((exports, module) => { module.exports = 42; });\n",
            "console.log(entry());\n",
            "use(entry());\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("U"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        assert!(lowered.source.contains("var entry = 42;"));
        assert!(lowered.source.contains("console.log(entry);"));
        assert!(lowered.source.contains("use(entry);"));
        assert!(!lowered.source.contains("lazyModule("));
        assert!(!lowered.uses_lazy_module);
    }

    #[test]
    fn delazify_collapses_lazy_module_with_object_literal_export() {
        let source = concat!(
            "var config = U((exports, module) => { module.exports = { port: 8080, host: 'localhost' }; });\n",
            "listen(config().port, config().host);\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("U"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        assert!(
            lowered
                .source
                .contains("var config = { port: 8080, host: 'localhost' };")
        );
        assert!(lowered.source.contains("listen(config.port, config.host);"));
        assert!(!lowered.uses_lazy_module);
    }

    #[test]
    fn delazify_collapses_lazy_module_with_class_export() {
        let source = concat!(
            "var Foo = U((exports, module) => { module.exports = class Foo { constructor() {} }; });\n",
            "new (Foo())();\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("U"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        assert!(
            lowered
                .source
                .contains("var Foo = class Foo { constructor() {} };")
        );
        assert!(lowered.source.contains("new (Foo)();"));
        assert!(!lowered.uses_lazy_module);
    }

    #[test]
    fn delazify_collapses_lazy_module_with_multiple_exports_assignments() {
        let source = concat!(
            "var api = U((exports, module) => { exports.foo = 1; exports.bar = 2; });\n",
            "use(api().foo, api().bar);\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("U"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        // Multi-property `exports.foo = ...; exports.bar = ...;` collapses
        // back to an inline object literal — same observable surface to
        // consumers (member access on the binding) without the lazy thunk.
        assert!(lowered.source.contains("var api = { foo: 1, bar: 2 };"));
        assert!(lowered.source.contains("use(api.foo, api.bar);"));
        assert!(!lowered.uses_lazy_module);
    }

    #[test]
    fn delazify_collapses_lazy_module_with_single_param_property_exports() {
        let source = concat!(
            // Single-param form `(exports) =>` with property assignments only —
            // this is the common minified shape when the bundler doesn't use
            // `module`. The collapse uses the exports param name to detect
            // the property targets.
            "var bag = U((uO) => { uO.value = 1; uO.flag = true; });\n",
            "use(bag().value);\n",
            "check(bag().flag);\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("U"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        assert!(
            lowered
                .source
                .contains("var bag = { value: 1, flag: true };")
        );
        assert!(lowered.source.contains("use(bag.value);"));
        assert!(lowered.source.contains("check(bag.flag);"));
        assert!(!lowered.uses_lazy_module);
    }

    #[test]
    fn delazify_skips_lazy_module_with_mixed_property_assignment_and_statement() {
        let source = concat!(
            "var bag = U((exports, module) => { var helper = 1; exports.foo = helper; });\n",
            "use(bag().foo);\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("U"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        // The local `var helper = 1` would have to be hoisted to the
        // consumer or inlined; the current pass refuses anything other
        // than a pure series of `exports.K = PURE_EXPR;` statements.
        assert!(lowered.source.contains("var bag = (() => {"));
        assert!(!lowered.source.contains("lazyModule("));
        assert!(!lowered.uses_lazy_module);
    }

    #[test]
    fn delazify_skips_lazy_module_with_bracket_indexed_exports() {
        let source = concat!(
            // `exports['default']` is computed-key access; out of scope —
            // we only recover bare-identifier keys.
            "var api = U((exports, module) => { exports['default'] = 1; });\n",
            "use(api().default);\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("U"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        assert!(lowered.source.contains("var api = (() => {"));
        assert!(!lowered.source.contains("lazyModule("));
        assert!(!lowered.uses_lazy_module);
    }

    #[test]
    fn delazify_skips_lazy_module_when_body_does_function_call() {
        let source = concat!(
            "var cached = U((exports, module) => { module.exports = computeHeavy(); });\n",
            "use(cached());\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("U"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        // `computeHeavy()` is impure — collapsing would change evaluation
        // timing (module load vs. first access). Keep the lazy wrapper.
        assert!(lowered.source.contains("var cached = (() => {"));
        assert!(!lowered.source.contains("lazyModule("));
        assert!(!lowered.uses_lazy_module);
    }

    #[test]
    fn delazify_collapses_lazy_module_with_single_property_single_param() {
        let source = concat!(
            "var bag = U((uO) => { uO.value = 1; });\n",
            "use(bag().value);\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("U"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        // Single-param `(exports) =>` with a single `exports.key = pure;`
        // statement is the simplest property-export shape — collapses to
        // an inline single-key object literal.
        assert!(lowered.source.contains("var bag = { value: 1 };"));
        assert!(lowered.source.contains("use(bag.value);"));
        assert!(!lowered.uses_lazy_module);
    }

    #[test]
    fn delazify_skips_exported_lazy_module() {
        let source = concat!(
            "var sharedConst = U((exports, module) => { module.exports = 100; });\n",
            "console.log(sharedConst());\n",
            "export { sharedConst };\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("U"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        // Exported binding — cross-module callers would still do
        // `sharedConst()` after we inline. Until the named-import rewriter
        // lands, keep the lazy wrap so the export shape stays a function.
        assert!(lowered.source.contains("var sharedConst = (() => {"));
        assert!(!lowered.source.contains("lazyModule("));
        assert!(!lowered.uses_lazy_module);
    }

    #[test]
    fn delazify_mixes_lazy_value_and_lazy_module_in_one_pass() {
        let source = concat!(
            "var port = L(() => { return 3000; });\n",
            "var host = U((exports, module) => { module.exports = 'localhost'; });\n",
            "listen(host(), port());\n",
        );
        let helper_kinds = BTreeMap::from([
            (
                BindingName::new("L"),
                RuntimePreludeBindingKind::LazyInitializer,
            ),
            (
                BindingName::new("U"),
                RuntimePreludeBindingKind::CommonJsWrapper,
            ),
        ]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        assert!(lowered.source.contains("var port = 3000;"));
        assert!(lowered.source.contains("var host = 'localhost';"));
        assert!(lowered.source.contains("listen(host, port);"));
        assert!(!lowered.uses_lazy_value);
        assert!(!lowered.uses_lazy_module);
    }

    #[test]
    fn decompose_function_namespace_explodes_api_object_into_top_level_bindings() {
        // Bundle-shape input: consumers call the thunk `api()` then access
        // the property. The intermediate Phase 6b object literal explodes
        // back into two top-level bindings whose call sites lose the
        // namespace prefix.
        let source = concat!(
            "var api = U((exports, module) => { exports.parse = function(s) { return s; }; ",
            "exports.stringify = function(o) { return o; }; });\n",
            "api().parse('x');\n",
            "api().stringify({});\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("U"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        assert!(
            !lowered.source.contains("var api ="),
            "got: {}",
            lowered.source
        );
        assert!(
            lowered
                .source
                .contains("var parse = function(s) { return s; };")
        );
        assert!(
            lowered
                .source
                .contains("var stringify = function(o) { return o; };")
        );
        // Call sites: bundle's `api().parse(x)` → `api.parse(x)` after Phase
        // 6b's `()` drop → `parse(x)` after Phase 7's namespace decomposition.
        assert!(lowered.source.contains("parse('x');"));
        assert!(lowered.source.contains("stringify({});"));
        assert!(!lowered.source.contains("api."));
    }

    #[test]
    fn decompose_function_namespace_keeps_primitive_record_grouped() {
        let source = concat!(
            "var config = U((exports, module) => { exports.port = 8080; exports.host = 'localhost'; });\n",
            "listen(config().port, config().host);\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("U"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        // Primitive values describe a data record (config-like). The object
        // stays grouped — splitting would scatter "port" / "host" / "name"
        // / "value" sort of generic identifiers into the module scope and
        // risk both readability loss and collisions.
        assert!(
            lowered
                .source
                .contains("var config = { port: 8080, host: 'localhost' };")
        );
        assert!(lowered.source.contains("listen(config.port, config.host);"));
    }

    #[test]
    fn decompose_function_namespace_keeps_mixed_function_and_primitive_grouped() {
        let source = concat!(
            "var bag = U((exports, module) => { exports.fn = function() { return 1; }; exports.flag = true; });\n",
            "bag().fn();\n",
            "check(bag().flag);\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("U"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        // Mixed function + primitive object — splitting would only partially
        // restore "named exports" semantics and is harder for a reader to
        // reason about. Conservative: keep grouped.
        assert!(lowered.source.contains("var bag = {"));
        assert!(lowered.source.contains("fn: function()"));
        assert!(lowered.source.contains("flag: true"));
        assert!(lowered.source.contains("bag.fn();"));
        assert!(lowered.source.contains("check(bag.flag);"));
    }

    #[test]
    fn decompose_function_namespace_skips_when_binding_passed_as_value() {
        // Mixed access pattern: some `api().method()` (member call), some
        // `api()` alone treated as a value passed to `register`. The bare
        // `api()` collapse to `api` (Phase 6b) makes `register(api)` a
        // namespace handoff — decomposing would break the consumer.
        let source = concat!(
            "var api = U((exports, module) => { exports.run = function() {}; exports.stop = function() {}; });\n",
            "register(api());\n",
            "api().run();\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("U"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        assert!(lowered.source.contains("var api = {"));
        assert!(lowered.source.contains("register(api);"));
    }

    #[test]
    fn decompose_function_namespace_skips_when_exported() {
        let source = concat!(
            "var api = U((exports, module) => { exports.parse = function(s) { return s; }; exports.stringify = function(o) { return o; }; });\n",
            "api().parse('x');\n",
            "export { api };\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("U"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        // `export { api };` references the binding by value — cross-module
        // consumers see `api`, not `parse` / `stringify` individually. The
        // upstream Phase 6b refuses to inline the lazy thunk in this case,
        // so Phase 7 has nothing to decompose either.
        assert!(lowered.source.contains("var api = (() => {"));
        assert!(!lowered.source.contains("lazyModule("));
        assert!(lowered.source.contains("export { api };"));
    }

    #[test]
    fn decompose_function_namespace_skips_when_key_collides_with_existing_binding() {
        let source = concat!(
            "var parse = 'pre-existing';\n",
            "var api = U((exports, module) => { exports.parse = function(s) { return s; }; exports.stringify = function(o) { return o; }; });\n",
            "api().parse('x');\n",
            "api().stringify({});\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("U"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        // The top-level `var parse = 'pre-existing'` would conflict with the
        // decomposed `var parse = function(s)`. Skip the decomposition; the
        // user must rename one of them by hand if they want the cleaner form.
        assert!(lowered.source.contains("var api = {"));
        assert!(lowered.source.contains("api.parse"));
        assert!(lowered.source.contains("api.stringify"));
        assert!(lowered.source.contains("var parse = 'pre-existing';"));
    }

    #[test]
    fn decompose_function_namespace_skips_unknown_key_access() {
        let source = concat!(
            "var api = U((exports, module) => { exports.parse = function() {}; exports.stringify = function() {}; });\n",
            // `api.unknown` is not a key in the object — could be a typo
            // bug in the bundle, or a dynamically-added property elsewhere.
            // Decomposing would silently lose the access; keep grouped.
            "api().unknown();\n",
            "api().parse();\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("U"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        assert!(lowered.source.contains("var api = {"));
        assert!(lowered.source.contains("api.unknown()"));
    }

    #[test]
    fn decompose_function_namespace_explodes_class_values() {
        let source = concat!(
            "var lib = U((exports, module) => { ",
            "exports.Service = class Service { constructor() {} }; ",
            "exports.Worker = class Worker { run() {} }; ",
            "});\n",
            "new (lib().Service)();\n",
            "new (lib().Worker)();\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("U"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        // Class expressions count as "function-shape" values for the namespace
        // decomposition — they're API surfaces, not data.
        assert!(!lowered.source.contains("var lib ="));
        assert!(
            lowered
                .source
                .contains("var Service = class Service { constructor() {} };")
        );
        assert!(
            lowered
                .source
                .contains("var Worker = class Worker { run() {} };")
        );
        assert!(lowered.source.contains("new (Service)();"));
        assert!(lowered.source.contains("new (Worker)();"));
    }

    #[test]
    fn decompose_function_namespace_explodes_arrow_function_values() {
        let source = concat!(
            "var fns = U((exports, module) => { exports.add = (a, b) => a + b; exports.sub = (a, b) => a - b; });\n",
            "fns().add(1, 2);\n",
            "fns().sub(3, 4);\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("U"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        // Arrow function expressions also count.
        assert!(!lowered.source.contains("var fns ="));
        assert!(lowered.source.contains("var add = (a, b) => a + b;"));
        assert!(lowered.source.contains("var sub = (a, b) => a - b;"));
        assert!(lowered.source.contains("add(1, 2);"));
        assert!(lowered.source.contains("sub(3, 4);"));
    }

    #[test]
    fn decompose_function_namespace_skips_when_no_access_sites() {
        // Bundle source has the lazyModule definition but the consumer never
        // calls `unused()`. Phase 6b still inlines the literal (the "all uses
        // are X()" check is vacuously satisfied when there are zero uses),
        // but Phase 7 then has nothing to decompose — decomposing into bare
        // `var parse = ...; var stringify = ...;` would inject unreferenced
        // top-level bindings that look authored. Keep the grouped form so
        // the dead code stays visibly grouped under its original name.
        let source = concat!(
            "var unused = U((exports, module) => { exports.parse = function() {}; exports.stringify = function() {}; });\n",
            "other();\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("U"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        assert!(lowered.source.contains("var unused = {"));
        assert!(lowered.source.contains("parse: function()"));
        assert!(lowered.source.contains("stringify: function()"));
        // Not decomposed:
        assert!(!lowered.source.contains("var parse ="));
        assert!(!lowered.source.contains("var stringify ="));
    }

    #[test]
    fn delazify_ignores_binding_reference_inside_string_or_comment() {
        let source = concat!(
            "var palette = L(() => { return { primary: '#abc' }; });\n",
            // String literal that mentions the binding name as text — must
            // not be flagged as a "value reference" of the binding.
            "log('palette is great');\n",
            // Comment that mentions the binding name — same.
            "// palette is also documented here\n",
            "use(palette().primary);\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("L"),
            RuntimePreludeBindingKind::LazyInitializer,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        assert!(
            lowered
                .source
                .contains("var palette = { primary: '#abc' };")
        );
        assert!(lowered.source.contains("use(palette.primary);"));
        // The string content and the comment remain verbatim.
        assert!(lowered.source.contains("'palette is great'"));
        assert!(
            lowered
                .source
                .contains("// palette is also documented here")
        );
        assert!(!lowered.uses_lazy_value);
    }

    #[test]
    fn end_to_end_planner_delazifies_pure_lazy_bindings_and_omits_helper_file() {
        let planner = ImportExportPlanner;
        // A bundle whose runtime prelude defines a `__commonJS`-style wrapper
        // (`$w`) and a `__lazy`-style initializer (`$l`). The body uses them
        // around pure values that should fully delazify back to direct
        // bindings — and the helper module should NOT be emitted, since no
        // module ends up importing `lazyModule` / `lazyValue`.
        let prelude = concat!(
            "var $w = (factory, cache) => () => ",
            "(cache || factory((cache = { exports: {} }).exports, cache), cache.exports);\n",
            "var $l = (init, cache) => () => (init && (cache = init(init = 0)), cache);\n",
        );
        let body = concat!(
            "var config = $w((exports, module) => { module.exports = { port: 8080, host: 'localhost' }; });\n",
            "var palette = $l(() => { return { primary: '#abc' }; });\n",
            "var api = $w((exports, module) => { exports.parse = function(s) { return s; }; exports.stringify = function(o) { return o; }; });\n",
            "console.log(config().port);\n",
            "render(palette().primary);\n",
            "api().parse('x');\n",
            "api().stringify({});\n",
        );
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");

        // No runtime helper file should be emitted — neither `lazyModule` nor
        // `lazyValue` is referenced after delazify.
        assert!(
            plan.files
                .iter()
                .all(|file| file.path != "modules/runtime/source-1-helpers.ts")
        );
        let entry = plan
            .files
            .iter()
            .find(|file| file.path == "modules/entry.ts")
            .expect("entry file should be planned");
        let entry_source = entry.body.join("\n");

        // Every lazy thunk collapsed to a direct binding; no helper imports.
        assert!(!entry_source.contains("lazyModule("));
        assert!(!entry_source.contains("= lazyValue("));
        assert!(!entry_source.contains("from './runtime/"));

        // Primitive-valued namespaces stay grouped — `config` and `palette`
        // are data records, not API surfaces.
        assert!(
            entry_source.contains("var config = {"),
            "got: {entry_source}"
        );
        assert!(entry_source.contains("port: 8080"));
        assert!(entry_source.contains("host: 'localhost'"));
        assert!(entry_source.contains("var palette = { primary: '#abc' };"));

        // Function-valued namespace `api` decomposes back to individual
        // top-level bindings — restoring the `export function parse / stringify`
        // shape the user would have written in ESM. Helper imports for
        // `lazyModule` / `lazyValue` are gone since nothing references them.
        assert!(!entry_source.contains("var api = "), "got: {entry_source}");
        assert!(entry_source.contains("var parse = function(s)"));
        assert!(entry_source.contains("var stringify = function(o)"));

        // Consumer call sites: member access on data records keeps `X.field`;
        // function-namespace access drops the namespace prefix entirely.
        assert!(entry_source.contains("console.log(config.port);"));
        assert!(entry_source.contains("render(palette.primary);"));
        assert!(entry_source.contains("parse('x');"), "got: {entry_source}");
        assert!(
            entry_source.contains("stringify({});"),
            "got: {entry_source}"
        );
        // And the obsolete namespace form must not survive.
        assert!(!entry_source.contains("api.parse"), "got: {entry_source}");
        assert!(
            !entry_source.contains("api.stringify"),
            "got: {entry_source}"
        );
    }

    #[test]
    fn cross_module_eager_safe_analysis_delazifies_exported_thunk_and_rewrites_consumer() {
        let planner = ImportExportPlanner;
        // Two-module fixture:
        //   - source file 1 declares a CJS-wrapped binding `palette` that
        //     `module.exports = { primary: '#abc' }`.
        //   - source file 2 (the consumer) imports `palette` from the
        //     first file and uses it as `palette().primary` inside a
        //     function body (NestedOnly call form).
        // Phase 8 SHOULD recognise:
        //   - the producer is a singleton SCC in the top-level dep graph
        //     (no cycle through it),
        //   - the consumer's only reference is the zero-arg `palette()`
        //     shape,
        //   - the producer's binding is thunk-wrapped (lazyModule),
        // and therefore eagerify the producer to a direct value AND
        // mechanically rewrite the consumer's `palette()` → `palette`.
        let producer_prelude = concat!(
            "var $w = (factory, cache) => () => ",
            "(cache || factory((cache = { exports: {} }).exports, cache), cache.exports);\n",
        );
        let producer_body =
            "var palette = $w((exports, module) => { module.exports = { primary: '#abc' }; });\n";
        let producer_source = format!("{producer_prelude}{producer_body}");
        let consumer_source = "function render() { return palette().primary; }\nrender();\n";

        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "producer.js",
            Some(producer_source.clone()),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "consumer.js",
            Some(consumer_source.to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "producer", "modules/producer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    producer_prelude.len() as u32,
                    producer_source.len() as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
                .with_source_file(2)
                .with_source_span(SourceSpan::new(0, consumer_source.len() as u32)),
        );
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(2),
            target: ModuleDependencyTarget::Module(ModuleId(1)),
        });

        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let producer = plan
            .files
            .iter()
            .find(|file| file.path == "modules/producer.ts")
            .expect("producer file should be planned");
        let consumer = plan
            .files
            .iter()
            .find(|file| file.path == "modules/consumer.ts")
            .expect("consumer file should be planned");
        let producer_source_out = producer.body.join("\n");
        let consumer_source_out = consumer.body.join("\n");

        // Producer: `palette` is exported AND was thunk-wrapped, but
        // Phase 8 cleared it via the SCC + call-form analysis →
        // delazify happens. The emitted producer carries a direct
        // value, not a `lazyModule(...)` wrap.
        assert!(
            producer_source_out.contains("var palette = { primary: '#abc' };"),
            "producer:\n{producer_source_out}"
        );
        assert!(
            !producer_source_out.contains("lazyModule("),
            "producer should not retain lazyModule wrap:\n{producer_source_out}"
        );
        assert!(producer_source_out.contains("export { palette };"));

        // Consumer: the cross-module rewrite stripped `palette()` to
        // bare `palette`. `palette.primary` is now property access on
        // the directly-imported value.
        assert!(
            consumer_source_out.contains("return palette.primary;"),
            "consumer:\n{consumer_source_out}"
        );
        assert!(
            !consumer_source_out.contains("palette()"),
            "consumer should not retain the zero-arg call:\n{consumer_source_out}"
        );
        // The consumer's bare reference `render()` at module top-level
        // is unchanged (not imported, no rewrite).
        assert!(consumer_source_out.contains("render();"));
    }

    #[test]
    fn cross_module_eager_safe_analysis_keeps_lazy_when_consumer_uses_thunk_as_value() {
        let planner = ImportExportPlanner;
        // Same shape as above, but the consumer passes the thunk as a
        // value (`register(palette)`) — a use Phase 8's call-form
        // analyzer rejects. The producer must KEEP the lazy thunk:
        // mechanically rewriting `palette` → ... would have no safe
        // form, and even delazifying alone (without the rewrite) would
        // break `register(palette)` semantics.
        let producer_prelude = concat!(
            "var $w = (factory, cache) => () => ",
            "(cache || factory((cache = { exports: {} }).exports, cache), cache.exports);\n",
        );
        let producer_body =
            "var palette = $w((exports, module) => { module.exports = { primary: '#abc' }; });\n";
        let producer_source = format!("{producer_prelude}{producer_body}");
        // Consumer uses palette as both a value (passed to `register`)
        // AND as a thunk call (`palette().primary`) — the value use
        // alone disqualifies it from eagerification.
        let consumer_source = concat!(
            "register(palette);\n",
            "function render() { return palette().primary; }\n",
        );

        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "producer.js",
            Some(producer_source.clone()),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "consumer.js",
            Some(consumer_source.to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "producer", "modules/producer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    producer_prelude.len() as u32,
                    producer_source.len() as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
                .with_source_file(2)
                .with_source_span(SourceSpan::new(0, consumer_source.len() as u32)),
        );
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(2),
            target: ModuleDependencyTarget::Module(ModuleId(1)),
        });

        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let producer = plan
            .files
            .iter()
            .find(|file| file.path == "modules/producer.ts")
            .expect("producer file should be planned");
        let producer_source_out = producer.body.join("\n");

        // Producer stays a lazy thunk because of the consumer's
        // disqualifying value-use of `palette`.
        assert!(
            producer_source_out.contains("var palette = (() => {"),
            "producer:\n{producer_source_out}"
        );
    }

    #[test]
    fn end_to_end_planner_keeps_lazy_thunk_when_export_or_side_effect_blocks_delazify() {
        let planner = ImportExportPlanner;
        // Same prelude. This time the bindings either get exported (forcing
        // the lazy thunk to stay so the cross-module surface remains a
        // function) or have side-effect bodies that can't be safely hoisted
        // to module-load time.
        let prelude = concat!(
            "var $w = (factory, cache) => () => ",
            "(cache || factory((cache = { exports: {} }).exports, cache), cache.exports);\n",
            "var $l = (init, cache) => () => (init && (cache = init(init = 0)), cache);\n",
        );
        let body = concat!(
            "var entry = $w((exports, module) => { module.exports = 1; });\n",
            "var init = $l(() => { entry(); });\n",
            "init();\n",
            "export { entry };\n",
        );
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");

        let entry = plan
            .files
            .iter()
            .find(|file| file.path == "modules/entry.ts")
            .expect("entry file should be planned");
        let entry_source = entry.body.join("\n");

        // `entry` is exported — delazifying would change cross-module
        // semantics. Stays a lazy thunk, but the CommonJS memoizer is local.
        assert!(entry_source.contains("var entry = (() => {"));
        assert!(!entry_source.contains("lazyModule("));
        // `init` body is `entry();` (side effect, no return). Can't
        // hoist to module load time. Stays as a lazy thunk, now with a
        // module-local memoizer instead of a runtime lazyValue import.
        assert!(entry_source.contains("var _$l"), "{entry_source}");
        assert!(entry_source.contains("var init = _$l(() => {"));
        assert!(!entry_source.contains("= lazyValue("));
        assert!(
            planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none(),
            "all lazy wrappers are now local, so no shared runtime helper is needed"
        );
    }

    #[test]
    fn source_backed_helper_calls_are_not_lowered_by_shape() {
        let source = concat!(
            "var schemaCache = pvH(() => { return { keys: ['name'] }; });\n",
            "schemaCache.value.keys;\n",
        );
        let helper_kinds = BTreeMap::from([(
            BindingName::new("pvH"),
            RuntimePreludeBindingKind::SourceBacked,
        )]);

        let lowered =
            lower_runtime_helpers(source, &helper_kinds, &BTreeSet::new(), &BTreeSet::new());

        assert_eq!(lowered.source, source);
        assert!(lowered.lowered_helpers.is_empty());
        assert!(lowered.remaining_helpers.contains(&BindingName::new("pvH")));
    }

    #[test]
    fn planner_direct_imports_bare_runtime_prelude_namespace_imports() {
        let prelude = "import * as pathNS from 'path';\n";
        let body = "var value = pathNS.join('a', 'b');\nexport { value };\n";
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );

        let plan = plan_from_rows(rows);
        let entry_source = planned_source(&plan, "modules/entry.ts");

        assert!(entry_source.contains("import * as pathNS from 'path';"));
        assert!(entry_source.contains("pathNS.join('a', 'b')"));
        assert!(
            planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none(),
            "the prelude import is now consumed directly, so no runtime helper file is needed"
        );
    }

    #[test]
    fn planner_direct_imports_bare_runtime_prelude_default_and_named_imports() {
        let prelude =
            "import proc, { cwd as cwdAlias, default as procAlias } from 'node:process';\n";
        let body = "var value = [proc, cwdAlias(), procAlias];\nexport { value };\n";
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );

        let plan = plan_from_rows(rows);
        let entry_source = planned_source(&plan, "modules/entry.ts");

        assert!(entry_source.contains("import proc from 'node:process';"));
        assert!(
            entry_source
                .contains("import { cwd as cwdAlias, default as procAlias } from 'node:process';")
        );
        assert!(entry_source.contains("procAlias"));
        let helper_source = planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts");
        assert!(helper_source.is_none(), "{helper_source:?}");
    }

    #[test]
    fn planner_keeps_relative_runtime_prelude_imports_on_runtime_path() {
        let prelude = "import * as localNS from './local.js';\n";
        let body = "var value = localNS.value;\nexport { value };\n";
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );

        let plan = plan_from_rows(rows);
        let entry_source = planned_source(&plan, "modules/entry.ts");
        let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

        assert!(entry_source.contains("import { localNS } from './runtime/source-1-helpers.js';"));
        assert!(helper_source.contains("import * as localNS from './local.js';"));
        assert!(helper_source.contains("export { localNS };"));
    }

    #[test]
    fn planner_direct_imports_profitable_prelude_import_when_runtime_edge_remains() {
        let prelude = "import { join as pathJoin } from 'path';\nfunction helper() { return 1; }\n";
        let body = "var value = pathJoin('a', String(helper()));\nexport { value };\n";
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );

        let plan = plan_from_rows(rows);
        let entry_source = planned_source(&plan, "modules/entry.ts");
        let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

        assert!(entry_source.contains("import { join as pathJoin } from 'path';"));
        assert!(
            entry_source.contains("import { helper } from './runtime/source-1-helpers.js';"),
            "entry:\n{entry_source}"
        );
        assert!(!helper_source.contains("from 'path'"));
        assert!(helper_source.contains("function helper()"));
        assert!(helper_source.contains("export { helper };"));
    }

    #[test]
    fn planner_keeps_namespace_prelude_import_on_runtime_path_when_runtime_edge_remains() {
        let prelude = "import * as pathNS from 'path';\nfunction helper() { return 1; }\n";
        let body = "var value = pathNS.join('a', String(helper()));\nexport { value };\n";
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );

        let plan = plan_from_rows(rows);
        let entry_source = planned_source(&plan, "modules/entry.ts");
        let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

        assert!(
            entry_source
                .contains("import { helper, pathNS } from './runtime/source-1-helpers.js';")
        );
        assert!(!entry_source.contains("from 'path'"));
        assert!(helper_source.contains("import * as pathNS from 'path';"));
        assert!(helper_source.contains("export { helper, pathNS };"));
    }

    #[test]
    fn planner_keeps_shared_prelude_import_on_runtime_path_when_direct_would_grow() {
        let prelude = "import { join as pathJoin } from 'path';\nfunction helper() { return 1; }\n";
        let body_a = "var valueA = pathJoin('a', String(helper()));\nexport { valueA };\n";
        let body_b = "var valueB = pathJoin('b', String(helper()));\nexport { valueB };\n";
        let source = format!("{prelude}{body_a}{body_b}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "a", "modules/a.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + body_a.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "b", "modules/b.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + body_a.len()) as u32,
                    source.len() as u32,
                )),
        );

        let plan = plan_from_rows(rows);
        let a_source = planned_source(&plan, "modules/a.ts");
        let b_source = planned_source(&plan, "modules/b.ts");
        let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

        assert!(
            a_source.contains("import { helper, pathJoin } from './runtime/source-1-helpers.js';")
        );
        assert!(
            b_source.contains("import { helper, pathJoin } from './runtime/source-1-helpers.js';")
        );
        assert!(!a_source.contains("from 'path'"));
        assert!(!b_source.contains("from 'path'"));
        assert!(helper_source.contains("import { join as pathJoin } from 'path';"));
        assert!(helper_source.contains("export { helper, pathJoin };"));
    }

    #[test]
    fn planner_keeps_prelude_import_on_runtime_path_when_helper_reads_it() {
        let prelude = "import * as pathNS from 'path';\nfunction helper() { return pathNS.sep; }\n";
        let body = "var value = pathNS.join('a', helper());\nexport { value };\n";
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );

        let plan = plan_from_rows(rows);
        let entry_source = planned_source(&plan, "modules/entry.ts");
        let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

        assert!(
            entry_source
                .contains("import { helper, pathNS } from './runtime/source-1-helpers.js';")
        );
        assert!(!entry_source.contains("from 'path'"));
        assert!(helper_source.contains("import * as pathNS from 'path';"));
        assert!(helper_source.contains("function helper()"));
        assert!(helper_source.contains("export { helper, pathNS };"));
    }

    #[test]
    fn enriched_program_normalizes_source_before_emit_plan() {
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("export function add(a,b){return a+b}".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");

        assert!(plan.files[0].body[0].contains("export function add(a, b)"));
        assert!(plan.files[0].body[0].contains("return a + b;"));
    }

    #[test]
    fn enriched_program_plans_real_source_slice_from_bundle_span() {
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("export const one = 1;\nexport const two = 2;".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "one", "modules/one.ts")
                .with_source_file(1)
                .with_source_span(reverts_input::SourceSpan::new(0, 21)),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "two", "modules/two.ts")
                .with_source_file(1)
                .with_source_span(reverts_input::SourceSpan::new(22, 43)),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");

        assert_eq!(plan.files[0].body[0].trim_end(), "export const one = 1;");
        assert_eq!(plan.files[1].body[0].trim_end(), "export const two = 2;");
    }

    #[test]
    fn compiler_profile_selects_webpack_recovery_decision() {
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("__webpack_require__(1);".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let mut compiler_profile = CompilerProfile::default();
        compiler_profile.insert_module(
            ModuleId(1),
            ModuleCompilerProfile::new(
                CompilerKind::Webpack,
                false,
                vec![CompilerEvidence::Identifier(
                    "__webpack_require__".to_string(),
                )],
            ),
        );
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        )
        .with_compiler_profile(compiler_profile);

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");

        assert_eq!(
            plan.files[0].compiler_recovery.strategy,
            SourceCompilerStrategy::WebpackRuntime
        );
        assert_eq!(
            plan.files[0].compiler_recovery.action,
            CompilerRecoveryAction::PreserveWebpackRuntime
        );
        assert_eq!(
            plan.files[0].compiler_recovery.evidence,
            vec![CompilerEvidence::Identifier(
                "__webpack_require__".to_string()
            )]
        );
        assert_eq!(plan.files[0].body[0].trim_end(), "__webpack_require__(1);");
    }

    #[test]
    fn compiler_recovery_actions_cover_known_compilers() {
        assert_eq!(
            CompilerRecoveryAction::from_compiler(CompilerKind::Webpack),
            CompilerRecoveryAction::PreserveWebpackRuntime
        );
        assert_eq!(
            CompilerRecoveryAction::from_compiler(CompilerKind::Esbuild),
            CompilerRecoveryAction::PreserveEsbuildHelpers
        );
        assert_eq!(
            CompilerRecoveryAction::from_compiler(CompilerKind::Rollup),
            CompilerRecoveryAction::PreserveRollupFacade
        );
        assert_eq!(
            CompilerRecoveryAction::from_compiler(CompilerKind::Babel),
            CompilerRecoveryAction::PreserveBabelTranspiledOutput
        );
        assert_eq!(
            CompilerRecoveryAction::from_compiler(CompilerKind::Terser),
            CompilerRecoveryAction::PreserveTerserMinifiedOutput
        );
        assert_eq!(
            CompilerRecoveryAction::from_compiler(CompilerKind::Unknown),
            CompilerRecoveryAction::DirectModuleSource
        );
    }

    #[test]
    fn shape_upgrade_above_namespace_object_drops_known_members_on_purpose() {
        // Design contract: once member-access constraints get upgraded by a
        // stronger constraint (here `Call`), the merged shape is no longer
        // `NamespaceObject`, and the previously collected property names are
        // no longer a reliable surface — so the planner drops them. Pinning
        // this so we notice if a future refactor starts attaching members
        // to shapes other than NamespaceObject.
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("function ns() { return 1; }\nconst v = ns.foo;\nns();".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let enriched = enriched_with_solved_shapes(input);

        // Sanity: solver collects `foo` as a member-access property…
        let raw_members = enriched.known_members(ModuleId(1), "ns");
        assert_eq!(
            raw_members
                .iter()
                .map(BindingName::as_str)
                .collect::<Vec<_>>(),
            vec!["foo"],
        );
        // …but the merged shape settles above NamespaceObject because of
        // the `ns()` call.
        let shape = enriched.binding_shape(ModuleId(1), "ns");
        assert!(
            shape > BindingShape::NamespaceObject,
            "expected merged shape above NamespaceObject, got {shape:?}",
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");

        let ns_binding = plan.files[0]
            .bindings
            .iter()
            .find(|binding| binding.original.as_str() == "ns")
            .expect("ns binding should be planned");
        assert_eq!(ns_binding.shape, shape);
        assert!(
            ns_binding.known_members.is_empty(),
            "known_members must be empty for non-NamespaceObject shapes, got {:?}",
            ns_binding.known_members,
        );
    }

    #[test]
    fn enriched_program_attaches_known_members_to_cross_module_imported_namespaces() {
        // Paper #7 downstream — cross-application-module path. When moduleB
        // imports `ns` from sibling moduleA and accesses `ns.foo`, the
        // planner's `imports_by_module` wiring runs first and previously
        // hard-coded BindingShape::Unknown. Solver-derived shape must reach
        // the planned binding so `known_members` stays consistent regardless
        // of which import path emits it.
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "modules/a.ts",
            Some("export const ns = { foo: 1, bar: 2 };".to_string()),
        ));
        // moduleB references `ns` without an explicit `import` statement —
        // the kind of cross-module bundle layout the planner's
        // `imports_by_module` wiring synthesizes a named import for.
        // (When an explicit `import` is present, the source-imports path
        // handles the binding; line 289 only fires for this implicit form.)
        rows.source_files.push(SourceFileInput::new(
            2,
            "modules/b.ts",
            Some("const a = ns.foo;\nconst b = ns.bar;".to_string()),
        ));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "a", "modules/a.ts").with_source_file(1));
        rows.modules
            .push(ModuleInput::application(ModuleId(2), "b", "modules/b.ts").with_source_file(2));
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(2),
            target: ModuleDependencyTarget::Module(ModuleId(1)),
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let enriched = enriched_with_solved_shapes(input);

        assert_eq!(
            enriched.binding_shape(ModuleId(2), "ns"),
            BindingShape::NamespaceObject,
            "solver must see `ns` as a namespace from moduleB's perspective",
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");

        let module_b_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/b.ts")
            .expect("moduleB plan should exist");
        let ns_binding = module_b_file
            .bindings
            .iter()
            .find(|binding| binding.original.as_str() == "ns")
            .expect("ns binding from cross-module import should be planned");
        assert_eq!(
            ns_binding.shape,
            BindingShape::NamespaceObject,
            "imports_by_module path must pick up solver-derived shape",
        );
        let members: Vec<_> = ns_binding
            .known_members
            .iter()
            .map(BindingName::as_str)
            .collect();
        assert_eq!(members, vec!["bar", "foo"]);
    }

    #[test]
    fn enriched_program_attaches_known_members_to_imported_namespace_bindings() {
        // Paper #7 downstream — source-imports path: `import * as ns from
        // 'pkg'` followed by `ns.foo` / `ns.bar` should reach the planner
        // with shape=NamespaceObject and known_members={bar, foo}. Before
        // this wiring, the source-imports loop hard-coded BindingShape::
        // Unknown so imported namespaces could never carry known_members.
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("import * as ns from 'pkg';\nconst a = ns.foo;\nconst b = ns.bar;".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1),
        );
        rows.package_surfaces.push(PackageSurfaceInput {
            package_name: "pkg".to_string(),
            package_version: Some("1.0.0".to_string()),
            export_specifier: "pkg".to_string(),
            status: PackageAttributionStatus::Accepted,
            evidence: None,
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let enriched = enriched_with_solved_shapes(input);

        assert_eq!(
            enriched.binding_shape(ModuleId(1), "ns"),
            BindingShape::NamespaceObject,
            "solver should classify the imported namespace",
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");

        let ns_binding = plan.files[0]
            .bindings
            .iter()
            .find(|binding| binding.original.as_str() == "ns")
            .expect("imported ns binding should be planned");
        assert_eq!(ns_binding.shape, BindingShape::NamespaceObject);
        let members: Vec<_> = ns_binding
            .known_members
            .iter()
            .map(BindingName::as_str)
            .collect();
        assert_eq!(members, vec!["bar", "foo"]);
    }

    #[test]
    fn enriched_program_attaches_known_members_to_namespace_object_bindings() {
        // Paper #7 downstream: when the shape solver classifies a definition
        // as `NamespaceObject` from `ns.foo` / `ns.bar` accesses, the planner
        // must thread those property names onto the `PlannedBinding` so the
        // emitter and audit gates can reason about the namespace's surface.
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some(
                "const ns = { foo: 1, bar: 2 };\nconst a = ns.foo;\nconst b = ns.bar;".to_string(),
            ),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let enriched = enriched_with_solved_shapes(input);

        // Sanity-check the shape solver before asserting on planner output.
        assert_eq!(
            enriched.binding_shape(ModuleId(1), "ns"),
            BindingShape::NamespaceObject
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");

        let ns_binding = plan.files[0]
            .bindings
            .iter()
            .find(|binding| binding.original.as_str() == "ns")
            .expect("ns binding should be planned");
        assert_eq!(ns_binding.shape, BindingShape::NamespaceObject);
        let members: Vec<_> = ns_binding
            .known_members
            .iter()
            .map(BindingName::as_str)
            .collect();
        assert_eq!(members, vec!["bar", "foo"]);

        // Non-NamespaceObject bindings must remain memberless to avoid noise.
        let a_binding = plan.files[0]
            .bindings
            .iter()
            .find(|binding| binding.original.as_str() == "a")
            .expect("a binding should be planned");
        assert!(a_binding.known_members.is_empty());
    }

    #[test]
    fn enriched_program_plans_recovered_bindings_with_shapes() {
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("function factory() { return 42; }".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let enriched = enriched_with_solved_shapes(input);

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");

        assert_eq!(plan.files[0].bindings.len(), 1);
        assert_eq!(plan.files[0].bindings[0].original.as_str(), "factory");
        assert_eq!(plan.files[0].bindings[0].shape, BindingShape::Callable);
        assert!(plan.files[0].bindings[0].source_backed);
    }

    #[test]
    fn input_symbol_without_ast_definition_is_planned_as_not_source_backed() {
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules.push(ModuleInput::application(
            ModuleId(1),
            "entry",
            "src/index.ts",
        ));
        rows.symbols.push(SymbolInput::new(ModuleId(1), "missing"));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");

        let missing = plan.files[0]
            .bindings
            .iter()
            .find(|binding| binding.original.as_str() == "missing")
            .expect("input symbol should be planned");
        assert!(!missing.source_backed);
    }

    #[test]
    fn source_backed_symbol_plans_late_readability_rename() {
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("var $F1 = 1; export { $F1 };".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1),
        );
        rows.symbols.push(
            SymbolInput::new(ModuleId(1), "$F1").with_semantic_name("lodashGlobalObjectInit"),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let mut semantic_names = reverts_model::SemanticNameMap::default();
        semantic_names.insert_binding(ModuleId(1), "$F1", "lodashGlobalObjectInit");
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            semantic_names,
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let binding = plan.files[0]
            .bindings
            .iter()
            .find(|binding| binding.original.as_str() == "$F1")
            .expect("source binding should be planned");

        assert!(binding.source_backed);
        assert_eq!(binding.emitted.as_str(), "lodashGlobalObjectInit");
        assert_eq!(plan.files[0].exports[0].binding.as_str(), "$F1");
        assert_eq!(plan.files[0].readability_renames.len(), 1);
        assert_eq!(
            plan.files[0].readability_renames[0].original.as_str(),
            "$F1"
        );
        assert_eq!(
            plan.files[0].readability_renames[0].renamed.as_str(),
            "lodashGlobalObjectInit"
        );
    }

    #[test]
    fn source_backed_import_plans_late_readability_rename() {
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("import { map as $F1 } from 'lodash'; export { $F1 };".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let mut semantic_names = reverts_model::SemanticNameMap::default();
        semantic_names.insert_binding(ModuleId(1), "$F1", "lodashMap");
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            semantic_names,
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let binding = plan.files[0]
            .bindings
            .iter()
            .find(|binding| binding.original.as_str() == "$F1")
            .expect("source import binding should be planned");

        assert!(binding.source_backed);
        assert_eq!(binding.emitted.as_str(), "lodashMap");
        assert_eq!(plan.files[0].readability_renames.len(), 1);
        assert_eq!(
            plan.files[0].readability_renames[0].original.as_str(),
            "$F1"
        );
        assert_eq!(
            plan.files[0].readability_renames[0].renamed.as_str(),
            "lodashMap"
        );
    }

    #[test]
    fn enriched_program_plans_source_backed_ast_exports() {
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("const answer = 42; export { answer };".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");

        assert_eq!(plan.files[0].exports.len(), 1);
        assert_eq!(plan.files[0].exports[0].binding.as_str(), "answer");
        assert!(plan.files[0].exports[0].source_backed);
    }

    #[test]
    fn source_imported_binding_can_back_source_export() {
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some("import { answer } from 'pkg'; export { answer };".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");

        assert!(
            plan.files[0]
                .bindings
                .iter()
                .any(|binding| { binding.original.as_str() == "answer" && binding.source_backed })
        );
        assert_eq!(plan.files[0].exports[0].binding.as_str(), "answer");
    }

    #[test]
    fn enriched_program_lowers_runtime_helpers_from_arbitrary_binding_names() {
        let planner = ImportExportPlanner;
        let prelude = concat!(
            "var $wrap7 = (factory, cache) => () => ",
            "(cache || factory((cache = { exports: {} }).exports, cache), cache.exports);\n",
            "var _lazy9 = (init, cache) => () => (init && (cache = init(init = 0)), cache);\n",
        );
        let body = concat!(
            "var entry = $wrap7((exports, module) => { module.exports = 1; });\n",
            "var init = _lazy9(() => { entry(); });\n",
            "init();\n",
            "export { entry };\n",
        );
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let module_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/entry.ts")
            .expect("module file should be planned");

        assert!(
            plan.files
                .iter()
                .all(|file| file.path != "modules/runtime/source-1-prelude.ts")
        );
        let module_source = module_file.body.join("\n");
        assert!(!module_source.contains("$wrap7"));
        assert!(!module_source.contains("_lazy9"));
        assert!(module_source.contains("var entry = (() => {"));
        assert!(!module_source.contains("lazyModule("));
        assert!(!module_source.contains("= lazyValue("));
        // The remaining CommonJS lazy boundary is now local to this module,
        // so its tiny memoization temps live in the recovered module instead
        // of requiring shared runtime lazyModule/lazyValue imports.
        assert!(module_source.contains("_$cached"));
        assert!(module_source.contains("var _$l"), "{module_source}");
        assert!(module_source.contains("var init = _$l(() => {"));
        assert!(module_source.contains("_$module"));
    }

    #[test]
    fn entrypoint_runtime_uses_shared_helper_module_with_tail_side_effects() {
        let planner = ImportExportPlanner;
        let prelude = "function main() { return cliEntry(); }\n";
        let body = "var cliEntry = () => 'ok';\nvar cliInit = () => {};\n";
        let tail = "cliInit();\nprocess.env.FLAG = 'ok';\nmain();\n";
        let source = format!("{prelude}{body}{tail}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + body.len()) as u32,
                )),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let module_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/entry.ts")
            .expect("module file should be planned");
        let cli_file = plan
            .files
            .iter()
            .find(|file| file.path == "cli.ts")
            .expect("cli entrypoint should be planned");
        let helper_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
            .expect("runtime helper file should be planned");

        assert!(
            plan.files
                .iter()
                .all(|file| file.path != "modules/runtime/source-1-prelude.ts")
        );
        let module_source = module_file.body.join("\n");
        let cli_source = cli_file.body.join("\n");
        let helper_source = helper_file.body.join("\n");

        assert!(module_source.contains("export { cliEntry, cliInit };"));
        assert!(cli_source.contains("#!/usr/bin/env node"));
        assert!(
            cli_source.contains("import { main } from './modules/runtime/source-1-helpers.js';")
        );
        assert!(cli_source.contains("await main();"));
        assert!(!cli_source.contains("function main()"));
        assert!(!cli_source.contains("cliInit();"));
        assert!(!cli_source.contains("process.env.FLAG = 'ok';"));
        assert!(!cli_source.contains("source-1-prelude"));
        assert!(helper_source.contains("import { cliEntry, cliInit } from '../entry.js';"));
        assert!(helper_source.contains("function main()"));
        assert!(helper_source.contains("return cliEntry();"));
        assert!(helper_source.contains("cliInit();"));
        assert!(helper_source.contains("process.env.FLAG = 'ok';"));
        assert!(helper_source.contains("export { main };"));
    }

    #[test]
    fn entrypoint_runtime_and_module_setters_share_single_helper_state() {
        let planner = ImportExportPlanner;
        let prelude = "var yA;\nfunction main() { initModule(); return yA(); }\n";
        let body = "yA = () => 'linux';\nfunction initModule() {}\nexport { initModule };\n";
        let tail = "main();\n";
        let source = format!("{prelude}{body}{tail}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + body.len()) as u32,
                )),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let entry_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/entry.ts")
            .expect("entry file should be planned");
        let cli_file = plan
            .files
            .iter()
            .find(|file| file.path == "cli.ts")
            .expect("cli entrypoint should be planned");
        let helper_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
            .expect("runtime helper file should be planned");
        let entry_source = entry_file.body.join("\n");
        let cli_source = cli_file.body.join("\n");
        let helper_source = helper_file.body.join("\n");

        assert!(
            entry_source
                .contains("import { __reverts_set_yA } from './runtime/source-1-helpers.js';")
        );
        assert!(!entry_source.contains("import { yA, __reverts_set_yA }"));
        assert!(entry_source.contains("__reverts_set_yA(() => 'linux');"));
        assert!(
            cli_source.contains("import { main } from './modules/runtime/source-1-helpers.js';")
        );
        assert!(cli_source.contains("await main();"));
        assert!(!cli_source.contains("var yA"));
        assert!(helper_source.contains("import { initModule } from '../entry.js';"));
        assert!(helper_source.contains("var yA;"));
        assert!(helper_source.contains("function main()"));
        assert!(helper_source.contains("return yA();"));
        assert!(
            helper_source
                .contains("function __reverts_set_yA(value) { yA = value; return value; }")
        );
        let export_line = helper_source
            .lines()
            .find(|line| line.starts_with("export {"))
            .expect("helper should export runtime bindings");
        let exports = parse_generated_named_export_statement(export_line)
            .expect("helper should emit a generated export list");
        assert!(exports.contains(&BindingName::new("__reverts_set_yA")));
        assert!(exports.contains(&BindingName::new("main")));
        assert!(!exports.contains(&BindingName::new("yA")));
    }

    #[test]
    fn entrypoint_runtime_preserves_side_effect_order_before_later_runtime_declarations() {
        let planner = ImportExportPlanner;
        let prelude = concat!(
            "var Constructor;\n",
            "function initializeConstructor() { Constructor = class RuntimeCommand {}; }\n",
        );
        let body = "export const value = 1;\n";
        let tail = concat!(
            "initializeConstructor();\n",
            "var command = new Constructor();\n",
            "function main() { return command; }\n",
            "main();\n",
        );
        let source = format!("{prelude}{body}{tail}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + body.len()) as u32,
                )),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let helper_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
            .expect("runtime helper file should be planned");
        let helper_source = helper_file.body.join("\n");

        let initialize_index = helper_source
            .find("initializeConstructor();")
            .expect("entrypoint side effect should be emitted");
        let command_index = helper_source
            .find("var command = new Constructor();")
            .expect("later runtime declaration should be emitted");
        assert!(initialize_index < command_index);
        assert!(helper_source.contains("function main()"));
        assert!(helper_source.contains("export { main };"));
    }

    #[test]
    fn entrypoint_runtime_preserves_namespace_export_order_before_tail_side_effects() {
        let planner = ImportExportPlanner;
        let prelude = concat!(
            "function zString() { return 'schema'; }\n",
            "var m = {};\n",
            "M5(m, { string: () => zString });\n",
        );
        let body = "export const value = 1;\n";
        let tail = concat!(
            "initializeSchemas();\n",
            "function initializeSchemas() { if (typeof m.string !== 'function') throw Error('missing zod string'); }\n",
            "function main() { return m.string(); }\n",
            "main();\n",
        );
        let source = format!("{prelude}{body}{tail}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + body.len()) as u32,
                )),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let helper_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
            .expect("runtime helper file should be planned");
        let helper_source = helper_file.body.join("\n");

        let namespace_index = helper_source
            .find("Object.defineProperties(m")
            .expect("namespace export should be emitted");
        let side_effect_index = helper_source
            .find("initializeSchemas();")
            .expect("entrypoint side effect should be emitted");
        assert!(
            namespace_index < side_effect_index,
            "namespace export must precede tail side effects that read it, got:\n{helper_source}"
        );
        assert!(helper_source.contains("function main()"));
        assert!(helper_source.contains("export { main };"));
    }

    #[test]
    fn contextual_identifier_as_is_kept_as_runtime_dependency() {
        let planner = ImportExportPlanner;
        let prelude = "var as = { command() { return this; } };\nfunction main() { return as.command('run'); }\n";
        let body = "export const value = 1;\n";
        let tail = "main();\n";
        let source = format!("{prelude}{body}{tail}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + body.len()) as u32,
                )),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let helper_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
            .expect("runtime helper file should be planned");
        let helper_source = helper_file.body.join("\n");

        assert!(helper_source.contains("var as = { command() { return this; } };"));
        assert!(helper_source.contains("function main()"));
        assert!(helper_source.contains("as.command('run')"));
        assert!(helper_source.contains("export { main };"));
    }

    #[test]
    fn entrypoint_runtime_imports_source_required_package_bindings() {
        let planner = ImportExportPlanner;
        let prelude = "function main() { return packageInit(); }\n";
        let body = "var value = packageInit();\nexport { value };\n";
        let tail = "packageInit();\nmain();\n";
        let source = format!("{prelude}{body}{tail}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.source_files.push(SourceFileInput::new(
            2,
            "package.js",
            Some("function packageInit() { return 1; }".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::package(
                ModuleId(2),
                "package",
                "modules/package.ts",
                "fixture-package",
                Some("1.0.0".to_string()),
            )
            .with_source_file(2),
        );
        rows.package_attributions
            .push(PackageAttributionInput::accepted_external(
                ModuleId(2),
                "fixture-package",
                "1.0.0",
                "fixture-package",
            ));
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(1),
            target: ModuleDependencyTarget::Module(ModuleId(2)),
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let cli_file = plan
            .files
            .iter()
            .find(|file| file.path == "cli.ts")
            .expect("cli entrypoint should be planned");
        let helper_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
            .expect("runtime helper file should be planned");
        let package_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/package.ts")
            .expect("source-required package file should be emitted");
        let cli_source = cli_file.body.join("\n");
        let helper_source = helper_file.body.join("\n");

        assert!(
            cli_source.contains("import { main } from './modules/runtime/source-1-helpers.js';")
        );
        assert!(helper_source.contains("import { packageInit } from '../package.js';"));
        assert!(helper_source.contains("packageInit();"));
        assert!(
            package_file
                .body
                .join("\n")
                .contains("export { packageInit };")
        );
    }

    #[test]
    fn source_required_commonjs_package_module_uses_external_adapter() {
        let planner = ImportExportPlanner;
        let app_source = "var value = packageInit().answer;\nexport { value };\n";
        let package_source = r#"
            var packageInit = (() => {
                let _$cached;
                return () => {
                    if (_$cached) return _$cached.exports;
                    var _$module = _$cached = { exports: {} };
                    ((exports) => { exports.answer = 42; })(_$module.exports, _$module);
                    return _$module.exports;
                };
            })();
            export { packageInit };
        "#;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "app.js",
            Some(app_source.to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "package.js",
            Some(package_source.to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
        );
        rows.modules.push(
            ModuleInput::package(
                ModuleId(2),
                "package",
                "modules/package.ts",
                "fixture-package",
                Some("1.0.0".to_string()),
            )
            .with_source_file(2),
        );
        rows.package_attributions
            .push(PackageAttributionInput::accepted_external(
                ModuleId(2),
                "fixture-package",
                "1.0.0",
                "fixture-package",
            ));
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(1),
            target: ModuleDependencyTarget::Module(ModuleId(2)),
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let package_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/package.ts")
            .expect("adapter package file should be emitted");
        let source = package_file.body.join("\n");

        assert_eq!(package_file.imports.len(), 1);
        assert_eq!(
            package_file.imports[0].resolution.specifier(),
            Some("fixture-package")
        );
        assert!(source.contains(
            "function packageInit() { return Object.prototype.hasOwnProperty.call(external_fixture_package, \"default\") ? external_fixture_package.default : external_fixture_package; }"
        ));
        assert!(source.contains("export { packageInit };"));
        assert!(!source.contains("_$cached"));
    }

    #[test]
    fn entrypoint_runtime_drops_noop_runtime_side_effects() {
        let planner = ImportExportPlanner;
        let prelude = "var noop = () => {};\nfunction main() { return 1; }\n";
        let body = "export const value = 1;\n";
        let tail = "noop();\nmain();\n";
        let source = format!("{prelude}{body}{tail}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + body.len()) as u32,
                )),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let cli_file = plan
            .files
            .iter()
            .find(|file| file.path == "cli.ts")
            .expect("cli entrypoint should be planned");
        let helper_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
            .expect("runtime helper file should be planned");
        let cli_source = cli_file.body.join("\n");
        let helper_source = helper_file.body.join("\n");

        assert!(!cli_source.contains("noop();"));
        assert!(!cli_source.contains("var noop"));
        assert!(!cli_source.contains("function main()"));
        assert!(!helper_source.contains("noop();"));
        assert!(!helper_source.contains("var noop"));
        assert!(helper_source.contains("function main()"));
    }

    #[test]
    fn entrypoint_runtime_keeps_non_noop_runtime_side_effect_dependencies() {
        let planner = ImportExportPlanner;
        let prelude =
            "var setup = () => { globalThis.ready = true; };\nfunction main() { return 1; }\n";
        let body = "export const value = 1;\n";
        let tail = "setup();\nmain();\n";
        let source = format!("{prelude}{body}{tail}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + body.len()) as u32,
                )),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let cli_file = plan
            .files
            .iter()
            .find(|file| file.path == "cli.ts")
            .expect("cli entrypoint should be planned");
        let helper_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
            .expect("runtime helper file should be planned");
        let cli_source = cli_file.body.join("\n");
        let helper_source = helper_file.body.join("\n");

        assert!(!cli_source.contains("var setup"));
        assert!(!cli_source.contains("setup();"));
        assert!(helper_source.contains("var setup"));
        assert!(helper_source.contains("setup();"));
        assert!(helper_source.contains("globalThis.ready = true"));
    }

    #[test]
    fn runtime_import_identifier_scan_ignores_globals_and_property_names() {
        let identifiers = super::runtime_import_identifiers_in_source(
            "function local() { function nested() {} }\n\
             let { isReady: localAlias } = source;\n\
             class Transport { buffer = Buffer.alloc(0); isBridge; async start() {} stop() {} }\n\
             const reader = (event) => event.ready;\n\
             const copy = [...shared];\n\
             as.command('servers');\n\
             console.log(request.method); Promise.resolve().then(() => (packageInit(), ns));",
        );

        assert!(identifiers.contains("as"));
        assert!(identifiers.contains("packageInit"));
        assert!(identifiers.contains("request"));
        assert!(identifiers.contains("ns"));
        assert!(identifiers.contains("source"));
        assert!(identifiers.contains("shared"));
        assert!(!identifiers.contains("console"));
        assert!(!identifiers.contains("log"));
        assert!(!identifiers.contains("method"));
        assert!(!identifiers.contains("Promise"));
        assert!(!identifiers.contains("resolve"));
        assert!(!identifiers.contains("then"));
        assert!(!identifiers.contains("local"));
        assert!(!identifiers.contains("nested"));
        assert!(!identifiers.contains("localAlias"));
        assert!(!identifiers.contains("buffer"));
        assert!(!identifiers.contains("isBridge"));
        assert!(!identifiers.contains("event"));
        assert!(!identifiers.contains("start"));
        assert!(!identifiers.contains("stop"));
    }

    #[test]
    fn runtime_import_identifier_scan_ignores_setter_class_expression_name() {
        let identifiers = super::runtime_import_identifiers_in_source(
            "var init = lazyValue(() => { __reverts_set_$F(class $F extends qP8 { method() { return qP8; } }); });",
        );

        assert!(identifiers.contains("__reverts_set_$F"));
        assert!(identifiers.contains("qP8"));
        assert!(!identifiers.contains("$F"));
    }

    #[test]
    fn runtime_import_identifier_scan_keeps_later_read_after_setter_class_expression() {
        let identifiers = super::runtime_import_identifiers_in_source(
            "var init = lazyValue(() => { __reverts_set_kr6(class kr6 {}); az.Hooks = kr6; });",
        );

        assert!(identifiers.contains("__reverts_set_kr6"));
        assert!(identifiers.contains("az"));
        assert!(identifiers.contains("kr6"));
    }

    #[test]
    fn call_identifier_scan_keeps_direct_initializer_calls_with_local_name_collision() {
        let identifiers = super::call_identifiers_in_source(
            "const local = ({ e3 }) => e3;\n\
             e3();\n\
             object.e3();\n\
             function e3Local() {}\n",
        );

        assert!(identifiers.contains("e3"));
        assert!(!identifiers.contains("e3Local"));
    }

    #[test]
    fn class_fields_are_not_implicit_global_writes() {
        let writes = super::implicit_global_writes_in_source(
            "class Transport { isBridge = false; ready; method() { this.ready = true; } }\n\
             shared = 1;",
        );

        assert!(writes.contains(&BindingName::new("shared")));
        assert!(!writes.contains(&BindingName::new("isBridge")));
        assert!(!writes.contains(&BindingName::new("ready")));
    }

    #[test]
    fn runtime_helper_closure_keeps_recursive_prelude_dependencies() {
        let planner = ImportExportPlanner;
        let prelude = concat!(
            "function first() { return second(); }\n",
            "function second() { return third(); }\n",
            "function third() { return 3; }\n",
        );
        let body = "var value = first();\nexport { value };\n";
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let helper_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
            .expect("runtime helper file should be planned");
        let helper_source = helper_file.body.join("\n");

        assert!(helper_source.contains("function first()"));
        assert!(helper_source.contains("function second()"));
        assert!(helper_source.contains("function third()"));
        assert!(helper_source.contains("export { first };"));
    }

    #[test]
    fn lazy_folded_source_keeps_prelude_dependencies_used_by_folded_chunk() {
        let planner = ImportExportPlanner;
        let prelude = concat!(
            "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
            "var shared;\n",
            "function buildShared() { return 42; }\n",
        );
        let body = concat!(
            "var initShared = lazy(() => { shared = buildShared(); });\n",
            "export { initShared, shared };\n",
        );
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let helper_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
            .expect("runtime helper file should be planned");
        let helper_source = helper_file.body.join("\n");

        assert!(helper_source.contains("function buildShared()"));
        assert!(helper_source.contains("shared = buildShared();"));
        assert!(helper_source.contains("var initShared = lazyValue(() => {"));
        assert!(helper_source.contains("function lazyValue(factory) {"));
        assert!(helper_source.contains("export { initShared, shared };"));
    }

    #[test]
    fn runtime_helper_self_audit_rejects_unresolved_references() {
        let prelude = RuntimePrelude {
            source_file_id: 1,
            source_file_path: "bundle.js".to_string(),
            source: String::new(),
            bindings: BTreeMap::from([(
                BindingName::new("missingHelper"),
                RuntimePreludeBindingKind::SourceBacked,
            )]),
            snippets: BTreeMap::new(),
            namespace_exports: Vec::new(),
            entrypoint: None,
        };
        let unresolved = super::unresolved_runtime_helper_references(
            &prelude,
            "function main() { return missingHelper(); }\n",
            &BTreeSet::new(),
            &BTreeMap::new(),
        );

        assert_eq!(
            unresolved,
            BTreeSet::from([BindingName::new("missingHelper")])
        );
    }

    #[test]
    fn module_dependencies_import_unresolved_source_bindings() {
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "entry.js",
            Some("var value = (() => helper())();\nexport { value };".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "helper.js",
            Some("function helper() { return 1; }".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
        );
        rows.modules.push(
            ModuleInput::package(
                ModuleId(2),
                "helper",
                "modules/helper.ts",
                "fixture-helper",
                Some("1.0.0".to_string()),
            )
            .with_source_file(2),
        );
        rows.package_attributions
            .push(PackageAttributionInput::rejected_source(
                ModuleId(2),
                "fixture-helper",
                "fixture helper stays source-backed in planner fixture",
            ));
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(1),
            target: ModuleDependencyTarget::Module(ModuleId(2)),
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let entry_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/entry.ts")
            .expect("entry file should be planned");
        let helper_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/helper.ts")
            .expect("helper file should be planned");

        assert!(
            entry_file
                .body
                .join("\n")
                .contains("import { helper } from './helper.js';")
        );
        assert!(helper_file.body.join("\n").contains("export { helper };"));
    }

    #[test]
    fn accepted_external_package_with_source_read_is_emitted_locally() {
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "entry.js",
            Some("packageInit();\nexport const value = 1;".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "package.js",
            Some("function packageInit() { return 1; }".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
        );
        rows.modules.push(
            ModuleInput::package(
                ModuleId(2),
                "package",
                "modules/package.ts",
                "fixture-package",
                Some("1.0.0".to_string()),
            )
            .with_source_file(2),
        );
        rows.package_attributions
            .push(PackageAttributionInput::accepted_external(
                ModuleId(2),
                "fixture-package",
                "1.0.0",
                "fixture-package",
            ));
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(1),
            target: ModuleDependencyTarget::Module(ModuleId(2)),
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let entry_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/entry.ts")
            .expect("entry file should be planned");
        let package_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/package.ts")
            .expect("source-read package file should be emitted");

        assert!(
            entry_file
                .body
                .join("\n")
                .contains("import { packageInit } from './package.js';")
        );
        assert!(
            package_file
                .body
                .join("\n")
                .contains("export { packageInit };")
        );
    }

    #[test]
    fn accepted_external_package_read_from_runtime_helper_is_emitted_locally() {
        let planner = ImportExportPlanner;
        let package_source = "var dNq = { default: () => 1 };\nvar cNq = lazyValue(() => dNq);\n";
        let app_source = "var app = lazyValue(() => (cNq(), dNq).default());\n";
        let entrypoint = "app();\n";
        let source = format!("{package_source}{app_source}{entrypoint}");
        let package_start = 0;
        let package_end = package_source.len() as u32;
        let app_start = package_end;
        let app_end = package_end + app_source.len() as u32;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source)));
        rows.modules.push(
            ModuleInput::package(
                ModuleId(1),
                "open",
                "modules/open.ts",
                "open",
                Some("10.2.0".to_string()),
            )
            .with_source_file(1)
            .with_source_span(SourceSpan::new(package_start, package_end)),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(app_start, app_end)),
        );
        rows.package_attributions
            .push(PackageAttributionInput::accepted_external(
                ModuleId(1),
                "open",
                "10.2.0",
                "open/index.js",
            ));
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let accepted_externalized_packages = super::externalized_package_modules(&enriched);
        let source_facts = super::SourceModuleFacts::from_program(&enriched);
        let source_required_packages = super::source_required_package_modules(
            &enriched,
            &accepted_externalized_packages,
            &source_facts,
        );
        assert!(
            source_required_packages.contains(&ModuleId(1)),
            "package bindings read by source without an import edge must stay local"
        );
        let init_shims = super::externalized_package_init_shims(
            &enriched,
            "await (cNq(), dNq).default();",
            &accepted_externalized_packages,
        );
        assert!(init_shims.contains(&BindingName::new("cNq")));
        assert!(!init_shims.contains(&BindingName::new("dNq")));

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("runtime helper package reads should be wired");

        let package_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/open.ts")
            .expect("runtime-read package file should be emitted locally");
        assert!(package_file.body.join("\n").contains("var cNq = lazyValue"));
    }

    #[test]
    fn source_module_import_takes_precedence_over_same_named_runtime_helper() {
        let planner = ImportExportPlanner;
        let prelude = "function shared() { return 0; }\n";
        let body = "var value = shared();\nexport { value };\n";
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.source_files.push(SourceFileInput::new(
            2,
            "shared.js",
            Some("function shared() { return 2; }\n".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "shared", "modules/shared.ts")
                .with_source_file(2),
        );
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(1),
            target: ModuleDependencyTarget::Module(ModuleId(2)),
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let entry_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/entry.ts")
            .expect("entry file should be planned");
        let entry_source = entry_file.body.join("\n");

        assert!(entry_source.contains("import { shared } from './shared.js';"));
        assert!(!entry_source.contains("source-1-prelude"));
        assert!(!entry_source.contains("source-1-helpers"));
        assert!(
            plan.files
                .iter()
                .all(|file| file.path != "modules/runtime/source-1-helpers.ts")
        );
    }

    #[test]
    fn single_reader_runtime_var_migration_moves_reader_with_writer() {
        let prelude = concat!(
            "var shared;\n",
            "var suffix = '!';\n",
            "function formatShared() { return shared + suffix; }\n",
        );
        let writer_body = "shared = 'ok';\nexport { shared };\n";
        let consumer_body = "var value = formatShared();\nexport { value };\n";
        let source = format!("{prelude}{writer_body}{consumer_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + writer_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + writer_body.len()) as u32,
                    source.len() as u32,
                )),
        );

        let plan = plan_from_rows(rows);
        let writer_source = planned_source(&plan, "modules/writer.ts");
        let consumer_source = planned_source(&plan, "modules/consumer.ts");
        let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

        assert!(writer_source.contains("import { suffix } from './runtime/source-1-helpers.js';"));
        assert!(writer_source.contains("var shared;"));
        assert!(writer_source.contains("function formatShared() { return shared + suffix; }"));
        assert!(writer_source.contains("shared = 'ok';"));
        assert!(writer_source.contains("export { formatShared };"));
        assert!(!writer_source.contains("__reverts_set_shared"));
        assert!(consumer_source.contains("import { formatShared } from './writer.js';"));
        assert!(!consumer_source.contains("source-1-helpers"));
        assert!(helper_source.contains("var suffix = '!';"));
        assert!(!helper_source.contains("var shared;"));
        assert!(!helper_source.contains("function formatShared()"));
        assert!(!helper_source.contains("__reverts_set_shared"));
        assert!(helper_source.contains("export { suffix };"));
        assert!(!helper_source.contains("from '../writer.js';"));
    }

    #[test]
    fn single_reader_runtime_var_migration_handles_reader_without_runtime_deps() {
        let prelude = "var shared;\nfunction getShared() { return shared; }\n";
        let writer_body = "shared = 1;\nexport { shared };\n";
        let consumer_body = "var value = getShared();\nexport { value };\n";
        let source = format!("{prelude}{writer_body}{consumer_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + writer_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + writer_body.len()) as u32,
                    source.len() as u32,
                )),
        );

        let plan = plan_from_rows(rows);
        let writer_source = planned_source(&plan, "modules/writer.ts");
        let consumer_source = planned_source(&plan, "modules/consumer.ts");

        assert!(!writer_source.contains("source-1-helpers"));
        assert!(writer_source.contains("var shared;"));
        assert!(writer_source.contains("function getShared() { return shared; }"));
        assert!(writer_source.contains("shared = 1;"));
        assert!(writer_source.contains("export { getShared };"));
        assert!(consumer_source.contains("import { getShared } from './writer.js';"));
        assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
    }

    #[test]
    fn reader_cluster_runtime_var_migration_moves_arrow_reader_with_writer() {
        let prelude = "var shared;\nvar getShared = () => shared;\n";
        let writer_body = "shared = 1;\nexport { shared };\n";
        let consumer_body = "var value = getShared();\nexport { value };\n";
        let source = format!("{prelude}{writer_body}{consumer_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + writer_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + writer_body.len()) as u32,
                    source.len() as u32,
                )),
        );

        let plan = plan_from_rows(rows);
        let writer_source = planned_source(&plan, "modules/writer.ts");
        let consumer_source = planned_source(&plan, "modules/consumer.ts");

        assert!(
            !writer_source.contains("source-1-helpers"),
            "{writer_source}"
        );
        assert!(writer_source.contains("var shared;"), "{writer_source}");
        assert!(writer_source.contains("var getShared = () => shared;"));
        assert!(writer_source.contains("shared = 1;"));
        assert!(writer_source.contains("export { getShared };"));
        assert!(consumer_source.contains("import { getShared } from './writer.js';"));
        assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
    }

    #[test]
    fn reader_cluster_runtime_var_migration_moves_class_reader_with_writer() {
        let prelude = concat!(
            "var shared;\n",
            "class ReadsShared { value() { return shared; } }\n",
        );
        let writer_body = "shared = 1;\nexport { shared };\n";
        let consumer_body = "var value = new ReadsShared().value();\nexport { value };\n";
        let source = format!("{prelude}{writer_body}{consumer_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + writer_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + writer_body.len()) as u32,
                    source.len() as u32,
                )),
        );

        let plan = plan_from_rows(rows);
        let writer_source = planned_source(&plan, "modules/writer.ts");
        let consumer_source = planned_source(&plan, "modules/consumer.ts");

        assert!(
            !writer_source.contains("source-1-helpers"),
            "{writer_source}"
        );
        assert!(writer_source.contains("var shared;"), "{writer_source}");
        assert!(writer_source.contains("class ReadsShared { value() { return shared; } }"));
        assert!(writer_source.contains("shared = 1;"));
        assert!(writer_source.contains("export { ReadsShared };"));
        assert!(consumer_source.contains("import { ReadsShared } from './writer.js';"));
        assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
    }

    #[test]
    fn reader_cluster_runtime_var_migration_moves_static_method_class_reader_with_writer() {
        let prelude = concat!(
            "var shared;\n",
            "class ReadsShared { static ready = false; static value() { return shared; } }\n",
        );
        let writer_body = "shared = 1;\nexport { shared };\n";
        let consumer_body = "var value = ReadsShared.value();\nexport { value };\n";
        let source = format!("{prelude}{writer_body}{consumer_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + writer_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + writer_body.len()) as u32,
                    source.len() as u32,
                )),
        );

        let plan = plan_from_rows(rows);
        let writer_source = planned_source(&plan, "modules/writer.ts");
        let consumer_source = planned_source(&plan, "modules/consumer.ts");

        assert!(
            !writer_source.contains("source-1-helpers"),
            "{writer_source}"
        );
        assert!(writer_source.contains("var shared;"), "{writer_source}");
        assert!(
            writer_source.contains(
                "class ReadsShared { static ready = false; static value() { return shared; } }"
            ),
            "{writer_source}"
        );
        assert!(writer_source.contains("shared = 1;"));
        assert!(writer_source.contains("export { ReadsShared };"));
        assert!(consumer_source.contains("import { ReadsShared } from './writer.js';"));
        assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
    }

    #[test]
    fn reader_cluster_runtime_var_migration_rejects_static_class_reader() {
        let prelude = concat!(
            "var shared;\n",
            "class ReadsShared { static value = shared; }\n",
        );
        let writer_body = "shared = 1;\nexport { shared };\n";
        let consumer_body = "var value = ReadsShared.value;\nexport { value };\n";
        let source = format!("{prelude}{writer_body}{consumer_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + writer_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + writer_body.len()) as u32,
                    source.len() as u32,
                )),
        );

        let plan = plan_from_rows(rows);
        let writer_source = planned_source(&plan, "modules/writer.ts");
        let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

        assert!(writer_source.contains("__reverts_set_shared(1);"));
        assert!(!writer_source.contains("class ReadsShared"));
        assert!(helper_source.contains("class ReadsShared { static value = shared; }"));
        assert!(
            helper_source
                .contains("function __reverts_set_shared(value) { shared = value; return value; }")
        );
    }

    #[test]
    fn reader_cluster_runtime_var_migration_rejects_static_block_class_reader() {
        let prelude = concat!(
            "var shared;\n",
            "class ReadsShared { static { this.value = shared; } }\n",
        );
        let writer_body = "shared = 1;\nexport { shared };\n";
        let consumer_body = "var value = ReadsShared.value;\nexport { value };\n";
        let source = format!("{prelude}{writer_body}{consumer_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + writer_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + writer_body.len()) as u32,
                    source.len() as u32,
                )),
        );

        let plan = plan_from_rows(rows);
        let writer_source = planned_source(&plan, "modules/writer.ts");
        let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

        assert!(writer_source.contains("__reverts_set_shared(1);"));
        assert!(!writer_source.contains("class ReadsShared"));
        assert!(helper_source.contains("class ReadsShared { static { this.value = shared; } }"));
        assert!(
            helper_source
                .contains("function __reverts_set_shared(value) { shared = value; return value; }")
        );
    }

    #[test]
    fn reader_cluster_runtime_var_migration_rejects_computed_class_reader_key() {
        let prelude = concat!(
            "var shared;\n",
            "class ReadsShared { [shared]() { return 1; } }\n",
        );
        let writer_body = "shared = 'value';\nexport { shared };\n";
        let consumer_body = "var value = new ReadsShared()[shared]();\nexport { value };\n";
        let source = format!("{prelude}{writer_body}{consumer_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + writer_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + writer_body.len()) as u32,
                    source.len() as u32,
                )),
        );

        let plan = plan_from_rows(rows);
        let writer_source = planned_source(&plan, "modules/writer.ts");
        let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

        assert!(writer_source.contains("__reverts_set_shared('value');"));
        assert!(!writer_source.contains("class ReadsShared"));
        assert!(helper_source.contains("class ReadsShared { [shared]() { return 1; } }"));
        assert!(
            helper_source
                .contains("function __reverts_set_shared(value) { shared = value; return value; }")
        );
    }

    #[test]
    fn reader_cluster_runtime_var_migration_moves_multiple_readers() {
        let prelude = concat!(
            "var shared;\n",
            "function first() { return shared; }\n",
            "function second() { return shared; }\n",
        );
        let writer_body = "shared = 1;\nexport { shared };\n";
        let consumer_body = "var value = first();\nexport { value };\n";
        let source = format!("{prelude}{writer_body}{consumer_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + writer_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + writer_body.len()) as u32,
                    source.len() as u32,
                )),
        );

        let plan = plan_from_rows(rows);
        let writer_source = planned_source(&plan, "modules/writer.ts");
        let consumer_source = planned_source(&plan, "modules/consumer.ts");

        assert!(!writer_source.contains("source-1-helpers"));
        assert!(writer_source.contains("var shared;"));
        assert!(writer_source.contains("function first() { return shared; }"));
        assert!(writer_source.contains("function second() { return shared; }"));
        assert!(writer_source.contains("shared = 1;"));
        assert!(writer_source.contains("first"));
        assert!(writer_source.contains("second"));
        assert!(consumer_source.contains("import { first } from './writer.js';"));
        assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
    }

    #[test]
    fn reader_cluster_runtime_var_migration_moves_dependent_reader_chain() {
        let prelude = concat!(
            "var shared;\n",
            "function rawShared() { return shared; }\n",
            "function decoratedShared() { return rawShared() + '!'; }\n",
        );
        let writer_body = "shared = 'ok';\nexport { shared };\n";
        let consumer_body = "var value = decoratedShared();\nexport { value };\n";
        let source = format!("{prelude}{writer_body}{consumer_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + writer_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + writer_body.len()) as u32,
                    source.len() as u32,
                )),
        );

        let plan = plan_from_rows(rows);
        let writer_source = planned_source(&plan, "modules/writer.ts");
        let consumer_source = planned_source(&plan, "modules/consumer.ts");

        assert!(writer_source.contains("function rawShared() { return shared; }"));
        assert!(writer_source.contains("function decoratedShared() { return rawShared() + '!'; }"));
        assert!(writer_source.contains("decoratedShared"));
        assert!(writer_source.contains("rawShared"));
        assert!(consumer_source.contains("import { decoratedShared } from './writer.js';"));
        assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
    }

    #[test]
    fn reader_cluster_runtime_var_migration_allows_owner_local_source_dependency() {
        let prelude = concat!(
            "var shared;\n",
            "function decoratedShared() { return decorate(shared); }\n",
        );
        let writer_body = concat!(
            "function decorate(value) { return `${value}!`; }\n",
            "shared = 'ok';\n",
            "export { shared };\n",
        );
        let consumer_body = "var value = decoratedShared();\nexport { value };\n";
        let source = format!("{prelude}{writer_body}{consumer_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + writer_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + writer_body.len()) as u32,
                    source.len() as u32,
                )),
        );

        let plan = plan_from_rows(rows);
        let writer_source = planned_source(&plan, "modules/writer.ts");
        let consumer_source = planned_source(&plan, "modules/consumer.ts");

        assert!(
            !writer_source.contains("source-1-helpers"),
            "{writer_source}"
        );
        assert!(writer_source.contains("var shared;"), "{writer_source}");
        assert!(
            writer_source.contains("function decoratedShared() { return decorate(shared); }"),
            "{writer_source}"
        );
        assert!(
            writer_source.contains("function decorate(value)"),
            "{writer_source}"
        );
        assert!(
            writer_source.contains("return `${value}!`;"),
            "{writer_source}"
        );
        assert!(writer_source.contains("shared = 'ok';"), "{writer_source}");
        assert!(writer_source.contains("export { decoratedShared };"));
        assert!(consumer_source.contains("import { decoratedShared } from './writer.js';"));
        assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
    }

    #[test]
    fn reader_cluster_runtime_var_migration_imports_declared_source_dependency() {
        let prelude = concat!(
            "var shared;\n",
            "function decoratedShared() { return decorate(shared); }\n",
        );
        let writer_body = "shared = 'ok';\nexport { shared };\n";
        let helper_body = "function decorate(value) { return `${value}!`; }\n";
        let consumer_body = "var value = decoratedShared();\nexport { value };\n";
        let source = format!("{prelude}{writer_body}{helper_body}{consumer_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + writer_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "helper", "modules/helper.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + writer_body.len()) as u32,
                    (prelude.len() + writer_body.len() + helper_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + writer_body.len() + helper_body.len()) as u32,
                    source.len() as u32,
                )),
        );
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(1),
            target: ModuleDependencyTarget::Module(ModuleId(2)),
        });

        let plan = plan_from_rows(rows);
        let writer_source = planned_source(&plan, "modules/writer.ts");
        let helper_source = planned_source(&plan, "modules/helper.ts");
        let consumer_source = planned_source(&plan, "modules/consumer.ts");

        assert!(
            writer_source.contains("import { decorate } from './helper.js';"),
            "{writer_source}"
        );
        assert!(
            writer_source.contains("function decoratedShared() { return decorate(shared); }"),
            "{writer_source}"
        );
        assert!(writer_source.contains("shared = 'ok';"), "{writer_source}");
        assert!(
            helper_source.contains("export { decorate };"),
            "{helper_source}"
        );
        assert!(consumer_source.contains("import { decoratedShared } from './writer.js';"));
        assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
    }

    #[test]
    fn namespace_getter_runtime_var_migration_moves_namespace_with_writer() {
        let prelude = concat!(
            "var shared;\n",
            "var ns = {};\n",
            "function expose(target, exports) {}\n",
            "expose(ns, { value: () => shared });\n",
            "function readShared() { return ns.value; }\n",
        );
        let writer_body = "shared = 'ok';\nexport { shared };\n";
        let consumer_body = "var value = readShared() + ns.value;\nexport { value };\n";
        let source = format!("{prelude}{writer_body}{consumer_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + writer_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + writer_body.len()) as u32,
                    source.len() as u32,
                )),
        );

        let plan = plan_from_rows(rows);
        let writer_source = planned_source(&plan, "modules/writer.ts");
        let consumer_source = planned_source(&plan, "modules/consumer.ts");

        assert!(!writer_source.contains("source-1-helpers"));
        assert!(writer_source.contains("var shared;"));
        assert!(writer_source.contains("var ns = {};"));
        assert!(writer_source.contains(
            "Object.defineProperties(ns, { value: { enumerable: true, get: () => shared } });"
        ));
        assert!(writer_source.contains("function readShared() { return ns.value; }"));
        assert!(writer_source.contains("shared = 'ok';"));
        assert!(writer_source.contains("export { ns, readShared };"));
        assert!(
            consumer_source.contains("import { readShared, shared } from './writer.js';"),
            "{consumer_source}"
        );
        assert!(consumer_source.contains("var value = readShared() + shared;"));
        assert!(!consumer_source.contains("ns.value"));
        let helper_source = planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts");
        assert!(
            helper_source.is_none(),
            "writer:\n{writer_source}\nconsumer:\n{consumer_source}\nhelper:\n{helper_source:?}"
        );
    }

    #[test]
    fn reader_cluster_runtime_var_migration_moves_same_writer_component() {
        let prelude = "var left;\nvar right;\nfunction pair() { return [left, right]; }\n";
        let writer_body = "left = 1;\nright = 2;\nexport { left, right };\n";
        let consumer_body = "var value = pair();\nexport { value };\n";
        let source = format!("{prelude}{writer_body}{consumer_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + writer_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + writer_body.len()) as u32,
                    source.len() as u32,
                )),
        );

        let plan = plan_from_rows(rows);
        let writer_source = planned_source(&plan, "modules/writer.ts");
        let consumer_source = planned_source(&plan, "modules/consumer.ts");

        assert!(
            !writer_source.contains("source-1-helpers"),
            "{writer_source}"
        );
        assert!(
            writer_source.contains("var left, right;") || writer_source.contains("var left;"),
            "{writer_source}"
        );
        assert!(writer_source.contains("function pair() { return [left, right]; }"));
        assert!(writer_source.contains("left = 1;"));
        assert!(writer_source.contains("right = 2;"));
        assert!(writer_source.contains("export { pair };"));
        assert!(consumer_source.contains("import { pair } from './writer.js';"));
        assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
    }

    #[test]
    fn reader_cluster_runtime_var_migration_rejects_cross_writer_component() {
        let prelude = "var left;\nvar right;\nfunction pair() { return [left, right]; }\n";
        let left_writer_body = "left = 1;\nexport { left };\n";
        let right_writer_body = "right = 2;\nexport { right };\n";
        let consumer_body = "var value = pair();\nexport { value };\n";
        let source = format!("{prelude}{left_writer_body}{right_writer_body}{consumer_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "left-writer", "modules/left-writer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + left_writer_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "right-writer", "modules/right-writer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + left_writer_body.len()) as u32,
                    (prelude.len() + left_writer_body.len() + right_writer_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + left_writer_body.len() + right_writer_body.len()) as u32,
                    source.len() as u32,
                )),
        );

        let plan = plan_from_rows(rows);
        let left_writer_source = planned_source(&plan, "modules/left-writer.ts");
        let right_writer_source = planned_source(&plan, "modules/right-writer.ts");
        let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

        assert!(left_writer_source.contains("__reverts_set_left(1);"));
        assert!(right_writer_source.contains("__reverts_set_right(2);"));
        assert!(!left_writer_source.contains("function pair()"));
        assert!(!right_writer_source.contains("function pair()"));
        assert!(helper_source.contains("function pair() { return [left, right]; }"));
        assert!(
            helper_source
                .contains("function __reverts_set_left(value) { left = value; return value; }")
        );
        assert!(
            helper_source
                .contains("function __reverts_set_right(value) { right = value; return value; }")
        );
    }

    #[test]
    fn runtime_setter_migration_blocker_report_counts_gate_reasons_without_changing_gate() {
        let prelude = concat!(
            "var accepted;\n",
            "var left;\n",
            "var right;\n",
            "function pair() { return [left, right]; }\n",
            "var complex = makeValue();\n",
            "var multi;\n",
        );
        let writer_body = concat!(
            "accepted = 1;\n",
            "left = 2;\n",
            "right = 3;\n",
            "complex = 4;\n",
            "multi = 5;\n",
            "export { accepted, left, right, complex, multi };\n",
        );
        let second_writer_body = "multi = 6;\nexport { multi };\n";
        let consumer_body = "var value = pair();\nexport { value };\n";
        let source = format!("{prelude}{writer_body}{second_writer_body}{consumer_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + writer_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "second-writer", "modules/second-writer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + writer_body.len()) as u32,
                    (prelude.len() + writer_body.len() + second_writer_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + writer_body.len() + second_writer_body.len()) as u32,
                    source.len() as u32,
                )),
        );

        let enriched = enriched_from_rows(rows);
        let report = ImportExportPlanner.runtime_setter_migration_blocker_report(&enriched);
        let plan = ImportExportPlanner
            .plan_enriched_program(&enriched)
            .expect("plan should still be valid");
        let writer_source = planned_source(&plan, "modules/writer.ts");
        let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

        assert_eq!(report.total_bindings, 5);
        assert_eq!(report.accepted_bindings, 3);
        assert_eq!(report.blocked_bindings, 2);
        assert_eq!(
            report
                .reasons
                .get(&RuntimeSetterMigrationBlockerReason::ReaderReadsOtherMovableBinding),
            None
        );
        assert_eq!(
            report
                .reasons
                .get(&RuntimeSetterMigrationBlockerReason::InitializerNotMigratable),
            Some(&1)
        );
        assert_eq!(
            report
                .reasons
                .get(&RuntimeSetterMigrationBlockerReason::MultipleEligibleWriters),
            Some(&1)
        );
        assert!(
            writer_source.contains("var accepted, left, right;")
                || writer_source.contains("var accepted;"),
            "{writer_source}"
        );
        assert!(writer_source.contains("accepted = 1;"), "{writer_source}");
        assert!(writer_source.contains("function pair() { return [left, right]; }"));
        assert!(writer_source.contains("left = 2;"), "{writer_source}");
        assert!(writer_source.contains("right = 3;"), "{writer_source}");
        assert!(
            !helper_source.contains("__reverts_set_left"),
            "{helper_source}"
        );
        assert!(
            !helper_source.contains("__reverts_set_right"),
            "{helper_source}"
        );
        assert!(
            helper_source.contains(
                "function __reverts_set_complex(value) { complex = value; return value; }"
            ),
            "{helper_source}"
        );
        assert!(
            helper_source
                .contains("function __reverts_set_multi(value) { multi = value; return value; }"),
            "{helper_source}"
        );
    }

    #[test]
    fn namespace_getter_runtime_var_migration_moves_same_writer_export_targets() {
        let prelude = concat!(
            "var left;\n",
            "var right;\n",
            "var ns = {};\n",
            "function expose(target, exports) {}\n",
            "expose(ns, { left: () => left, right: () => right });\n",
        );
        let writer_body = "left = 1;\nright = 2;\nexport { left, right };\n";
        let consumer_body = "var value = ns.left;\nexport { value };\n";
        let source = format!("{prelude}{writer_body}{consumer_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + writer_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + writer_body.len()) as u32,
                    source.len() as u32,
                )),
        );

        let plan = plan_from_rows(rows);
        let writer_source = planned_source(&plan, "modules/writer.ts");
        let consumer_source = planned_source(&plan, "modules/consumer.ts");

        assert!(!writer_source.contains("source-1-helpers"));
        assert!(writer_source.contains("var left, right;") || writer_source.contains("var left;"));
        assert!(writer_source.contains("var ns = {};"));
        assert!(writer_source.contains(
            "Object.defineProperties(ns, { left: { enumerable: true, get: () => left }, right: { enumerable: true, get: () => right } });"
        ));
        assert!(writer_source.contains("left = 1;"));
        assert!(writer_source.contains("right = 2;"));
        assert!(writer_source.contains("export { ns };"));
        assert!(
            consumer_source.contains("import { left } from './writer.js';"),
            "{consumer_source}"
        );
        assert!(consumer_source.contains("var value = left;"));
        assert!(!consumer_source.contains("ns.left"));
        assert!(planned_source_opt(&plan, "modules/runtime/source-1-helpers.ts").is_none());
    }

    #[test]
    fn namespace_getter_runtime_var_migration_rejects_cross_writer_export_target() {
        let prelude = concat!(
            "var left;\n",
            "var right;\n",
            "var ns = {};\n",
            "function expose(target, exports) {}\n",
            "expose(ns, { left: () => left, right: () => right });\n",
        );
        let left_body = "left = 1;\nexport { left };\n";
        let right_body = "right = 2;\nexport { right };\n";
        let consumer_body = "var value = ns.left;\nexport { value };\n";
        let source = format!("{prelude}{left_body}{right_body}{consumer_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "left-writer", "modules/left-writer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + left_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "right-writer", "modules/right-writer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + left_body.len()) as u32,
                    (prelude.len() + left_body.len() + right_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + left_body.len() + right_body.len()) as u32,
                    source.len() as u32,
                )),
        );

        let plan = plan_from_rows(rows);
        let left_source = planned_source(&plan, "modules/left-writer.ts");
        let right_source = planned_source(&plan, "modules/right-writer.ts");
        let consumer_source = planned_source(&plan, "modules/consumer.ts");
        let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

        assert!(left_source.contains("__reverts_set_left"), "{left_source}");
        assert!(
            right_source.contains("__reverts_set_right"),
            "{right_source}"
        );
        assert!(
            consumer_source.contains("import { left } from './runtime/source-1-helpers.js';"),
            "{consumer_source}"
        );
        assert!(consumer_source.contains("var value = left;"));
        assert!(!consumer_source.contains("ns.left"));
        assert!(!helper_source.contains("var ns = {};"), "{helper_source}");
        assert!(
            !helper_source.contains("Object.defineProperties(ns"),
            "{helper_source}"
        );
        assert!(
            helper_source
                .contains("function __reverts_set_left(value) { left = value; return value; }")
        );
        assert!(
            helper_source
                .contains("function __reverts_set_right(value) { right = value; return value; }")
        );
    }

    #[test]
    fn runtime_namespace_member_access_rewrites_to_target_import_and_drops_namespace() {
        let prelude = concat!(
            "var shared = 1;\n",
            "var ns = {};\n",
            "function expose(target, exports) {}\n",
            "expose(ns, { value: () => shared });\n",
        );
        let consumer_body = "var value = ns.value + 1;\nexport { value };\n";
        let source = format!("{prelude}{consumer_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "consumer", "modules/consumer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );

        let plan = plan_from_rows(rows);
        let consumer_source = planned_source(&plan, "modules/consumer.ts");
        let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

        assert!(
            consumer_source.contains("import { shared } from './runtime/source-1-helpers.js';"),
            "{consumer_source}"
        );
        assert!(consumer_source.contains("var value = shared + 1;"));
        assert!(!consumer_source.contains("ns.value"));
        assert!(!consumer_source.contains("import { ns"));
        assert!(helper_source.contains("var shared = 1;"), "{helper_source}");
        assert!(!helper_source.contains("var ns = {};"), "{helper_source}");
        assert!(
            !helper_source.contains("Object.defineProperties(ns"),
            "{helper_source}"
        );
        assert!(
            !helper_source.contains("function expose"),
            "{helper_source}"
        );
    }

    #[test]
    fn runtime_namespace_member_access_keeps_namespace_for_value_use() {
        let prelude = concat!(
            "var shared = 1;\n",
            "var ns = {};\n",
            "function expose(target, exports) {}\n",
            "expose(ns, { value: () => shared });\n",
        );
        let consumer_body = "var value = ns;\nexport { value };\n";
        let source = format!("{prelude}{consumer_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "consumer", "modules/consumer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );

        let plan = plan_from_rows(rows);
        let consumer_source = planned_source(&plan, "modules/consumer.ts");
        let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

        assert!(
            consumer_source.contains("import { ns } from './runtime/source-1-helpers.js';"),
            "{consumer_source}"
        );
        assert!(consumer_source.contains("var value = ns;"));
        assert!(helper_source.contains("var ns = {};"), "{helper_source}");
        assert!(
            helper_source.contains("Object.defineProperties(ns"),
            "{helper_source}"
        );
    }

    #[test]
    fn runtime_namespace_member_access_rejects_local_name_collision() {
        let prelude = concat!(
            "var shared = 1;\n",
            "var ns = {};\n",
            "function expose(target, exports) {}\n",
            "expose(ns, { value: () => shared });\n",
        );
        let consumer_body =
            "var shared = 'local';\nvar value = ns.value + shared;\nexport { value };\n";
        let source = format!("{prelude}{consumer_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "consumer", "modules/consumer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );

        let plan = plan_from_rows(rows);
        let consumer_source = planned_source(&plan, "modules/consumer.ts");
        let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

        assert!(
            consumer_source.contains("import { ns } from './runtime/source-1-helpers.js';"),
            "{consumer_source}"
        );
        assert!(consumer_source.contains("var value = ns.value + shared;"));
        assert!(helper_source.contains("var ns = {};"), "{helper_source}");
        assert!(
            helper_source.contains("Object.defineProperties(ns"),
            "{helper_source}"
        );
    }

    #[test]
    fn runtime_namespace_member_access_rejects_writes_and_updates() {
        let prelude = concat!(
            "var shared = 1;\n",
            "var ns = {};\n",
            "function expose(target, exports) {}\n",
            "expose(ns, { value: () => shared });\n",
        );
        let consumer_body = "ns.value = 2;\nvar value = ns.value++;\nexport { value };\n";
        let source = format!("{prelude}{consumer_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "consumer", "modules/consumer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );

        let plan = plan_from_rows(rows);
        let consumer_source = planned_source(&plan, "modules/consumer.ts");
        let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

        assert!(
            consumer_source.contains("import { ns } from './runtime/source-1-helpers.js';"),
            "{consumer_source}"
        );
        assert!(consumer_source.contains("ns.value = 2;"));
        assert!(consumer_source.contains("var value = ns.value++;"));
        assert!(helper_source.contains("var ns = {};"), "{helper_source}");
        assert!(
            helper_source.contains("Object.defineProperties(ns"),
            "{helper_source}"
        );
    }

    #[test]
    fn runtime_namespace_member_access_read_gate_rejects_compound_assignments() {
        for (source, start, end) in [
            ("ns.value += 1;", 0, 8),
            ("ns.value &&= 1;", 0, 8),
            ("ns.value ||= 1;", 0, 8),
            ("ns.value ??= 1;", 0, 8),
            ("ns.value **= 2;", 0, 8),
            ("ns.value <<= 1;", 0, 8),
            ("ns.value >>>= 1;", 0, 8),
            ("++ns.value;", 2, 10),
            ("ns.value--;", 0, 8),
        ] {
            assert!(
                !super::runtime_namespace_member_access_site_is_read_only(source, start, end),
                "{source}"
            );
        }
        for (source, start, end) in [
            ("ns.value + 1;", 0, 8),
            ("ns.value && other;", 0, 8),
            ("ns.value || other;", 0, 8),
            ("ns.value ?? other;", 0, 8),
            ("ns.value >= 1;", 0, 8),
            ("ns.value === 1;", 0, 8),
        ] {
            assert!(
                super::runtime_namespace_member_access_site_is_read_only(source, start, end),
                "{source}"
            );
        }
    }

    #[test]
    fn single_reader_runtime_var_migration_rejects_non_runtime_reader_dependency() {
        let prelude = "var shared;\nfunction formatShared() { return decorate(shared); }\n";
        let writer_body = "shared = 'ok';\nexport { shared };\n";
        let helper_body = "function decorate(value) { return value; }\nexport { decorate };\n";
        let consumer_body = "var value = formatShared();\nexport { value };\n";
        let source = format!("{prelude}{writer_body}{helper_body}{consumer_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "writer", "modules/writer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + writer_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "helper", "modules/helper.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + writer_body.len()) as u32,
                    (prelude.len() + writer_body.len() + helper_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + writer_body.len() + helper_body.len()) as u32,
                    source.len() as u32,
                )),
        );

        let plan = plan_from_rows(rows);
        let writer_source = planned_source(&plan, "modules/writer.ts");
        let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

        assert!(writer_source.contains(
            "import { shared, __reverts_set_shared } from './runtime/source-1-helpers.js';"
        ));
        assert!(writer_source.contains("__reverts_set_shared('ok');"));
        assert!(!writer_source.contains("function formatShared()"));
        assert!(helper_source.contains("import { decorate } from '../helper.js';"));
        assert!(helper_source.contains("var shared;"));
        assert!(helper_source.contains("function formatShared() { return decorate(shared); }"));
        assert!(
            helper_source
                .contains("function __reverts_set_shared(value) { shared = value; return value; }")
        );
    }

    #[test]
    fn private_runtime_lazy_initializer_purifies_private_assignments() {
        let source = concat!(
            "var target;\n",
            "function value() { return 1; }\n",
            "var init = lazyValue(() => {\n",
            "\ttarget = value;\n",
            "});\n",
            "function read() { init(); return target; }\n",
        );
        let writable = BTreeSet::from([
            BindingName::new("target"),
            BindingName::new("init"),
            BindingName::new("value"),
            BindingName::new("read"),
        ]);
        let lowered = purify_private_runtime_lazy_initializers(source, &writable);

        assert!(!lowered.contains("= lazyValue("));
        assert!(lowered.contains("target = value;"));
        assert!(lowered.contains("var init = () => {};"));
        assert!(lowered.contains("function read() { init(); return target; }"));
    }

    #[test]
    fn private_runtime_lazy_initializer_keeps_impure_bodies_lazy() {
        let source = concat!(
            "var target;\n",
            "function value() { return 1; }\n",
            "var init = lazyValue(() => {\n",
            "\tsetup();\n",
            "\ttarget = value;\n",
            "});\n",
        );
        let writable = BTreeSet::from([
            BindingName::new("target"),
            BindingName::new("init"),
            BindingName::new("value"),
        ]);
        let lowered = purify_private_runtime_lazy_initializers(source, &writable);

        assert!(lowered.contains("var init = lazyValue(() => {"));
    }

    #[test]
    fn runtime_prelude_binding_written_by_module_uses_live_setter() {
        let planner = ImportExportPlanner;
        // Object literal initializer keeps Phase 10c's migration plan
        // from picking this binding up — the test still validates that
        // the cross-module setter mechanism is wired correctly.
        let prelude = "var shared = {};\n";
        let body = "shared = 1;\nexport { shared };\n";
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let entry_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/entry.ts")
            .expect("entry file should be planned");
        let entry_source = entry_file.body.join("\n");

        assert!(entry_source.contains(
            "import { shared, __reverts_set_shared } from './runtime/source-1-helpers.js';"
        ));
        assert!(entry_source.contains("__reverts_set_shared(1);"));
        assert!(entry_source.contains("export { shared };"));
        let helper_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
            .expect("runtime helper file should be planned");
        let helper_source = helper_file.body.join("\n");
        assert!(
            helper_source
                .contains("function __reverts_set_shared(value) { shared = value; return value; }")
        );
        let export_line = helper_source
            .lines()
            .find(|line| line.starts_with("export {"))
            .expect("helper should export runtime bindings");
        let exports = parse_generated_named_export_statement(export_line)
            .expect("helper should emit a generated export list");
        assert!(exports.contains(&BindingName::new("__reverts_set_shared")));
        assert!(exports.contains(&BindingName::new("shared")));
    }

    #[test]
    fn write_only_runtime_prelude_binding_imports_setter_without_value() {
        let planner = ImportExportPlanner;
        // Object literal initializer keeps the binding in runtime so this test
        // focuses on the setter import surface, not migration.
        let prelude = "var shared = {};\n";
        let body = "shared = 1;\n";
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let entry_source = planned_source(&plan, "modules/entry.ts");

        assert!(
            entry_source
                .contains("import { __reverts_set_shared } from './runtime/source-1-helpers.js';")
        );
        assert!(!entry_source.contains("import { shared, __reverts_set_shared }"));
        assert!(entry_source.contains("__reverts_set_shared(1);"));
        assert!(!entry_source.contains("export { shared };"));
        let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");
        assert!(
            helper_source
                .contains("function __reverts_set_shared(value) { shared = value; return value; }")
        );
        let export_line = helper_source
            .lines()
            .find(|line| line.starts_with("export {"))
            .expect("helper should export runtime bindings");
        let exports = parse_generated_named_export_statement(export_line)
            .expect("helper should emit a generated export list");
        assert!(exports.contains(&BindingName::new("__reverts_set_shared")));
        assert!(!exports.contains(&BindingName::new("shared")));
    }

    #[test]
    fn lazy_initializer_module_written_runtime_bindings_are_folded_into_helper() {
        let planner = ImportExportPlanner;
        let prelude = concat!(
            "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
            "var shared;\n",
            "var Custom;\n",
        );
        let body = concat!(
            "var initShared = lazy(() => { shared = { ['required']: !1, matches: (A) => A.ready }; Custom = class Custom extends Error { constructor(A) { super(A); } }; });\n",
            "export { initShared, shared, Custom };\n",
        );
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let entry_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/entry.ts")
            .expect("entry file should be planned");
        let helper_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
            .expect("runtime helper file should be planned");
        let entry_source = entry_file.body.join("\n");
        let helper_source = helper_file.body.join("\n");

        assert!(entry_source.contains(
            "export { Custom, initShared, shared } from './runtime/source-1-helpers.js';"
        ));
        assert!(!entry_source.contains("__reverts_set_shared"));
        assert!(
            helper_source.contains("var shared, Custom;"),
            "consecutive runtime var declarations should be coalesced:\n{helper_source}"
        );
        assert!(helper_source.contains("shared = { ['required']: !1, matches: (A) => A.ready };"));
        assert!(
            helper_source
                .contains("Custom = class Custom extends Error { constructor(A) { super(A); } };")
        );
        assert!(helper_source.contains("var initShared = () => {};"));
        assert!(!helper_source.contains("_$init"));
        assert!(!helper_source.contains("_$val"));
        assert!(!helper_source.contains("var initShared = (() => {"));
        assert!(!helper_source.contains("__reverts_set_shared"));
        assert!(helper_source.contains("export { Custom, initShared, shared };"));
    }

    #[test]
    fn impure_lazy_initializer_keeps_lazy_thunk_while_folding_into_helper() {
        let planner = ImportExportPlanner;
        let prelude = concat!(
            "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
            "var shared;\n",
        );
        let body = concat!(
            "var initShared = lazy(() => { shared = Date.now(); });\n",
            "export { initShared, shared };\n",
        );
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let entry_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/entry.ts")
            .expect("entry file should be planned");
        let helper_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
            .expect("runtime helper file should be planned");
        let entry_source = entry_file.body.join("\n");
        let helper_source = helper_file.body.join("\n");

        assert!(
            entry_source
                .contains("export { initShared, shared } from './runtime/source-1-helpers.js';")
        );
        assert!(helper_source.contains("var initShared = lazyValue(() => {"));
        assert!(helper_source.contains("function lazyValue(factory) {"));
        assert!(helper_source.contains("shared = Date.now()"));
        assert!(!helper_source.contains("__reverts_set_shared"));
        assert!(helper_source.contains("export { initShared, shared };"));
    }

    #[test]
    fn lazy_folded_stub_with_internal_consumers_is_bypassed() {
        let prelude = concat!(
            "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
            "var shared;\n",
        );
        let folded_body = concat!(
            "var initShared = lazy(() => { shared = 1; });\n",
            "export { initShared, shared };\n",
        );
        let folded_source = format!("{prelude}{folded_body}");
        let consumer_source = "initShared();\nconsole.log(shared);\n";
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "folded.js",
            Some(folded_source.clone()),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "consumer.js",
            Some(consumer_source.to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "folded", "modules/folded.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    folded_source.len() as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "consumer", "modules/consumer.ts")
                .with_source_file(2),
        );
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(2),
            target: ModuleDependencyTarget::Module(ModuleId(1)),
        });

        let plan = plan_from_rows(rows);
        let consumer_source = planned_source(&plan, "modules/consumer.ts");
        let helper_source = planned_source(&plan, "modules/runtime/source-1-helpers.ts");

        assert!(
            planned_source_opt(&plan, "modules/folded.ts").is_none(),
            "pure re-export folded stub should be omitted"
        );
        assert!(
            consumer_source
                .contains("import { initShared, shared } from './runtime/source-1-helpers.js';")
        );
        assert!(!consumer_source.contains("from './folded.js'"));
        assert!(helper_source.contains("shared = 1;"));
        assert!(helper_source.contains("var initShared = () => {};"));
        assert!(helper_source.contains("export { initShared, shared };"));
    }

    #[test]
    fn pure_reexport_stub_with_internal_consumers_is_bypassed() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "target.js",
            Some("export const value = 1;\n".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "barrel.js",
            Some("export { value } from './target.js';\n".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            3,
            "consumer.js",
            Some("console.log(value);\n".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "target", "modules/target.ts")
                .with_source_file(1),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "barrel", "modules/barrel.ts")
                .with_source_file(2),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
                .with_source_file(3),
        );
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(2),
            target: ModuleDependencyTarget::Module(ModuleId(1)),
        });
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(3),
            target: ModuleDependencyTarget::Module(ModuleId(2)),
        });

        let plan = plan_from_rows(rows);
        let consumer_source = planned_source(&plan, "modules/consumer.ts");

        assert!(planned_source_opt(&plan, "modules/barrel.ts").is_none());
        assert!(consumer_source.contains("import { value } from './target.js';"));
        assert!(!consumer_source.contains("from './barrel.js'"));
    }

    #[test]
    fn pure_import_then_export_barrel_with_internal_consumers_is_bypassed() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "target.js",
            Some("export const value = 1;\n".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "barrel.js",
            Some("import { value } from './target.js';\nexport { value };\n".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            3,
            "consumer.js",
            Some("console.log(value);\n".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "target", "modules/target.ts")
                .with_source_file(1),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "barrel", "modules/barrel.ts")
                .with_source_file(2),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
                .with_source_file(3),
        );
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(2),
            target: ModuleDependencyTarget::Module(ModuleId(1)),
        });
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(3),
            target: ModuleDependencyTarget::Module(ModuleId(2)),
        });

        let plan = plan_from_rows(rows);
        let consumer_source = planned_source(&plan, "modules/consumer.ts");

        assert!(planned_source_opt(&plan, "modules/barrel.ts").is_none());
        assert!(consumer_source.contains("import { value } from './target.js';"));
        assert!(!consumer_source.contains("from './barrel.js'"));
    }

    #[test]
    fn pure_reexport_alias_stub_is_not_bypassed() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "target.js",
            Some("export const value = 1;\n".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "barrel.js",
            Some("export { value as renamed } from './target.js';\n".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            3,
            "consumer.js",
            Some("console.log(renamed);\n".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "target", "modules/target.ts")
                .with_source_file(1),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "barrel", "modules/barrel.ts")
                .with_source_file(2),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
                .with_source_file(3),
        );
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(2),
            target: ModuleDependencyTarget::Module(ModuleId(1)),
        });
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(3),
            target: ModuleDependencyTarget::Module(ModuleId(2)),
        });

        let plan = plan_from_rows(rows);
        let consumer_source = planned_source(&plan, "modules/consumer.ts");
        let barrel_source = planned_source(&plan, "modules/barrel.ts");

        assert!(consumer_source.contains("import { renamed } from './barrel.js';"));
        assert!(barrel_source.contains("export { value as renamed } from './target.js';"));
    }

    #[test]
    fn import_alias_then_export_barrel_is_not_bypassed() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "target.js",
            Some("export const value = 1;\n".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "barrel.js",
            Some(
                "import { value as renamed } from './target.js';\nexport { renamed };\n"
                    .to_string(),
            ),
        ));
        rows.source_files.push(SourceFileInput::new(
            3,
            "consumer.js",
            Some("console.log(renamed);\n".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "target", "modules/target.ts")
                .with_source_file(1),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "barrel", "modules/barrel.ts")
                .with_source_file(2),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
                .with_source_file(3),
        );
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(2),
            target: ModuleDependencyTarget::Module(ModuleId(1)),
        });
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(3),
            target: ModuleDependencyTarget::Module(ModuleId(2)),
        });

        let plan = plan_from_rows(rows);
        let consumer_source = planned_source(&plan, "modules/consumer.ts");
        let barrel_source = planned_source(&plan, "modules/barrel.ts");

        assert!(consumer_source.contains("import { renamed } from './barrel.js';"));
        assert!(barrel_source.contains("import { value as renamed } from './target.js';"));
        assert!(barrel_source.contains("export { renamed };"));
    }

    #[test]
    fn direct_reexport_with_extra_import_barrel_is_not_bypassed() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "target.js",
            Some("export const value = 1;\n".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "side.js",
            Some("export const side = 2;\nconsole.log(side);\n".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            3,
            "barrel.js",
            Some(
                "import { side } from './side.js';\nexport { value } from './target.js';\n"
                    .to_string(),
            ),
        ));
        rows.source_files.push(SourceFileInput::new(
            4,
            "consumer.js",
            Some("console.log(value);\n".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "target", "modules/target.ts")
                .with_source_file(1),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "side", "modules/side.ts").with_source_file(2),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(3), "barrel", "modules/barrel.ts")
                .with_source_file(3),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(4), "consumer", "modules/consumer.ts")
                .with_source_file(4),
        );
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(3),
            target: ModuleDependencyTarget::Module(ModuleId(1)),
        });
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(3),
            target: ModuleDependencyTarget::Module(ModuleId(2)),
        });
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(4),
            target: ModuleDependencyTarget::Module(ModuleId(3)),
        });

        let plan = plan_from_rows(rows);
        let consumer_source = planned_source(&plan, "modules/consumer.ts");
        let barrel_source = planned_source(&plan, "modules/barrel.ts");

        assert!(consumer_source.contains("import { value } from './barrel.js';"));
        assert!(barrel_source.contains("import { side } from './side.js';"));
        assert!(barrel_source.contains("export { value } from './target.js';"));
    }

    #[test]
    fn side_effectful_reexport_barrel_is_not_bypassed() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "target.js",
            Some("export const value = 1;\n".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            2,
            "barrel.js",
            Some("console.log('load barrel');\nexport { value } from './target.js';\n".to_string()),
        ));
        rows.source_files.push(SourceFileInput::new(
            3,
            "consumer.js",
            Some("console.log(value);\n".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "target", "modules/target.ts")
                .with_source_file(1),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "barrel", "modules/barrel.ts")
                .with_source_file(2),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(3), "consumer", "modules/consumer.ts")
                .with_source_file(3),
        );
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(2),
            target: ModuleDependencyTarget::Module(ModuleId(1)),
        });
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(3),
            target: ModuleDependencyTarget::Module(ModuleId(2)),
        });

        let plan = plan_from_rows(rows);
        let consumer_source = planned_source(&plan, "modules/consumer.ts");
        let barrel_source = planned_source(&plan, "modules/barrel.ts");

        assert!(consumer_source.contains("import { value } from './barrel.js';"));
        assert!(barrel_source.contains("console.log('load barrel')"));
        assert!(barrel_source.contains("export { value } from './target.js';"));
    }

    #[test]
    fn lazy_initializer_fold_preserves_tail_side_effect_order_before_entrypoint() {
        let planner = ImportExportPlanner;
        let prelude = concat!(
            "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
            "var shared;\n",
        );
        let body = concat!(
            "var initShared = lazy(() => { shared = 1; });\n",
            "export { initShared, shared };\n",
        );
        let tail = concat!(
            "var main = () => { console.log(shared); };\n",
            "initShared();\n",
            "main();\n",
        );
        let source = format!("{prelude}{body}{tail}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + body.len()) as u32,
                )),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let helper_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
            .expect("runtime helper file should be planned");
        let helper_source = helper_file.body.join("\n");
        let init_assignment = helper_source
            .find("shared = 1;")
            .expect("pure initializer assignment should be before side effects");
        let tail_call = helper_source
            .find("initShared();")
            .expect("tail side effect should be preserved");

        assert!(init_assignment < tail_call);
        assert!(helper_source.contains("var initShared = () => {};"));
        assert!(helper_source.contains("var main = () => { console.log(shared); };"));
        assert!(helper_source.contains("export { initShared, main, shared };"));
    }

    #[test]
    fn lazy_initializer_fold_imports_source_module_dependencies_from_helper() {
        let planner = ImportExportPlanner;
        let prelude = concat!(
            "var lazy = (init, value) => () => (init && (value = init(init = 0)), value);\n",
            "var shared;\n",
        );
        let body = concat!(
            "var initShared = lazy(() => { shared = buildValue(); });\n",
            "export { initShared, shared };\n",
        );
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.source_files.push(SourceFileInput::new(
            2,
            "dep.js",
            Some("function buildValue() { return 42; }\n".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "dep", "modules/dep.ts").with_source_file(2),
        );
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(1),
            target: ModuleDependencyTarget::Module(ModuleId(2)),
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let helper_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
            .expect("runtime helper file should be planned");
        let dep_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/dep.ts")
            .expect("source dependency should be planned");
        let helper_source = helper_file.body.join("\n");
        let dep_source = dep_file.body.join("\n");

        assert!(helper_source.contains("import { buildValue } from '../dep.js';"));
        assert!(helper_source.contains("shared = buildValue();"));
        assert!(dep_source.contains("export { buildValue };"));
    }

    #[test]
    fn runtime_prelude_update_writes_use_live_setters() {
        let planner = ImportExportPlanner;
        // Object-literal initializers keep both bindings out of the
        // Phase 10c migration plan (literal-number initializers would
        // otherwise be picked up). The numeric update-operator rewrite
        // path under test runs against the runtime-coerced numbers
        // produced at runtime.
        let prelude = "var counter = {};\nvar result = {};\n";
        let body = "result = counter--;\n++counter;\nexport { counter, result };\n";
        let source = format!("{prelude}{body}");
        assert!(
            super::implicit_global_writes_in_source(body).contains(&BindingName::new("counter"))
        );
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let entry_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/entry.ts")
            .expect("entry file should be planned");
        let entry_source = entry_file.body.join("\n");

        assert!(entry_source.contains(
            "import { counter, result, __reverts_set_counter, __reverts_set_result } from './runtime/source-1-helpers.js';"
        ));
        assert!(entry_source.contains("__reverts_set_result((() => {"));
        assert!(entry_source.contains("let _$p = _$u--;"));
        assert!(entry_source.contains("let _$n = ++_$u;"));
        assert!(entry_source.contains("__reverts_set_counter(_$u);"));
        assert!(!entry_source.contains("counter--"));
        assert!(!entry_source.contains("++counter"));
    }

    #[test]
    fn runtime_helper_files_import_source_module_dependencies_and_initialize_namespaces() {
        let planner = ImportExportPlanner;
        let prelude = concat!(
            "var ns = {};\n",
            "function expose(target, exports) {}\n",
            "expose(ns, { ready: () => ready });\n",
            "function ready() { return true; }\n",
            "function helper() { return Promise.resolve().then(() => (init(), ns)); }\n",
        );
        let entry_body = "var value = helper();\nexport { value };\n";
        let init_body = "var init = () => {};\n";
        let source = format!("{prelude}{entry_body}{init_body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    prelude.len() as u32,
                    (prelude.len() + entry_body.len()) as u32,
                )),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "init", "modules/init.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(
                    (prelude.len() + entry_body.len()) as u32,
                    source.len() as u32,
                )),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let helper_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
            .expect("runtime helper file should be planned");
        let helper_source = helper_file.body.join("\n");
        let init_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/init.ts")
            .expect("init file should be planned");
        let init_source = init_file.body.join("\n");

        assert!(helper_source.contains("import { init } from '../init.js';"));
        assert!(helper_source.contains("Object.defineProperties(ns,"));
        assert!(helper_source.contains("get: () => ready"));
        assert!(init_source.contains("export { init };"));
    }

    #[test]
    fn runtime_prelude_array_destructuring_writes_use_live_setters() {
        let planner = ImportExportPlanner;
        // Object-literal initializers keep both bindings out of the
        // Phase 10c migration plan — the destructuring rewrite path
        // under test still routes writes through the cross-module
        // setter.
        let prelude = "var left = {};\nvar right = {};\n";
        let body = "[left, right] = [1, 2];\nexport { left, right };\n";
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let entry_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/entry.ts")
            .expect("entry file should be planned");
        let entry_source = entry_file.body.join("\n");

        assert!(entry_source.contains(
            "import { left, right, __reverts_set_left, __reverts_set_right } from './runtime/source-1-helpers.js';"
        ));
        assert!(entry_source.contains("__reverts_set_left(_$t[0]);"));
        assert!(entry_source.contains("__reverts_set_right(_$t[1]);"));
        assert!(!entry_source.contains("[left, right] ="));
    }

    #[test]
    fn runtime_prelude_write_inside_computed_class_key_uses_live_setter() {
        let planner = ImportExportPlanner;
        // Object-literal initializers keep the prelude vars out of the
        // Phase 10c migration plan — the test still exercises the
        // setter rewrite inside a computed class key.
        let prelude = "var J = (init, value) => () => (init && (value = init(init = 0)), value);\nvar Stream = {};\nvar holder = {};\n";
        let body = "void 0;\nvar init = J(() => { Stream = class Stream { [(holder = new WeakMap(), Symbol.iterator)]() { return 1; } }; });\nexport { Stream, init };\n";
        let source = format!("{prelude}{body}");
        assert!(
            super::implicit_global_writes_in_source(body).contains(&BindingName::new("holder"))
        );
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let entry_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/entry.ts")
            .expect("entry file should be planned");
        let entry_source = entry_file.body.join("\n");

        assert!(entry_source.contains("__reverts_set_Stream"));
        assert!(entry_source.contains("__reverts_set_holder"));
        assert!(entry_source.contains("__reverts_set_holder(new WeakMap())"));
        assert!(!entry_source.contains("holder = new WeakMap()"));
    }

    #[test]
    fn runtime_helper_namespace_exports_initialize_empty_object_helpers() {
        let planner = ImportExportPlanner;
        // Object-literal initializer keeps d2 out of the Phase 10c
        // migration plan — this test continues to validate the
        // namespace-export setup alongside cross-module setter use.
        let prelude =
            "var zT = () => 'enum';\nvar m = {};\nM5(m, { enum: () => zT });\nvar d2 = {};\n";
        let body = "d2 = m;\nexport { d2 };\n";
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let entry_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/entry.ts")
            .expect("entry file should be planned");
        let helper_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
            .expect("runtime helper file should be planned");
        let entry_source = entry_file.body.join("\n");
        let helper_source = helper_file.body.join("\n");

        assert!(
            entry_source.contains(
                "import { d2, m, __reverts_set_d2 } from './runtime/source-1-helpers.js';"
            )
        );
        assert!(helper_source.contains("var zT = () => 'enum';"));
        assert!(helper_source.contains("var m = {};"));
        assert!(helper_source.contains(
            "Object.defineProperties(m, { enum: { enumerable: true, get: () => zT } });"
        ));
        assert!(helper_source.contains("export { __reverts_set_d2, d2, m };"));
    }

    #[test]
    fn array_member_assignment_is_not_treated_as_binding_pattern() {
        assert!(
            super::array_destructuring_assignment_writes("[this.value] = values;", 0).is_none()
        );
        assert!(super::array_destructuring_assignment_writes("object[key] = value;", 6).is_none());
    }

    #[test]
    fn template_interpolation_runtime_helper_is_imported_from_prelude() {
        let planner = ImportExportPlanner;
        let prelude = "var EDL = 'date';\n";
        let body = "var value = new RegExp(`^${EDL}$`);\nexport { value };\n";
        let source = format!("{prelude}{body}");
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(source.clone())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(prelude.len() as u32, source.len() as u32)),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let entry_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/entry.ts")
            .expect("entry file should be planned");
        let helper_file = plan
            .files
            .iter()
            .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
            .expect("runtime helper file should be planned");
        let entry_source = entry_file.body.join("\n");
        let helper_source = helper_file.body.join("\n");

        assert!(entry_source.contains("import { EDL } from './runtime/source-1-helpers.js';"));
        assert!(helper_source.contains("var EDL = 'date';"));
        assert!(helper_source.contains("export { EDL };"));
    }

    #[test]
    fn bare_commonjs_require_gets_esm_create_require_bridge() {
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("var crypto = require('crypto');\nexport { crypto };".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let entry_source = plan.files[0].body.join("\n");

        assert!(entry_source.contains("import { createRequire } from 'node:module';"));
        assert!(entry_source.contains("var require = createRequire(import.meta.url);"));
        assert!(entry_source.contains("require('crypto')"));
    }

    #[test]
    fn require_bridge_does_not_redeclare_implicit_require_global() {
        let planner = ImportExportPlanner;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("require('fs');\nrequire = require;\nexport const value = 1;".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "entry", "modules/entry.ts").with_source_file(1),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");
        let model = ProgramModel::from_input(input);
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
            Vec::new(),
            reverts_ir::BindingShapeSolution::default(),
        );

        let plan = planner
            .plan_enriched_program(&enriched)
            .expect("fixture should normalize");
        let entry_source = plan.files[0].body.join("\n");

        assert!(entry_source.contains("var require = createRequire(import.meta.url);"));
        assert!(!entry_source.contains("const require ="));
        assert!(!entry_source.contains("var require;\n"));
    }
}
