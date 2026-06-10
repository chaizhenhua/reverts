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
        }
    }
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
        let mut used_runtime_helper_setters = BTreeMap::<u32, BTreeSet<BindingName>>::new();
        let accepted_externalized_packages = externalized_package_modules(program);
        let source_required_packages =
            source_required_package_modules(program, &accepted_externalized_packages);
        let externalized_packages = accepted_externalized_packages
            .difference(&source_required_packages)
            .copied()
            .collect::<BTreeSet<_>>();
        let source_module_wiring = source_module_wiring(program, &externalized_packages);

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
                        file.add_binding(PlannedBinding::new(
                            binding.clone(),
                            binding.clone(),
                            BindingShape::Unknown,
                            true,
                        ));
                    }
                }
            }

            let runtime_imports = program.model().graph().runtime_imports_for(module.id);
            let source = program.model().input().module_source_slice(module.id);
            let (lowered_source, lowered_helpers, remaining_runtime_helpers) = source.map_or_else(
                || (None, BTreeSet::new(), BTreeSet::new()),
                |source| {
                    let mut helper_kinds =
                        runtime_helper_kinds(program.model().graph(), &runtime_imports);
                    helper_kinds.extend(runtime_helper_kinds_for_source(
                        program.model().graph(),
                        source.source_file_id,
                        source.source,
                    ));
                    let lowering = lower_runtime_helpers(source.source, &helper_kinds);
                    (
                        Some((
                            source.source_file_id,
                            source.source_file_path,
                            lowering.source,
                        )),
                        lowering.lowered_helpers,
                        lowering.remaining_helpers,
                    )
                },
            );
            let local_source_definitions = lowered_source
                .as_ref()
                .map(|(_, _, source)| top_level_definitions_in_source(source.as_str()))
                .unwrap_or_default();
            let local_source_writes = lowered_source
                .as_ref()
                .map(|(_, _, source)| implicit_global_writes_in_source(source.as_str()))
                .unwrap_or_default();
            let remaining_runtime_helpers = remaining_runtime_helpers
                .into_iter()
                .filter(|binding| !planned_bindings.contains(binding))
                .filter(|binding| !local_source_definitions.contains(binding))
                .collect::<BTreeSet<_>>();
            let written_runtime_helpers = remaining_runtime_helpers
                .intersection(&local_source_writes)
                .cloned()
                .collect::<BTreeSet<_>>();

            let runtime_import_groups = group_runtime_imports(runtime_imports);
            if let Some((source_file_id, _, _)) = &lowered_source
                && !remaining_runtime_helpers.is_empty()
            {
                used_runtime_helper_files
                    .entry(*source_file_id)
                    .or_default()
                    .extend(remaining_runtime_helpers.iter().cloned());
                if !written_runtime_helpers.is_empty() {
                    used_runtime_helper_setters
                        .entry(*source_file_id)
                        .or_default()
                        .extend(written_runtime_helpers.iter().cloned());
                }
                let specifier =
                    relative_import_specifier(path, runtime_helpers_path(*source_file_id).as_str());
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
                    file.add_binding(PlannedBinding::new(
                        binding.clone(),
                        binding.clone(),
                        BindingShape::Unknown,
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
                    file.add_binding(PlannedBinding::new(
                        binding.clone(),
                        binding,
                        BindingShape::Unknown,
                        true,
                    ));
                }
            }

            let source_definitions = program.model().graph().ast_definitions_for(module.id);
            let source_imports = program.model().graph().ast_imports_for(module.id);
            for original in program.model().graph().definitions_for(module.id) {
                let shape = program.binding_shape(module.id, original.as_str());
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
                file.add_binding(PlannedBinding::new(original, emitted, shape, source_backed));
            }

            for original in &source_imports {
                if planned_bindings.contains(original) {
                    continue;
                }
                file.add_binding(PlannedBinding::new(
                    original.clone(),
                    original.clone(),
                    BindingShape::Unknown,
                    true,
                ));
            }

            if let Some((_source_file_id, source_file_path, mut source)) = lowered_source {
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
            let mut root_bindings = helper_bindings.clone();
            if let Some(entrypoint) = entrypoint {
                root_bindings.extend(runtime_entrypoint_root_bindings(prelude, entrypoint));
            }
            let mut source_bindings = prelude.required_bindings_for(root_bindings.iter());
            let namespace_exports =
                runtime_namespace_exports_for_helpers(prelude, &source_bindings);
            for namespace_export in &namespace_exports {
                source_bindings.extend(namespace_export.exports.values().cloned());
            }
            source_bindings = prelude.required_bindings_for(source_bindings.iter());
            let helper_source = entrypoint.map_or_else(
                || prelude.source_for_bindings(source_bindings.iter()),
                |entrypoint| {
                    runtime_entrypoint_helper_source(prelude, &source_bindings, entrypoint)
                },
            );
            let helper_imports = runtime_source_module_imports(
                program,
                prelude,
                helper_source.as_str(),
                &externalized_packages,
            );
            let helper_path = runtime_helpers_path(*source_file_id);
            for (module_id, bindings) in &helper_imports {
                ensure_planned_module_exports(&mut plan, program, *module_id, bindings);
                let Some(module_path) = module_output_path(program, *module_id) else {
                    continue;
                };
                let specifier =
                    relative_import_specifier(helper_path.as_str(), module_path.as_str());
                file.push_source(named_import_statement(bindings.iter(), specifier.as_str()));
            }
            file.push_source(helper_source);
            for namespace_export in &namespace_exports {
                file.push_source(runtime_namespace_export_statement(namespace_export));
            }
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

fn runtime_entrypoint_helper_source(
    prelude: &RuntimePrelude,
    source_bindings: &BTreeSet<BindingName>,
    entrypoint: &RuntimeEntrypoint,
) -> String {
    let mut chunks = BTreeMap::<(u32, u8), String>::new();
    for binding in source_bindings {
        let Some(snippet) = prelude.snippets.get(binding) else {
            continue;
        };
        chunks
            .entry((snippet.byte_start, 0))
            .or_insert_with(|| snippet.source.clone());
    }
    for side_effect in runtime_entrypoint_side_effects(prelude, entrypoint) {
        chunks
            .entry((side_effect.byte_start, 1))
            .or_insert(side_effect.source);
    }
    chunks.into_values().collect::<Vec<_>>().join("\n")
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
    prelude: &reverts_graph::RuntimePrelude,
    source: &str,
    externalized_packages: &BTreeSet<ModuleId>,
) -> BTreeMap<ModuleId, BTreeSet<BindingName>> {
    let definition_modules = unique_source_definition_modules(program, externalized_packages);
    let mut imports = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
    for identifier in runtime_import_identifiers_in_source(source) {
        let binding = BindingName::new(identifier);
        if prelude.defines(&binding) {
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

fn runtime_import_identifiers_in_source(source: &str) -> BTreeSet<String> {
    let local_bindings = local_bindings_in_source(source);
    identifiers_in_source(source)
        .into_iter()
        .filter(|identifier| !is_runtime_global_identifier(identifier))
        .filter(|identifier| !local_bindings.contains(identifier))
        .filter(|identifier| contains_value_identifier_reference(source, identifier))
        .collect()
}

fn contains_value_identifier_reference(source: &str, identifier: &str) -> bool {
    let mut cursor = 0usize;
    while let Some(start) = find_identifier_occurrence(source, identifier, cursor) {
        cursor = start + identifier.len();
        if identifier_occurrence_is_value_reference(source, start, cursor) {
            return true;
        }
    }
    false
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
                b'(' | b'[' | b'{' => {
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

fn find_identifier_occurrence(source: &str, identifier: &str, from: usize) -> Option<usize> {
    let mut cursor = from;
    while let Some(relative) = source[cursor..].find(identifier) {
        let start = cursor + relative;
        let end = start + identifier.len();
        let before = start
            .checked_sub(1)
            .and_then(|index| source.as_bytes().get(index))
            .copied();
        let after = source.as_bytes().get(end).copied();
        if before.is_none_or(|byte| !is_identifier_continue(byte))
            && after.is_none_or(|byte| !is_identifier_continue(byte))
        {
            return Some(start);
        }
        cursor = end;
    }
    None
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
            let target_bindings = source_exportable_bindings(program, target_module_id);
            if candidate_reads.is_disjoint(&target_bindings) {
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
        let target_bindings = source_exportable_bindings(program, target_module_id);
        let imported_bindings = candidate_reads
            .intersection(&target_bindings)
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
        }
    }
}

impl Error for PlanError {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use reverts_graph::RuntimePreludeBindingKind;
    use reverts_input::{
        InputBundle, InputRows, ModuleDependencyInput, ModuleDependencyTarget, ModuleInput,
        PackageAttributionInput, PackageEmissionMode, ProjectInput, SourceFileInput, SourceSpan,
        SymbolInput,
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
             class Transport { buffer = Buffer.alloc(0); async start() {} stop() {} }\n\
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
        assert!(!identifiers.contains("start"));
        assert!(!identifiers.contains("stop"));
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
            .push(PackageAttributionInput::proposed(
                ModuleId(2),
                "fixture-helper",
                Some("1.0.0".to_string()),
                PackageEmissionMode::ApplicationSource,
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
        let body = "var init = J(() => { Stream = class Stream { [(holder = new WeakMap(), Symbol.iterator)]() { return 1; } }; });\nexport { Stream, init };\n";
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
