use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use reverts_graph::{
    RevertsGraph, RuntimeEntrypoint, RuntimeNamespaceExport, RuntimePrelude,
    RuntimePreludeBindingKind, RuntimePreludeImport,
};
use reverts_input::{ModuleDependencyTarget, PackageAttributionStatus, PackageEmissionMode};
use reverts_ir::{BindingName, BindingShape, ModuleId, ModuleKind};
use reverts_js::{ParseGoal, format_source_pretty, parse_error_message};
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
) -> PlannedBinding {
    let shape = program.binding_shape(module_id, original.as_str());
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
        let accepted_externalized_packages = externalized_package_modules(program);
        let source_required_packages =
            source_required_package_modules(program, &accepted_externalized_packages);
        let externalized_packages = accepted_externalized_packages
            .difference(&source_required_packages)
            .copied()
            .collect::<BTreeSet<_>>();
        let source_module_wiring = source_module_wiring(program, &externalized_packages);
        let lowered_runtime_sources = lowered_runtime_sources(program, &source_module_wiring);
        let runtime_lazy_folds =
            runtime_lazy_fold_plan(program, &source_module_wiring, &lowered_runtime_sources);

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

            let runtime_import_groups = group_runtime_imports(runtime_imports);
            if let Some(lowered_source) = lowered_source
                && !remaining_runtime_helpers.is_empty()
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
                let specifier = relative_import_specifier(
                    path,
                    runtime_helpers_path(lowered_source.source_file_id).as_str(),
                );
                file.push_source(runtime_helper_import_statement(
                    &remaining_runtime_helpers,
                    &written_runtime_helpers,
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
                    ));
                }
            }

            let source_definitions = program.model().graph().ast_definitions_for(module.id);
            let source_imports = program.model().graph().ast_imports_for(module.id);
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
                planned_bindings.insert(original.clone());
                file.add_binding(plan_binding_from_program(
                    program,
                    module.id,
                    original,
                    emitted,
                    source_backed,
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
                let implicit_globals = implicit_global_declarations_for_module(
                    source.as_str(),
                    &source_definitions,
                    &source_imports,
                    &planned_bindings,
                );
                if !implicit_globals.is_empty() {
                    file.push_source(variable_declaration_statement(implicit_globals.iter()));
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
            let helper_closure =
                close_runtime_helper_source(prelude, &root_bindings, entrypoint, folded_chunks);
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
            for binding in &setter_bindings {
                file.push_source(runtime_helper_setter_declaration(binding));
            }
            let mut exported_bindings = helper_bindings.clone();
            exported_bindings.extend(
                setter_bindings
                    .iter()
                    .map(|binding| BindingName::new(runtime_helper_setter_name(binding))),
            );
            file.push_source(named_export_statement(exported_bindings.iter()));
            for binding in helper_bindings.iter().cloned() {
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
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct RuntimeLazyFoldPlan {
    modules: BTreeMap<ModuleId, RuntimeLazyFoldModule>,
    chunks_by_source_file: BTreeMap<u32, Vec<RuntimeFoldedSourceChunk>>,
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
        let lowering = lower_runtime_helpers(source.source, &helper_kinds);
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
    let marker = "__reverts_value";
    let mut search_from = 0usize;
    let equals = loop {
        let marker_start = initializer[search_from..].find(marker)? + search_from;
        let equals = skip_ws(initializer.as_bytes(), marker_start + marker.len());
        if initializer.as_bytes().get(equals) == Some(&b'=') {
            break equals;
        }
        search_from = marker_start + marker.len();
    };
    let mut cursor = skip_ws(initializer.as_bytes(), equals + 1);
    if initializer.as_bytes().get(cursor) != Some(&b'(') {
        return None;
    }
    cursor = skip_ws(initializer.as_bytes(), cursor + 1);
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
    let call_start = skip_ws(initializer.as_bytes(), after_body + 1);
    if initializer.as_bytes().get(call_start) != Some(&b'(')
        || initializer
            .as_bytes()
            .get(skip_ws(initializer.as_bytes(), call_start + 1))
            != Some(&b')')
    {
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
    compact.starts_with(
        "(()=>{let__reverts_initialized=false;let__reverts_value;return()=>{if(!__reverts_initialized){__reverts_initialized=true;__reverts_value=(()=>{",
    ) && compact.contains("})();}return__reverts_value;};})()")
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
        .filter(|binding| !binding.as_str().starts_with("__reverts_"))
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
}

fn lower_runtime_helpers(
    source: &str,
    helper_kinds: &BTreeMap<BindingName, RuntimePreludeBindingKind>,
) -> RuntimeHelperLowering {
    let mut lowered = source.to_string();
    let mut lowered_helpers = BTreeSet::new();
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
    RuntimeHelperLowering {
        source: lowered,
        lowered_helpers,
        remaining_helpers,
    }
}

fn lower_commonjs_wrapper_helper(source: &str, helper: &str) -> Option<String> {
    lower_helper_declarations(source, helper, HelperDeclarationKind::CommonJsWrapper)
}

fn lower_lazy_initializer_helper(source: &str, helper: &str) -> Option<String> {
    lower_helper_declarations(source, helper, HelperDeclarationKind::LazyInitializer)
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
    let module_binding = module_alias
        .map(|module| format!("var {module} = __reverts_current_module;\n"))
        .unwrap_or_default();
    Some((
        format!(
            "var {binding} = (() => {{\nlet __reverts_cached_module;\nreturn () => {{\nif (__reverts_cached_module) return __reverts_cached_module.exports;\nvar __reverts_current_module = __reverts_cached_module = {{ exports: {{}} }};\nvar {exports} = __reverts_current_module.exports;\n{module_binding}(() => {{\n{body}\n}})();\nreturn __reverts_current_module.exports;\n}};\n}})();"
        ),
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
        format!(
            "var {binding} = (() => {{\nlet __reverts_initialized = false;\nlet __reverts_value;\nreturn () => {{\nif (!__reverts_initialized) {{\n__reverts_initialized = true;\n__reverts_value = (() => {{\n{body}\n}})();\n}}\nreturn __reverts_value;\n}};\n}})();"
        ),
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
        .filter(|binding| !binding.as_str().starts_with("__reverts_"))
        .collect()
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
                "{}(__reverts_destructure{})",
                runtime_helper_setter_name(&binding),
                property_access_source(key.as_str())
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Some((
        rhs_end,
        format!(
            "(() => {{ const __reverts_destructure = {rhs}; {assignments}; return __reverts_destructure; }})()"
        ),
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
        .map(|(index, binding)| {
            format!(
                "{}(__reverts_destructure[{index}])",
                runtime_helper_setter_name(&binding)
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Some((
        rhs_end,
        format!(
            "(() => {{ const __reverts_destructure = {rhs}; {assignments}; return __reverts_destructure; }})()"
        ),
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
            "(() => {{ let __reverts_update = {binding_name}; let __reverts_next = {operator}__reverts_update; {setter}(__reverts_update); return __reverts_next; }})()",
            operator = operator.source()
        ),
        UpdatePosition::Postfix => format!(
            "(() => {{ let __reverts_update = {binding_name}; let __reverts_previous = __reverts_update{operator}; {setter}(__reverts_update); return __reverts_previous; }})()",
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
                    if start
                        .checked_sub(1)
                        .and_then(|index| bytes.get(index))
                        .is_some_and(|byte| matches!(*byte, b'#' | b'.'))
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
    specifier: &str,
) -> String {
    let mut names = bindings
        .iter()
        .map(|binding| binding.as_str().to_string())
        .collect::<Vec<_>>();
    names.extend(setter_bindings.iter().map(runtime_helper_setter_name));
    format!("import {{ {} }} from '{specifier}';", names.join(", "))
}

fn runtime_helper_setter_name(binding: &BindingName) -> String {
    format!("__reverts_set_{}", binding.as_str())
}

fn runtime_helper_setter_declaration(binding: &BindingName) -> String {
    let setter = runtime_helper_setter_name(binding);
    let binding = binding.as_str();
    format!("function {setter}(value) {{ {binding} = value; return value; }}")
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
    "import { createRequire as __reverts_createRequire } from 'node:module';\nvar require = __reverts_createRequire(import.meta.url);".to_string()
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
        CompilerRecoveryAction, ImportExportPlanner, SourceCompilerStrategy, lower_runtime_helpers,
    };

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

        let lowered = lower_runtime_helpers(source, &helper_kinds);

        assert!(lowered.source.contains("__reverts_cached_module"));
        assert!(
            lowered
                .source
                .contains("var uO = __reverts_current_module.exports;")
        );
        assert!(!lowered.source.contains("hFA = U("));
        assert!(lowered.lowered_helpers.contains(&BindingName::new("U")));
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

        let lowered = lower_runtime_helpers(source, &helper_kinds);

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
        assert!(module_source.contains("__reverts_cached_module"));
        assert!(module_source.contains("__reverts_initialized"));
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
        assert!(!helper_source.contains("__reverts_initialized"));
        assert!(!helper_source.contains("__reverts_value"));
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
        assert!(helper_source.contains("var initShared = (() => {"));
        assert!(helper_source.contains("__reverts_initialized"));
        assert!(helper_source.contains("shared = Date.now()"));
        assert!(!helper_source.contains("__reverts_set_shared"));
        assert!(helper_source.contains("export { initShared, shared };"));
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
        let prelude = "var counter = 2;\nvar result;\n";
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
        assert!(entry_source.contains("let __reverts_previous = __reverts_update--;"));
        assert!(entry_source.contains("let __reverts_next = ++__reverts_update;"));
        assert!(entry_source.contains("__reverts_set_counter(__reverts_update);"));
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
        let prelude = "var left, right;\n";
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
        assert!(entry_source.contains("__reverts_set_left(__reverts_destructure[0]);"));
        assert!(entry_source.contains("__reverts_set_right(__reverts_destructure[1]);"));
        assert!(!entry_source.contains("[left, right] ="));
    }

    #[test]
    fn runtime_prelude_write_inside_computed_class_key_uses_live_setter() {
        let planner = ImportExportPlanner;
        let prelude = "var J = (init, value) => () => (init && (value = init(init = 0)), value);\nvar Stream;\nvar holder;\n";
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
        let prelude = "var zT = () => 'enum';\nvar m = {};\nM5(m, { enum: () => zT });\nvar d2;\n";
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

        assert!(
            entry_source.contains(
                "import { createRequire as __reverts_createRequire } from 'node:module';"
            )
        );
        assert!(entry_source.contains("var require = __reverts_createRequire(import.meta.url);"));
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

        assert!(entry_source.contains("var require = __reverts_createRequire(import.meta.url);"));
        assert!(!entry_source.contains("const require ="));
        assert!(!entry_source.contains("var require;\n"));
    }
}
