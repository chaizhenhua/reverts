//! Pass-oriented planner pipeline.
//!
//! The historical planner lived as one large `plan_enriched_program` body.
//! This module now contains only pass ordering and state threading; runtime
//! preparation, module-loop context, and usage accumulators live in named
//! modules so each stage has an explicit owner.

use crate::module_planning_context::ModulePlanningContext;
use crate::package_runtime::emit_package_runtime_helper_files;
use crate::package_runtime_accumulator::PackageRuntimeAccumulator;
use crate::plan_reachability::prune_plan_to_cli_reachable;
use crate::planner_context::PlannerContext;
use crate::relative_paths::relative_import_specifier;
use crate::runtime_helper_usage::RuntimeHelperUsageAccumulator;
use crate::runtime_plan_preparation::RuntimePlanPreparation;
use crate::statement_parsers::{
    NamedImportSpecifier, parse_generated_named_import_specifiers,
    parse_generated_named_reexport_statement,
};
use crate::statements::{
    named_import_alias_statement, named_reexport_statement, runtime_helpers_path,
};
use crate::{
    EmitPlan, PlanError, cli_entrypoint, runtime_entrypoint, runtime_helper_emission,
    top_level_definitions_in_source,
};
use reverts_ir::BindingName;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn run_planner_pipeline(context: &PlannerContext<'_>) -> Result<EmitPlan, PlanError> {
    let mut state = PlanningState::new(context);
    PlanModulesPass.run(context, &mut state)?;
    EmitPackageRuntimePass.run(context, &mut state)?;
    MarkEntrypointRuntimePass.run(context, &mut state)?;
    RerouteRuntimeBarrelImportsPass.run(context, &mut state)?;
    RegisterEntrypointIslandSettersPass.run(context, &mut state)?;
    EmitRuntimeHelpersPass.run(context, &mut state)?;
    EmitCliEntrypointPass.run(context, &mut state)?;
    // Lower inlined CommonJS module bodies (`require('./x')`, `module.exports`)
    // to ESM BEFORE reachability/prune, so a module reached only through a
    // `require()` becomes a real `import` edge (else it is pruned) and its
    // relative require resolves to the rerouted emitted path.
    crate::commonjs_lowering::lower_commonjs_modules_to_esm(context.program(), &mut state.plan);
    PruneUnreachableFilesPass.run(context, &mut state)?;
    PruneDeadExportsPass.run(context, &mut state)?;
    PruneInvalidExportsPass.run(context, &mut state)?;
    crate::delazify_init_chains::delazify_init_chains(&mut state.plan);
    // Layer-2 export-name recovery: rename minified bindings to their real esbuild
    // export names across every planned file (modules + entrypoint island + runtime
    // helpers). Runs last so it sees the final, pruned file set.
    let export_names = crate::compute_modules::build_namespace_export_name_map(context.program());
    crate::compute_modules::apply_export_name_renames(&mut state.plan, &export_names);
    // Final correctness completion: a binding the runtime-reader-cluster
    // migration routed into a cycle-avoidance bucket can be referenced (in call
    // position, e.g. an esbuild `__esm` init thunk in an `await import()`
    // lowering) without being bound locally, while the same module's namespace
    // IS imported. Add such referenced-but-unbound names to the EXISTING import
    // from the module that exports them — cycle-safe (augments an existing edge,
    // adds none) — so the emitted ESM has no dangling reference.
    crate::complete_referenced_imports::complete_referenced_module_imports(&mut state.plan);
    // Add a NEW import for a referenced runtime helper (e.g. esbuild's shared
    // `__esm` initializer `st`) used cross-source-file with no import — safe
    // because helpers are emitted as hoisted function declarations.
    crate::complete_runtime_helper_imports::complete_runtime_helper_imports(&mut state.plan);
    // Add a NEW import for a referenced-but-unbound name that another, ALREADY
    // coupled module top-level-defines as a function (e.g. execa's `aut` in the
    // helpers file calling `sut`/`iut`/… defined in a sliced module that imports
    // back). Load-safe: function values are used via deferred call sites, and the
    // existing reverse edge proves the two modules are one scope/bundle. The
    // missing `export` on the definer is added by the export completion below.
    crate::complete_coupled_module_function_imports::complete_coupled_module_function_imports(
        &mut state.plan,
    );
    // Add a NEW import for a referenced-but-unbound esbuild init-thunk call
    // (`cdA()`) the eager entrypoint makes to a sliced module it has no other
    // edge to. Deferred-call + same-bundle (shared runtime helpers) → cycle-safe.
    crate::complete_init_thunk_imports::complete_init_thunk_imports(&mut state.plan);
    // Symmetric completion: every name a consumer imports from a sibling module
    // must actually be exported by it. Works from the final emitted imports, so
    // it closes export gaps the def-use graph missed (esbuild scope-hoisted
    // cross-module bindings) by re-exporting from the module's externalized
    // package or exporting a locally-defined binding.
    crate::export_completion::complete_cross_module_exports(&mut state.plan);
    // Correctness: after every import-completion pass has settled the final
    // import set, ensure no file imports a binding it reassigns (ESM imports are
    // read-only). Re-home each such binding to its writer as a local `var`.
    crate::localize_written_imports::localize_written_imports(&mut state.plan);
    // Several late passes (CommonJS lowering, scope-hoisted export emission,
    // cross-module reconciliation) append `export { … }` statements into an
    // already-finalised single body chunk, which the per-chunk coalescer can no
    // longer merge — sweep the whole body text to drop any duplicate export name.
    crate::export_completion::dedupe_redundant_named_exports(&mut state.plan);
    Ok(state.plan)
}

trait PlanningPass {
    fn run(&self, context: &PlannerContext<'_>, state: &mut PlanningState)
    -> Result<(), PlanError>;
}

pub(crate) struct PlanningState {
    pub(crate) plan: EmitPlan,
    pub(crate) runtime: RuntimePlanPreparation,
    pub(crate) runtime_helpers: RuntimeHelperUsageAccumulator,
    pub(crate) package_runtime: PackageRuntimeAccumulator,
}

impl PlanningState {
    fn new(context: &PlannerContext<'_>) -> Self {
        let runtime = RuntimePlanPreparation::from_context(context);
        let runtime_helpers = RuntimeHelperUsageAccumulator::from_runtime_lazy_folds(
            &context.analysis().runtime_lazy_folds,
        );
        Self {
            plan: EmitPlan::default(),
            runtime,
            runtime_helpers,
            package_runtime: PackageRuntimeAccumulator::default(),
        }
    }
}

struct PlanModulesPass;

impl PlanningPass for PlanModulesPass {
    fn run(
        &self,
        context: &PlannerContext<'_>,
        state: &mut PlanningState,
    ) -> Result<(), PlanError> {
        ModulePlanningContext { context, state }.plan_all_modules()
    }
}

struct EmitPackageRuntimePass;

impl PlanningPass for EmitPackageRuntimePass {
    fn run(
        &self,
        context: &PlannerContext<'_>,
        state: &mut PlanningState,
    ) -> Result<(), PlanError> {
        emit_package_runtime_helper_files(
            context.program(),
            &mut state.plan,
            &state.package_runtime.used_helper_files,
            &context.analysis().externalized_packages,
        )
    }
}

struct RerouteRuntimeBarrelImportsPass;

impl PlanningPass for RerouteRuntimeBarrelImportsPass {
    fn run(
        &self,
        context: &PlannerContext<'_>,
        state: &mut PlanningState,
    ) -> Result<(), PlanError> {
        let scope = BundleScope::from_program(context.program());
        reroute_runtime_barrel_imports(&mut state.plan, &mut state.runtime_helpers, &scope);
        Ok(())
    }
}

/// Maps an emitted module path to its esbuild bundle (= its source file id) so
/// a barrel-import reroute never crosses a bundle boundary. A source file with
/// MANY sliced modules is a scope-hoisted bundle; a binding referenced inside it
/// must resolve WITHIN it. Rerouting a sliced-bundle reader's binding to an
/// owner in a DIFFERENT file (e.g. an Electron main module to a renderer
/// `ion-dist` chunk that uses `document`) is the cross-bundle leak this blocks.
/// A standalone (single-module) file keeps unrestricted reroute (explicit
/// cross-chunk imports, e.g. inside one renderer build).
struct BundleScope {
    file_of_path: BTreeMap<String, u32>,
    modules_per_file: BTreeMap<u32, usize>,
}

impl BundleScope {
    fn from_program(program: &reverts_model::EnrichedProgram) -> Self {
        let mut file_of_path = BTreeMap::new();
        let mut modules_per_file = BTreeMap::new();
        for module in program.model().modules() {
            let Some(source_file_id) = module.source_file_id else {
                continue;
            };
            *modules_per_file.entry(source_file_id).or_insert(0) += 1;
            if let Some(path) = crate::module_output_path(program, module.id) {
                file_of_path.insert(path, source_file_id);
            }
        }
        // The entrypoint island is synthetic (no module row) but belongs to the
        // main bundle's source file; map it so its barrel imports are scoped the
        // same way as that bundle's modules.
        if let Some((_prelude, entrypoint)) = crate::runtime_entrypoint(program) {
            file_of_path.insert(
                cli_entrypoint::ENTRYPOINT_ISLAND_PATH.to_string(),
                entrypoint.source_file_id,
            );
        }
        Self {
            file_of_path,
            modules_per_file,
        }
    }

    /// Whether a barrel binding may be rerouted from `reader_path` to
    /// `owner_path`. Blocked when the reader sits in a sliced bundle file and
    /// the owner is in a different file (a cross-bundle leak).
    fn allows_reroute(&self, reader_path: &str, owner_path: &str) -> bool {
        let Some(reader_file) = self.file_of_path.get(reader_path).copied() else {
            return true;
        };
        if self
            .modules_per_file
            .get(&reader_file)
            .copied()
            .unwrap_or(0)
            <= 1
        {
            return true;
        }
        self.file_of_path.get(owner_path).copied() == Some(reader_file)
    }
}

struct MarkEntrypointRuntimePass;

impl PlanningPass for MarkEntrypointRuntimePass {
    fn run(
        &self,
        context: &PlannerContext<'_>,
        state: &mut PlanningState,
    ) -> Result<(), PlanError> {
        if let Some((_prelude, entrypoint)) = runtime_entrypoint(context.program())
            && !cli_entrypoint::entrypoint_can_import_owner_directly(
                context.program(),
                &state.runtime.runtime_var_migrations,
                entrypoint.source_file_id,
                &entrypoint.callee,
            )
        {
            let occupied =
                occupied_runtime_bindings_for_entrypoint(context, state, entrypoint.source_file_id);
            if let Some(island) = cli_entrypoint::entrypoint_island_plan(
                context.program(),
                &state.runtime.binding_owners,
                &occupied,
                &context.analysis().externalized_packages,
                Some(&state.plan),
            ) {
                let runtime_bindings = island.runtime_bindings.clone();
                cli_entrypoint::emit_planned_entrypoint_island(
                    context.program(),
                    &mut state.plan,
                    island,
                );
                state
                    .runtime_helpers
                    .mark_runtime_bindings(entrypoint.source_file_id, &runtime_bindings);
            } else {
                state
                    .runtime_helpers
                    .mark_entrypoint(entrypoint.source_file_id, &entrypoint.callee);
            }
        }
        Ok(())
    }
}

/// Record the entrypoint island's writes to imported runtime bindings as setter
/// targets BEFORE the runtime helper file is emitted, so the helper declares and
/// exports `__reverts_set_X` for state the island mutates. The island itself
/// (emitted later by `EmitCliEntrypointPass`) rewrites those writes to setter
/// calls. Without this pass the island assigns to read-only ESM imports →
/// `TypeError: Assignment to constant variable` at runtime (silent paint abort).
struct RegisterEntrypointIslandSettersPass;

impl PlanningPass for RegisterEntrypointIslandSettersPass {
    fn run(
        &self,
        context: &PlannerContext<'_>,
        state: &mut PlanningState,
    ) -> Result<(), PlanError> {
        let occupied = runtime_entrypoint(context.program())
            .map(|(_prelude, entrypoint)| {
                occupied_runtime_bindings_for_entrypoint(context, state, entrypoint.source_file_id)
            })
            .unwrap_or_default();
        cli_entrypoint::register_entrypoint_island_setters(
            context.program(),
            &state.runtime.runtime_var_migrations,
            &state.runtime.binding_owners,
            &occupied,
            &context.analysis().externalized_packages,
            &state.plan,
            &mut state.runtime_helpers.used_runtime_helper_setters,
        );
        Ok(())
    }
}

struct EmitRuntimeHelpersPass;

impl PlanningPass for EmitRuntimeHelpersPass {
    fn run(
        &self,
        context: &PlannerContext<'_>,
        state: &mut PlanningState,
    ) -> Result<(), PlanError> {
        let usage = &state.runtime_helpers;
        runtime_helper_emission::emit_runtime_helper_files(
            &runtime_helper_emission::RuntimeHelperEmissionContext {
                program: context.program(),
                runtime_var_migrations: &state.runtime.runtime_var_migrations,
                binding_owners: &state.runtime.binding_owners,
                runtime_lazy_folds: &context.analysis().runtime_lazy_folds,
                externalized_packages: &context.analysis().externalized_packages,
                external_package_adapters: &context.analysis().external_package_adapters,
                used_runtime_helper_files: &usage.used_runtime_helper_files,
                exported_runtime_helper_bindings: &usage.exported_runtime_helper_bindings,
                required_runtime_helper_bindings: &usage.required_runtime_helper_bindings,
                used_runtime_helper_setters: &usage.used_runtime_helper_setters,
                used_lazy_module: &usage.used_lazy_module,
                used_lazy_value: &usage.used_lazy_value,
                exported_lazy_module: &usage.exported_lazy_module,
                exported_lazy_value: &usage.exported_lazy_value,
            },
            &mut state.plan,
        )
    }
}

struct EmitCliEntrypointPass;

impl PlanningPass for EmitCliEntrypointPass {
    fn run(
        &self,
        context: &PlannerContext<'_>,
        state: &mut PlanningState,
    ) -> Result<(), PlanError> {
        cli_entrypoint::emit_cli_entrypoint(
            context.program(),
            &state.runtime.runtime_var_migrations,
            &state.runtime.binding_owners,
            &runtime_entrypoint(context.program())
                .map(|(_prelude, entrypoint)| {
                    occupied_runtime_bindings_for_entrypoint(
                        context,
                        state,
                        entrypoint.source_file_id,
                    )
                })
                .unwrap_or_default(),
            &context.analysis().externalized_packages,
            &mut state.plan,
        );
        Ok(())
    }
}

struct PruneUnreachableFilesPass;

impl PlanningPass for PruneUnreachableFilesPass {
    fn run(
        &self,
        _context: &PlannerContext<'_>,
        state: &mut PlanningState,
    ) -> Result<(), PlanError> {
        prune_plan_to_cli_reachable(&mut state.plan);
        Ok(())
    }
}

struct PruneDeadExportsPass;

impl PlanningPass for PruneDeadExportsPass {
    fn run(
        &self,
        _context: &PlannerContext<'_>,
        state: &mut PlanningState,
    ) -> Result<(), PlanError> {
        crate::dead_export_prune::prune_dead_exports(&mut state.plan);
        Ok(())
    }
}

struct PruneInvalidExportsPass;

impl PlanningPass for PruneInvalidExportsPass {
    fn run(
        &self,
        _context: &PlannerContext<'_>,
        state: &mut PlanningState,
    ) -> Result<(), PlanError> {
        crate::dead_export_prune::prune_invalid_exports(&mut state.plan);
        Ok(())
    }
}

fn occupied_runtime_bindings_for_entrypoint(
    context: &PlannerContext<'_>,
    state: &PlanningState,
    source_file_id: u32,
) -> BTreeSet<BindingName> {
    let mut occupied = state
        .runtime_helpers
        .occupied_runtime_bindings(source_file_id);
    if let Some(chunks) = context
        .analysis()
        .runtime_lazy_folds
        .chunks_by_source_file
        .get(&source_file_id)
    {
        let prelude = context
            .program()
            .model()
            .graph()
            .runtime_prelude(source_file_id);
        for chunk in chunks {
            occupied.extend(
                top_level_definitions_in_source(chunk.source.as_str())
                    .into_iter()
                    .filter(|binding| prelude.is_none_or(|prelude| !prelude.defines(binding))),
            );
        }
    }
    occupied
}

fn reroute_runtime_barrel_imports(
    plan: &mut EmitPlan,
    runtime_helpers: &mut RuntimeHelperUsageAccumulator,
    scope: &BundleScope,
) {
    let source_file_ids = runtime_helper_usage_source_file_ids(runtime_helpers);
    if source_file_ids.is_empty() {
        return;
    }
    let owner_paths = planned_unique_non_runtime_export_owner_paths(plan);
    if owner_paths.is_empty() {
        return;
    }

    let mut rerouted_by_source = BTreeMap::<u32, BTreeSet<BindingName>>::new();
    for file in &mut plan.files {
        if file.path.starts_with("modules/runtime/") {
            continue;
        }
        let mut rewritten = Vec::with_capacity(file.body.len());
        for source in std::mem::take(&mut file.body) {
            if let Some((specifiers, import_specifier)) =
                parse_generated_named_import_specifiers(&source)
                && let Some(source_file_id) = runtime_helper_source_file_for_specifier(
                    file.path.as_str(),
                    import_specifier.as_str(),
                    &source_file_ids,
                )
            {
                let partition = partition_runtime_barrel_import_specifiers(
                    file.path.as_str(),
                    specifiers,
                    &owner_paths,
                    scope,
                );
                if partition.rerouted.is_empty() {
                    rewritten.push(source);
                    continue;
                }
                push_partitioned_runtime_barrel_imports(
                    file.path.as_str(),
                    import_specifier.as_str(),
                    &mut rewritten,
                    &partition,
                );
                rerouted_by_source
                    .entry(source_file_id)
                    .or_default()
                    .extend(partition.rerouted_bindings);
                continue;
            }

            if let Some((bindings, reexport_specifier)) =
                parse_generated_named_reexport_statement(&source)
                && let Some(source_file_id) = runtime_helper_source_file_for_specifier(
                    file.path.as_str(),
                    reexport_specifier.as_str(),
                    &source_file_ids,
                )
            {
                let partition = partition_runtime_barrel_reexports(
                    file.path.as_str(),
                    bindings,
                    &owner_paths,
                    scope,
                );
                if partition.rerouted.is_empty() {
                    rewritten.push(source);
                    continue;
                }
                push_partitioned_runtime_barrel_reexports(
                    file.path.as_str(),
                    reexport_specifier.as_str(),
                    &mut rewritten,
                    &partition,
                );
                rerouted_by_source
                    .entry(source_file_id)
                    .or_default()
                    .extend(partition.rerouted_bindings);
                continue;
            }

            rewritten.push(source);
        }
        file.body = rewritten;
    }

    for (source_file_id, rerouted) in rerouted_by_source {
        let consumed =
            runtime_helper_emission::planned_runtime_helper_consumed_bindings(plan, source_file_id);
        let removable = rerouted
            .difference(&consumed)
            .cloned()
            .collect::<BTreeSet<_>>();
        runtime_helpers.remove_runtime_bindings(source_file_id, &removable);
    }
}

fn runtime_helper_usage_source_file_ids(
    runtime_helpers: &RuntimeHelperUsageAccumulator,
) -> BTreeSet<u32> {
    let mut source_file_ids = BTreeSet::new();
    source_file_ids.extend(runtime_helpers.used_runtime_helper_files.keys().copied());
    source_file_ids.extend(
        runtime_helpers
            .exported_runtime_helper_bindings
            .keys()
            .copied(),
    );
    source_file_ids.extend(
        runtime_helpers
            .required_runtime_helper_bindings
            .keys()
            .copied(),
    );
    source_file_ids.extend(runtime_helpers.used_runtime_helper_setters.keys().copied());
    source_file_ids
}

fn planned_unique_non_runtime_export_owner_paths(
    plan: &EmitPlan,
) -> BTreeMap<BindingName, Option<String>> {
    let mut owners = BTreeMap::<BindingName, Option<String>>::new();
    for file in &plan.files {
        if file.path == "cli.ts"
            || file.path.starts_with("modules/runtime/")
            || file
                .body
                .iter()
                .any(|source| source.contains("runtime/source-"))
        {
            continue;
        }
        for export in &file.exports {
            owners
                .entry(export.binding.clone())
                .and_modify(|owner| {
                    if owner.as_ref().is_none_or(|path| path != &file.path) {
                        *owner = None;
                    }
                })
                .or_insert_with(|| Some(file.path.clone()));
        }
    }
    owners
}

fn runtime_helper_source_file_for_specifier(
    file_path: &str,
    specifier: &str,
    source_file_ids: &BTreeSet<u32>,
) -> Option<u32> {
    source_file_ids.iter().copied().find(|source_file_id| {
        relative_import_specifier(file_path, runtime_helpers_path(*source_file_id).as_str())
            == specifier
    })
}

#[derive(Default)]
struct RuntimeBarrelImportPartition {
    remaining: Vec<NamedImportSpecifier>,
    rerouted: BTreeMap<String, Vec<NamedImportSpecifier>>,
    rerouted_bindings: BTreeSet<BindingName>,
}

fn partition_runtime_barrel_import_specifiers(
    file_path: &str,
    specifiers: Vec<NamedImportSpecifier>,
    owner_paths: &BTreeMap<BindingName, Option<String>>,
    scope: &BundleScope,
) -> RuntimeBarrelImportPartition {
    let mut partition = RuntimeBarrelImportPartition::default();
    for specifier in specifiers {
        if specifier.imported.as_str().starts_with("__reverts_set_") {
            partition.remaining.push(specifier);
            continue;
        }
        match owner_paths.get(&specifier.imported).and_then(Clone::clone) {
            Some(owner_path)
                if owner_path != file_path && scope.allows_reroute(file_path, &owner_path) =>
            {
                partition
                    .rerouted
                    .entry(owner_path)
                    .or_default()
                    .push(specifier.clone());
                partition.rerouted_bindings.insert(specifier.imported);
            }
            _ => partition.remaining.push(specifier),
        }
    }
    partition
}

fn push_partitioned_runtime_barrel_imports(
    file_path: &str,
    runtime_specifier: &str,
    body: &mut Vec<String>,
    partition: &RuntimeBarrelImportPartition,
) {
    if !partition.remaining.is_empty() {
        body.push(import_specifier_statement(
            &partition.remaining,
            runtime_specifier,
        ));
    }
    for (owner_path, specifiers) in &partition.rerouted {
        let specifier = relative_import_specifier(file_path, owner_path);
        body.push(import_specifier_statement(specifiers, specifier.as_str()));
    }
}

fn import_specifier_statement(specifiers: &[NamedImportSpecifier], source: &str) -> String {
    named_import_alias_statement(
        specifiers
            .iter()
            .map(|specifier| (specifier.imported.as_str(), &specifier.local)),
        source,
    )
}

#[derive(Default)]
struct RuntimeBarrelReexportPartition {
    remaining: BTreeSet<BindingName>,
    rerouted: BTreeMap<String, BTreeSet<BindingName>>,
    rerouted_bindings: BTreeSet<BindingName>,
}

fn partition_runtime_barrel_reexports(
    file_path: &str,
    bindings: BTreeSet<BindingName>,
    owner_paths: &BTreeMap<BindingName, Option<String>>,
    scope: &BundleScope,
) -> RuntimeBarrelReexportPartition {
    let mut partition = RuntimeBarrelReexportPartition::default();
    for binding in bindings {
        match owner_paths.get(&binding).and_then(Clone::clone) {
            Some(owner_path)
                if owner_path != file_path && scope.allows_reroute(file_path, &owner_path) =>
            {
                partition
                    .rerouted
                    .entry(owner_path)
                    .or_default()
                    .insert(binding.clone());
                partition.rerouted_bindings.insert(binding);
            }
            _ => {
                partition.remaining.insert(binding);
            }
        }
    }
    partition
}

fn push_partitioned_runtime_barrel_reexports(
    file_path: &str,
    runtime_specifier: &str,
    body: &mut Vec<String>,
    partition: &RuntimeBarrelReexportPartition,
) {
    if !partition.remaining.is_empty() {
        body.push(named_reexport_statement(
            partition.remaining.iter(),
            runtime_specifier,
        ));
    }
    for (owner_path, bindings) in &partition.rerouted {
        let specifier = relative_import_specifier(file_path, owner_path);
        body.push(named_reexport_statement(
            bindings.iter(),
            specifier.as_str(),
        ));
    }
}
