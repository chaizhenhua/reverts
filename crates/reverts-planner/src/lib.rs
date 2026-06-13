use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use reverts_graph::{
    RevertsGraph, RuntimeEntrypoint, RuntimeNamespaceExport, RuntimePrelude,
    RuntimePreludeBindingKind, RuntimePreludeImport,
};
use reverts_input::{ModuleDependencyTarget, PackageAttributionStatus, PackageEmissionMode};
use reverts_ir::{BindingName, BindingShape, ModuleId, ModuleKind};
use reverts_js::{
    ImportUsageScope, ParseGoal, classify_import_usage_scope, format_source_pretty,
    parse_error_message, verify_only_immediate_call_references,
};
use reverts_model::{CompilerEvidence, CompilerKind, EnrichedProgram, ModuleCompilerProfile};
use reverts_package::PackageResolution;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EmitPlan {
    pub files: Vec<PlannedFile>,
}

impl EmitPlan {
    pub fn push_file(&mut self, file: PlannedFile) {
        self.files.push(file);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub path: String,
    pub imports: Vec<PlannedImport>,
    pub bindings: Vec<PlannedBinding>,
    pub exports: Vec<PlannedExport>,
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

    pub fn set_compiler_recovery(&mut self, compiler_recovery: CompilerRecoveryDecision) {
        self.compiler_recovery = compiler_recovery;
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

impl ImportExportPlanner {
    pub fn plan_enriched_program(self, program: &EnrichedProgram) -> Result<EmitPlan, PlanError> {
        let mut plan = EmitPlan::default();
        let mut used_runtime_helper_files = BTreeMap::<u32, BTreeSet<BindingName>>::new();
        let mut required_runtime_helper_bindings = BTreeMap::<u32, BTreeSet<BindingName>>::new();
        let mut used_runtime_helper_setters = BTreeMap::<u32, BTreeSet<BindingName>>::new();
        let mut used_lazy_module = BTreeSet::<u32>::new();
        let mut used_lazy_value = BTreeSet::<u32>::new();
        let accepted_externalized_packages = externalized_package_modules(program);
        let source_required_packages =
            source_required_package_modules(program, &accepted_externalized_packages);
        let externalized_packages = accepted_externalized_packages
            .difference(&source_required_packages)
            .copied()
            .collect::<BTreeSet<_>>();
        let source_module_wiring = source_module_wiring(program, &externalized_packages);
        let eager_safe_analysis = compute_eager_safe_analysis(program, &source_module_wiring);
        let lowered_runtime_sources =
            lowered_runtime_sources(program, &source_module_wiring, &eager_safe_analysis);
        let runtime_lazy_folds =
            runtime_lazy_fold_plan(program, &source_module_wiring, &lowered_runtime_sources);
        let runtime_var_migrations = compute_runtime_var_migration_plan(
            program,
            &lowered_runtime_sources,
            &runtime_lazy_folds,
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

            if let Some(folded) = runtime_lazy_folds.modules.get(&module.id) {
                used_runtime_helper_files
                    .entry(folded.source_file_id)
                    .or_default()
                    .extend(folded.stub_exports.iter().cloned());
                required_runtime_helper_bindings
                    .entry(folded.source_file_id)
                    .or_default()
                    .extend(folded.required_bindings.iter().cloned());
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

            if let Some(module_imports) = source_module_wiring.imports_by_module.get(&module.id) {
                for (target_module_id, bindings) in module_imports {
                    let Some(target_path) = module_output_path(program, *target_module_id) else {
                        continue;
                    };
                    let specifier = relative_import_specifier(path, target_path.as_str());
                    file.push_source(named_import_statement(bindings.iter(), specifier.as_str()));
                    for binding in bindings {
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

            let runtime_imports = program.model().graph().runtime_imports_for(module.id);
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
            let remaining_runtime_helpers: BTreeSet<BindingName> = remaining_runtime_helpers
                .difference(&migrated_locally)
                .cloned()
                .collect();
            let written_runtime_helpers: BTreeSet<BindingName> = written_runtime_helpers
                .difference(&migrated_locally)
                .cloned()
                .collect();

            let runtime_import_groups = group_runtime_imports(runtime_imports);
            let lazy_helper_names = lowered_source
                .map(lazy_helper_import_names_for_source)
                .unwrap_or_default();
            if let Some(lowered_source) = lowered_source
                && (!remaining_runtime_helpers.is_empty() || !lazy_helper_names.is_empty())
            {
                used_runtime_helper_files
                    .entry(lowered_source.source_file_id)
                    .or_default()
                    .extend(remaining_runtime_helpers.iter().cloned());
                required_runtime_helper_bindings
                    .entry(lowered_source.source_file_id)
                    .or_default()
                    .extend(remaining_runtime_helpers.iter().cloned());
                if !written_runtime_helpers.is_empty() {
                    used_runtime_helper_setters
                        .entry(lowered_source.source_file_id)
                        .or_default()
                        .extend(written_runtime_helpers.iter().cloned());
                }
                if lowered_source.uses_lazy_module {
                    used_lazy_module.insert(lowered_source.source_file_id);
                }
                if lowered_source.uses_lazy_value {
                    used_lazy_value.insert(lowered_source.source_file_id);
                }
                let specifier = relative_import_specifier(
                    path,
                    runtime_helpers_path(lowered_source.source_file_id).as_str(),
                );
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

            for (source_file_id, bindings) in runtime_import_groups {
                let bindings = bindings
                    .into_iter()
                    .filter(|binding| !lowered_helpers.contains(binding))
                    .filter(|binding| !remaining_runtime_helpers.contains(binding))
                    .filter(|binding| !planned_bindings.contains(binding))
                    .filter(|binding| !local_source_definitions.contains(binding))
                    .filter(|binding| !local_source_writes.contains(binding))
                    .collect::<BTreeSet<_>>();
                if bindings.is_empty() {
                    continue;
                }
                used_runtime_helper_files
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
            let source_imports = program.model().graph().ast_imports_for(module.id);
            let reshaped_bindings = lowered_source
                .map(|src| src.reshaped_bindings.clone())
                .unwrap_or_default();
            for original in program.model().graph().definitions_for(module.id) {
                let source_backed = source_definitions.contains(&original);
                let emitted = if source_backed {
                    original.clone()
                } else {
                    program
                        .semantic_names()
                        .binding_name(module.id, original.as_str())
                        .cloned()
                        .unwrap_or_else(|| original.clone())
                };
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
                planned_bindings.insert(original.clone());
                file.add_binding(plan_binding_from_program(
                    program,
                    module.id,
                    original.clone(),
                    original.clone(),
                    true,
                    None,
                ));
            }

            if let Some(lowered_source) = lowered_source {
                let source_file_path = lowered_source.source_file_path.as_str();
                let mut source = lowered_source.source.clone();
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
                for binding in &migrated_locally {
                    planned_bindings.insert(binding.clone());
                    file.add_binding(plan_binding_from_program(
                        program,
                        module.id,
                        binding.clone(),
                        binding.clone(),
                        true,
                        Some(BindingShape::Unknown),
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
                // resolving them transparently.
                if !migrated_locally.is_empty() {
                    file.push_source(variable_declaration_statement(migrated_locally.iter()));
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
            if !migrated_locally.is_empty() {
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
                let new_exports: BTreeSet<BindingName> = migrated_locally
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
            let entrypoint = prelude
                .entrypoint
                .as_ref()
                .filter(|entrypoint| helper_bindings.contains(&entrypoint.callee));
            let mut root_bindings = required_runtime_helper_bindings
                .get(source_file_id)
                .cloned()
                .unwrap_or_else(|| helper_bindings.clone());
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
            let migrations_for_source: BTreeMap<BindingName, ModuleId> = runtime_var_migrations
                .migrations_by_binding
                .iter()
                .filter(|(_, migration)| migration.source_file_id == *source_file_id)
                .map(|(binding, migration)| (binding.clone(), migration.owner_module))
                .collect();
            if !migrations_for_source.is_empty() {
                helper_closure.source = strip_runtime_var_declarations(
                    helper_closure.source.as_str(),
                    migrations_for_source.keys(),
                );
            }
            let helper_imports = runtime_source_module_imports(
                program,
                helper_closure.source.as_str(),
                &helper_closure.emitted_bindings,
                &externalized_packages,
            );
            let helper_path = runtime_helpers_path(*source_file_id);
            let unresolved = unresolved_runtime_helper_references(
                prelude,
                helper_closure.source.as_str(),
                &helper_closure.emitted_bindings,
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
            file.push_source(helper_closure.source);
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
            if emits_lazy_module {
                file.push_source(lazy_module_helper_source());
            }
            if emits_lazy_value {
                file.push_source(lazy_value_helper_source());
            }
            let mut exported_bindings = helper_bindings.clone();
            exported_bindings.extend(
                setter_bindings
                    .iter()
                    .map(|binding| BindingName::new(runtime_helper_setter_name(binding))),
            );
            // Phase 10b: drop migrated bindings from the runtime helper's
            // own named export — they're re-exported below from their
            // new owner module so the consumer's `import { X } from
            // runtime` still resolves to a live binding.
            for binding in migrations_for_source.keys() {
                exported_bindings.remove(binding);
            }
            if emits_lazy_module {
                exported_bindings.insert(BindingName::new("lazyModule"));
            }
            if emits_lazy_value {
                exported_bindings.insert(BindingName::new("lazyValue"));
            }
            file.push_source(named_export_statement(exported_bindings.iter()));
            // Phase 10b: per-owner re-exports for migrated bindings.
            // Grouping by owner emits one re-export statement per owner
            // module instead of one per binding, keeping the runtime
            // helper file's tail readable.
            let mut migrations_by_owner_path: BTreeMap<String, BTreeSet<BindingName>> =
                BTreeMap::new();
            for (binding, owner_module) in &migrations_for_source {
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
            for binding in helper_bindings
                .iter()
                .filter(|binding| !migrations_for_source.contains_key(*binding))
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
                emits_lazy_module.then_some("lazyModule"),
                emits_lazy_value.then_some("lazyValue"),
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
    migrations_by_owner: BTreeMap<ModuleId, BTreeSet<BindingName>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RuntimeVarMigration {
    owner_module: ModuleId,
    source_file_id: u32,
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
    lowered_runtime_sources: &BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
    runtime_lazy_folds: &RuntimeLazyFoldPlan,
) -> RuntimeVarMigrationPlan {
    // Modules whose lazy initializer bodies have already been folded
    // into the runtime helper file. Their consumer file is an empty
    // re-export stub; there is no source body to host a migrated
    // declaration or to absorb same-module assignments. Skip them.
    let folded_modules: BTreeSet<ModuleId> = runtime_lazy_folds.modules.keys().copied().collect();
    // Invert `written_helpers` to find single-writer bindings — but
    // exclude writes that came from a module that was later folded.
    let mut writers: BTreeMap<BindingName, BTreeSet<(ModuleId, u32)>> = BTreeMap::new();
    for (module_id, source) in lowered_runtime_sources {
        if folded_modules.contains(module_id) {
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
    let mut plan = RuntimeVarMigrationPlan::default();
    // Group single-writer candidates by source file id so each runtime
    // prelude is scanned once.
    let mut by_source: BTreeMap<u32, Vec<(BindingName, ModuleId)>> = BTreeMap::new();
    for (binding, (module_id, source_id)) in single_writers {
        by_source
            .entry(source_id)
            .or_default()
            .push((binding, module_id));
    }
    for (source_id, candidates) in by_source {
        let Some(prelude) = program.model().graph().runtime_prelude(source_id) else {
            continue;
        };
        for (binding, owner_module) in candidates {
            // The binding must have a setter (zero-writer or shared
            // bindings never enter `written_helpers`, but the prelude may
            // also expose source-backed reads for which there is no setter
            // — skip those).
            // The binding must be a real prelude declaration. Restrict
            // to bare `var X;` (or `let`/`const`) declarations without
            // an initializer — when the prelude has `var X = something;`
            // the runtime would lose the initial value if we removed the
            // line and moved the declaration to the writer (whose first
            // assignment may not match the original init).
            let Some(snippet) = prelude.snippets.get(&binding) else {
                continue;
            };
            if !is_uninitialized_var_declaration(snippet.source.as_str(), binding.as_str()) {
                continue;
            }
            // The setter function is synthesized by the planner at emit
            // time — it doesn't appear in the prelude snippets. Any
            // OTHER prelude snippet, namespace export, or folded chunk
            // that references X counts as a runtime read.
            let mut runtime_reads = false;
            for (key, snippet) in &prelude.snippets {
                if key == &binding {
                    continue;
                }
                if identifier_reference_positions(snippet.source.as_str(), binding.as_str())
                    .next()
                    .is_some()
                {
                    runtime_reads = true;
                    break;
                }
            }
            // Folded lazy initializer bodies have been pulled into the
            // runtime helpers file via `runtime_lazy_folds`. Any read of
            // the binding inside a folded chunk is just as fatal as one
            // in a regular prelude snippet — the runtime file's loader
            // would need the migrated binding to be defined before
            // evaluating that chunk.
            if !runtime_reads
                && let Some(chunks) = runtime_lazy_folds.chunks_by_source_file.get(&source_id)
            {
                for chunk in chunks {
                    if identifier_reference_positions(chunk.source.as_str(), binding.as_str())
                        .next()
                        .is_some()
                    {
                        runtime_reads = true;
                        break;
                    }
                }
            }
            // Namespace exports synthesize `Object.defineProperties(...,
            // { name: { get: () => X } })` runtime reads. Migrating X
            // would force the runtime to import it back to drive the
            // getter. Treat any binding referenced by a namespace export
            // (either as the namespace itself or as one of the exported
            // bindings) as a runtime read.
            if !runtime_reads {
                for namespace_export in &prelude.namespace_exports {
                    if namespace_export.namespace == binding
                        || namespace_export.helper == binding
                        || namespace_export
                            .exports
                            .values()
                            .any(|target| target == &binding)
                    {
                        runtime_reads = true;
                        break;
                    }
                }
            }
            if runtime_reads {
                continue;
            }
            plan.migrations_by_binding.insert(
                binding.clone(),
                RuntimeVarMigration {
                    owner_module,
                    source_file_id: source_id,
                },
            );
            plan.migrations_by_owner
                .entry(owner_module)
                .or_default()
                .insert(binding);
        }
    }
    plan
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
) -> BTreeMap<ModuleId, LoweredRuntimeModuleSource> {
    let mut sources = BTreeMap::new();
    for module in program.model().modules() {
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
        let mut lowering = lower_runtime_helpers(
            source.source,
            &helper_kinds,
            &exported_bindings,
            eager_safe_call_targets,
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
    let mut identifiers = BTreeSet::new();
    let class_field_bindings = class_field_bindings_in_source(source);
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'`' => cursor = collect_template_value_identifiers(source, cursor, &mut identifiers),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            byte if is_identifier_start(byte) => {
                let start = cursor;
                cursor += 1;
                while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
                    cursor += 1;
                }
                let identifier = &source[start..cursor];
                if !is_js_keyword(identifier)
                    && !is_runtime_global_identifier(identifier)
                    && !class_field_bindings.contains_key(&start)
                    && bytes.get(skip_ws(bytes, cursor)) == Some(&b'(')
                    && identifier_occurrence_is_value_reference(source, start, cursor)
                {
                    identifiers.insert(identifier.to_string());
                }
            }
            _ => cursor += 1,
        }
    }
    identifiers
}

fn value_identifiers_in_source(source: &str) -> BTreeSet<String> {
    let mut identifiers = BTreeSet::new();
    let class_field_bindings = class_field_bindings_in_source(source);
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'`' => cursor = collect_template_value_identifiers(source, cursor, &mut identifiers),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            byte if is_identifier_start(byte) => {
                let start = cursor;
                cursor += 1;
                while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
                    cursor += 1;
                }
                let identifier = &source[start..cursor];
                if !is_js_keyword(identifier)
                    && !class_field_bindings.contains_key(&start)
                    && identifier_occurrence_is_value_reference(source, start, cursor)
                {
                    identifiers.insert(identifier.to_string());
                }
            }
            _ => cursor += 1,
        }
    }
    identifiers
}

fn collect_template_value_identifiers(
    source: &str,
    start: usize,
    identifiers: &mut BTreeSet<String>,
) -> usize {
    let bytes = source.as_bytes();
    let mut cursor = start + 1;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor += 2,
            b'`' => return cursor + 1,
            b'$' if bytes.get(cursor + 1) == Some(&b'{') => {
                let open = cursor + 1;
                let Some(close) = find_matching_brace(source, open) else {
                    return skip_quoted(bytes, start, b'`');
                };
                identifiers.extend(value_identifiers_in_source(&source[open + 1..close]));
                cursor = close + 1;
            }
            _ => cursor += 1,
        }
    }
    bytes.len()
}

fn identifier_occurrence_is_value_reference(source: &str, start: usize, end: usize) -> bool {
    let bytes = source.as_bytes();
    if previous_non_ws(bytes, start)
        .and_then(|index| bytes.get(index))
        .is_some_and(|byte| matches!(*byte, b'.' | b'#'))
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

fn source_required_package_modules(
    program: &EnrichedProgram,
    externalized_packages: &BTreeSet<ModuleId>,
) -> BTreeSet<ModuleId> {
    let candidate_reads_by_module = candidate_source_reads_by_module(program);
    let modules_by_id = program
        .model()
        .modules()
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let exportable_bindings_by_module = source_exportable_bindings_by_module(program);
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
        if !changed {
            break;
        }
    }

    required
}

fn source_module_wiring(
    program: &EnrichedProgram,
    externalized_packages: &BTreeSet<ModuleId>,
) -> SourceModuleWiring {
    let mut wiring = SourceModuleWiring::default();
    let candidate_reads_by_module = candidate_source_reads_by_module(program);
    let modules_by_id = program
        .model()
        .modules()
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let exportable_bindings_by_module = source_exportable_bindings_by_module(program);

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

fn candidate_source_reads_by_module(
    program: &EnrichedProgram,
) -> BTreeMap<ModuleId, BTreeSet<BindingName>> {
    let mut reads = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
    for (module_id, binding) in program.model().graph().def_use().unresolved_reads() {
        reads.entry(module_id).or_default().insert(binding);
    }
    for module in program.model().modules() {
        let Some(source) = program.model().input().module_source_slice(module.id) else {
            continue;
        };
        let local_bindings = source_exportable_bindings(program, module.id);
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
    bindings
}

fn source_exportable_bindings_by_module(
    program: &EnrichedProgram,
) -> BTreeMap<ModuleId, BTreeSet<BindingName>> {
    program
        .model()
        .modules()
        .iter()
        .map(|module| (module.id, source_exportable_bindings(program, module.id)))
        .collect()
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
    let mut definitions = BTreeMap::<BindingName, Option<ModuleId>>::new();
    for module in program.model().modules() {
        if module.kind == ModuleKind::Package && externalized_packages.contains(&module.id) {
            continue;
        }
        let source_definitions = source_definition_bindings(program, module.id);
        for binding in source_definitions {
            definitions
                .entry(binding)
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

fn lower_runtime_helpers(
    source: &str,
    helper_kinds: &BTreeMap<BindingName, RuntimePreludeBindingKind>,
    exported_bindings: &BTreeSet<BindingName>,
    eager_safe_call_targets: &BTreeSet<String>,
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
    let remaining_helpers = helper_kinds
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
    // Fast path: byte-level scanner for `return PURE_EXPR;`. Falls
    // back to the OXC-AST classifier when the body is wrapped in an
    // IIFE, contains intermediate `var X;` declarations, or uses
    // assignment chains the byte path doesn't recognize.
    let value_expr = match extract_pure_return_expression(body) {
        Some(expr) => expr,
        None => reverts_js::extract_lazy_module_eager_value_with_safe_deps(
            body,
            "",
            None,
            None,
            ParseGoal::TypeScript,
            eager_safe_call_targets,
        )?,
    };
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
    // Body extraction has three fallback layers, tried in order of
    // cost. The byte-level scanners (1) and (2) cover the bulk of
    // simple shapes the bundler emits directly. Only when both fail
    // do we reach for the OXC-AST classifier (3), which adds support
    // for IIFE wrappers, assignment chains, harmless interleaved
    // declarations, and `Object.defineProperty(exports, ...)` patterns
    // — at the cost of an extra parse per declaration.
    let value_expr = if let Some(module_name) = module_param
        && let Some(expr) = extract_module_exports_assignment(body, module_name)
    {
        expr
    } else if let Some(expr) = extract_exports_properties_as_object_literal(body, exports_param) {
        expr
    } else if let Some(expr) = reverts_js::extract_lazy_module_eager_value_with_safe_deps(
        body,
        exports_param,
        module_param,
        None,
        ParseGoal::TypeScript,
        eager_safe_call_targets,
    ) {
        expr
    } else {
        return None;
    };
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

/// Match a body that is one or more `<exports_param>.<key> = PURE_EXPR;`
/// statements (and nothing else) and build the equivalent object literal
/// `{ key1: expr1, key2: expr2, ... }`. Property insertion order is preserved
/// so the rewritten value matches the CommonJS exports object's own-key
/// iteration order. Rejects duplicate keys, non-pure expressions, anything
/// other than `IDENT` keys (no `exports['foo']`), and any non-property
/// statement.
fn extract_exports_properties_as_object_literal(body: &str, exports_param: &str) -> Option<String> {
    let statements: Vec<&str> = top_level_statement_slices(body)
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if statements.is_empty() {
        return None;
    }
    let prefix = format!("{exports_param}.");
    let mut properties: Vec<(String, String)> = Vec::new();
    let mut seen_keys = BTreeSet::new();
    for stmt_text in &statements {
        let stmt = stmt_text.trim_end_matches(';').trim();
        let rest = stmt.strip_prefix(&prefix)?;
        let key_bytes = rest.as_bytes();
        if key_bytes.is_empty() || !is_identifier_start(key_bytes[0]) {
            return None;
        }
        let mut key_end = 1;
        while key_end < key_bytes.len() && is_identifier_continue(key_bytes[key_end]) {
            key_end += 1;
        }
        let key = &rest[..key_end];
        if !seen_keys.insert(key.to_string()) {
            return None;
        }
        let after_key = rest[key_end..].trim_start();
        let after_eq = after_key.strip_prefix('=')?;
        // Reject `==`, `===`, `=>` — these aren't assignments.
        if after_eq.starts_with('=') || after_eq.starts_with('>') {
            return None;
        }
        let value = after_eq.trim();
        if value.is_empty() {
            return None;
        }
        if !is_pure_initializer_expression(value) {
            return None;
        }
        properties.push((key.to_string(), value.to_string()));
    }
    if properties.is_empty() {
        return None;
    }
    let formatted = properties
        .iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("{{ {formatted} }}"))
}

/// Match a body that is exactly `<module_param>.exports = PURE_EXPR;` and
/// return the pure expression on the right-hand side. The check rejects bodies
/// with other statements, with non-pure expressions, or with assignments
/// targeting anything other than `<module_param>.exports`.
fn extract_module_exports_assignment(body: &str, module_param: &str) -> Option<String> {
    let statements: Vec<&str> = top_level_statement_slices(body)
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if statements.len() != 1 {
        return None;
    }
    let stmt = statements[0].trim_end_matches(';').trim();
    let prefix = format!("{module_param}.exports");
    let rest = stmt.strip_prefix(&prefix)?;
    // After the prefix, the next non-identifier-continue byte must be `=`,
    // otherwise we matched `module.exportsCache` or similar.
    let rest = rest.trim_start();
    let mut chars = rest.chars();
    let first = chars.next()?;
    if first != '=' {
        return None;
    }
    let second = chars.next();
    // Reject `==`, `===`, `=>`.
    if matches!(second, Some('=') | Some('>')) {
        return None;
    }
    let expr = rest[1..].trim();
    if expr.is_empty() {
        return None;
    }
    if !is_pure_initializer_expression(expr) {
        return None;
    }
    Some(expr.to_string())
}

fn extract_pure_return_expression(body: &str) -> Option<String> {
    let statements: Vec<&str> = top_level_statement_slices(body)
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if statements.len() != 1 {
        return None;
    }
    let stmt = statements[0].trim_end_matches(';').trim();
    if !stmt.starts_with("return") {
        return None;
    }
    // Reject "returnable" / "returns" etc. — the `return` keyword must be
    // followed by whitespace or an opening paren, not an identifier continue.
    let rest = &stmt["return".len()..];
    match rest.as_bytes().first() {
        None => return None,
        Some(byte) if is_identifier_continue(*byte) => return None,
        _ => {}
    }
    let expr = rest.trim();
    if expr.is_empty() {
        return None;
    }
    if !is_pure_initializer_expression(expr) {
        return None;
    }
    Some(expr.to_string())
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
    let mut identifiers = BTreeSet::new();
    let bytes = source.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'`' => cursor = collect_template_identifiers(source, cursor, &mut identifiers),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            byte if is_identifier_start(byte) => {
                let start = cursor;
                cursor += 1;
                while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
                    cursor += 1;
                }
                let identifier = &source[start..cursor];
                if !is_js_keyword(identifier) {
                    identifiers.insert(identifier.to_string());
                }
            }
            _ => cursor += 1,
        }
    }
    identifiers
}

fn collect_template_identifiers(
    source: &str,
    start: usize,
    identifiers: &mut BTreeSet<String>,
) -> usize {
    let bytes = source.as_bytes();
    let mut cursor = start + 1;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor += 2,
            b'`' => return cursor + 1,
            b'$' if bytes.get(cursor + 1) == Some(&b'{') => {
                let open = cursor + 1;
                let Some(close) = find_matching_brace(source, open) else {
                    return skip_quoted(bytes, start, b'`');
                };
                identifiers.extend(identifiers_in_source(&source[open + 1..close]));
                cursor = close + 1;
            }
            _ => cursor += 1,
        }
    }
    bytes.len()
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
    identifier_reference_positions(source, identifier).any(|cursor| {
        let after = skip_ws(source.as_bytes(), cursor);
        source.as_bytes().get(after) == Some(&b'(')
    })
}

fn contains_identifier_reference(source: &str, identifier: &str) -> bool {
    identifiers_in_source(source).contains(identifier)
}

fn identifier_reference_positions<'a>(
    source: &'a str,
    identifier: &'a str,
) -> impl Iterator<Item = usize> + 'a {
    let mut positions = Vec::new();
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
            byte if is_identifier_start(byte) => {
                let start = cursor;
                cursor += 1;
                while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
                    cursor += 1;
                }
                if &source[start..cursor] == identifier {
                    // `obj.X` and `obj#X` access X as a property name, not
                    // a binding reference — skip. But `...X` (spread) is
                    // a value read of X, so when the preceding byte is
                    // `.` we have to peek further: a SECOND preceding
                    // `.` indicates the trailing dot of `...` and the
                    // identifier IS a binding reference.
                    let prev = start.checked_sub(1).and_then(|index| bytes.get(index));
                    if prev == Some(&b'#') {
                        continue;
                    }
                    if prev == Some(&b'.')
                        && start.checked_sub(2).and_then(|index| bytes.get(index)) != Some(&b'.')
                    {
                        continue;
                    }
                    positions.push(cursor);
                }
            }
            _ => cursor += 1,
        }
    }
    positions.into_iter()
}

fn skip_ws(bytes: &[u8], mut cursor: usize) -> usize {
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    cursor
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$')
}

fn is_identifier_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}

fn is_js_keyword(value: &str) -> bool {
    matches!(
        value,
        "async"
            | "await"
            | "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "default"
            | "do"
            | "else"
            | "export"
            | "extends"
            | "false"
            | "finally"
            | "for"
            | "from"
            | "function"
            | "if"
            | "import"
            | "in"
            | "let"
            | "new"
            | "null"
            | "return"
            | "super"
            | "switch"
            | "this"
            | "throw"
            | "true"
            | "try"
            | "typeof"
            | "undefined"
            | "var"
            | "void"
            | "while"
            | "with"
            | "yield"
    )
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

fn skip_line_comment(bytes: &[u8], mut cursor: usize) -> usize {
    while cursor < bytes.len() && bytes[cursor] != b'\n' {
        cursor += 1;
    }
    cursor
}

fn skip_block_comment(bytes: &[u8], mut cursor: usize) -> usize {
    while cursor + 1 < bytes.len() {
        if bytes[cursor] == b'*' && bytes[cursor + 1] == b'/' {
            return cursor + 2;
        }
        cursor += 1;
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

/// Returns `true` when `snippet` is exactly `var X;`, `let X;`, or
/// `const X;` (no initializer) for the given binding. The migration plan
/// gates on this so we never silently drop an initializer when moving a
/// declaration to its writer module.
fn is_uninitialized_var_declaration(snippet: &str, binding: &str) -> bool {
    let trimmed = snippet.trim();
    for keyword in ["var", "let", "const"] {
        if let Some(rest) = trimmed.strip_prefix(keyword)
            && rest.starts_with(|c: char| c.is_ascii_whitespace())
        {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_suffix(';')
                && rest.trim() == binding
            {
                return true;
            }
        }
    }
    false
}

/// Remove `var X;` (or `var X = undefined;`) statements for each binding
/// in `bindings` from the runtime helper source. Used after Phase 10b's
/// migration plan moves the declaration to a new owner module — the
/// declaration is no longer needed in the runtime, and leaving it would
/// either create a duplicate-declaration audit failure or shadow the
/// re-exported binding from the owner module.
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
            .map(str::trim)
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
        CompilerEvidence, CompilerKind, CompilerProfile, ModuleCompilerProfile, ProgramModel,
    };

    use super::{
        CompilerRecoveryAction, ImportExportPlanner, SourceCompilerStrategy,
        inline_internal_setter_calls, lower_runtime_helpers,
    };

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

        assert!(lowered.source.contains("var hFA = lazyModule((uO) => {"));
        assert!(lowered.source.contains("var U = 1; uO.value = U;"));
        assert!(!lowered.source.contains("hFA = U("));
        assert!(lowered.uses_lazy_module);
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
        assert!(!lowered.source.contains("lazyValue("));
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
        assert!(!lowered.source.contains("lazyValue("));
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

        assert!(lowered.source.contains("var thunk = lazyValue(() => {"));
        assert!(lowered.uses_lazy_value);
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
        // boundary. Keep the lazy thunk until cross-module rewriting lands.
        assert!(lowered.source.contains("var settings = lazyValue(() => {"));
        assert!(lowered.uses_lazy_value);
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
        // Keep the lazy thunk.
        assert!(lowered.source.contains("var init = lazyValue(() => {"));
        assert!(lowered.source.contains("setup();"));
        assert!(lowered.uses_lazy_value);
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
        assert!(lowered.source.contains("var lazy = lazyValue(() => {"));
        assert!(lowered.uses_lazy_value);
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

        assert!(lowered.source.contains("var thunk = lazyValue(() => {"));
        assert!(lowered.uses_lazy_value);
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
        assert!(lowered.source.contains("var bag = lazyModule("));
        assert!(lowered.uses_lazy_module);
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

        assert!(lowered.source.contains("var api = lazyModule("));
        assert!(lowered.uses_lazy_module);
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
        assert!(lowered.source.contains("var cached = lazyModule("));
        assert!(lowered.uses_lazy_module);
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
        assert!(lowered.source.contains("var sharedConst = lazyModule("));
        assert!(lowered.uses_lazy_module);
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
        assert!(lowered.source.contains("var api = lazyModule("));
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
        assert!(!entry_source.contains("lazyValue("));
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
            producer_source_out.contains("var palette = lazyModule("),
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
        let helper = plan
            .files
            .iter()
            .find(|file| file.path == "modules/runtime/source-1-helpers.ts")
            .expect("runtime helper file should be planned");
        let entry_source = entry.body.join("\n");
        let helper_source = helper.body.join("\n");

        // `entry` is exported — delazifying would change cross-module
        // semantics. Stays a lazy thunk and pulls in `lazyModule`.
        assert!(entry_source.contains("var entry = lazyModule("));
        // `init` body is `entry();` (side effect, no return). Can't
        // hoist to module load time. Stays as a lazy thunk.
        assert!(entry_source.contains("var init = lazyValue("));
        // Helper file declares the helpers consumers still need.
        assert!(helper_source.contains("function lazyModule(factory)"));
        assert!(helper_source.contains("function lazyValue(factory)"));
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
    fn source_backed_symbol_keeps_original_emitted_binding_until_ast_rewrite_exists() {
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
        let enriched = reverts_model::EnrichedProgram::new(
            model,
            reverts_model::SemanticNameMap::default(),
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
        assert_eq!(binding.emitted.as_str(), "$F1");
        assert_eq!(plan.files[0].exports[0].binding.as_str(), "$F1");
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
        assert!(module_source.contains("lazyModule("));
        assert!(module_source.contains("lazyValue("));
        // _$* names only live inside the helper file, never the business module.
        assert!(!module_source.contains("_$cached"));
        assert!(!module_source.contains("_$init"));
        assert!(!module_source.contains("_$val"));
        assert!(!module_source.contains("_$module"));
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
                .contains("import { yA, __reverts_set_yA } from './runtime/source-1-helpers.js';")
        );
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
        assert!(helper_source.contains("export { __reverts_set_yA, main, yA };"));
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
             as.command('servers');\n\
             console.log(request.method); Promise.resolve().then(() => (packageInit(), ns));",
        );

        assert!(identifiers.contains("as"));
        assert!(identifiers.contains("packageInit"));
        assert!(identifiers.contains("request"));
        assert!(identifiers.contains("ns"));
        assert!(identifiers.contains("source"));
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
        assert!(helper_source.contains("function lazyValue(factory) {"));
        assert!(helper_source.contains("export { initShared, lazyValue, shared };"));
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
    fn runtime_prelude_binding_written_by_module_uses_live_setter() {
        let planner = ImportExportPlanner;
        let prelude = "var shared = 0;\n";
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
        assert!(helper_source.contains("export { __reverts_set_shared, shared };"));
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
        assert!(helper_source.contains("var shared;"));
        assert!(helper_source.contains("var Custom;"));
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
        assert!(helper_source.contains("export { initShared, lazyValue, shared };"));
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
        // Both bindings have explicit initializers in the prelude so the
        // Phase 10b migration plan skips them — the setter mechanism
        // remains intact and this test continues to validate the
        // update-operator rewrite path.
        let prelude = "var counter = 2;\nvar result = 0;\n";
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
        // Initialize both prelude vars so the Phase 10b migration plan
        // skips them — the destructuring rewrite path under test still
        // routes writes through the setter.
        let prelude = "var left = 0;\nvar right = 0;\n";
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
        // Initialize the prelude vars so the Phase 10b migration plan
        // leaves them in the runtime — the test still exercises the
        // setter rewrite inside a computed class key.
        let prelude = "var J = (init, value) => () => (init && (value = init(init = 0)), value);\nvar Stream = null;\nvar holder = null;\n";
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
        // Initialize d2 so Phase 10b's migration plan leaves it in the
        // runtime — this test continues to validate the
        // namespace-export setup alongside cross-module setter use.
        let prelude =
            "var zT = () => 'enum';\nvar m = {};\nM5(m, { enum: () => zT });\nvar d2 = null;\n";
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
