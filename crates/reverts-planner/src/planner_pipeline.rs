//! Pass-oriented planner pipeline.
//!
//! The historical planner lived as one large `plan_enriched_program` body.
//! This module makes the top-level phases explicit: derived runtime/package
//! plans are prepared once, then named passes mutate a shared planning state.

use std::collections::{BTreeMap, BTreeSet};

use reverts_ir::{BindingName, ModuleId};

use crate::binding_owner::BindingOwnerPlan;
use crate::package_runtime::{
    PackageRuntimeHelperKey, PackageRuntimeHelperUsage, emit_package_runtime_helper_files,
    package_runtime_island_plan,
};
use crate::planner_context::PlannerContext;
use crate::pure_reexport_bypass::{
    PureReexportBypassPlan, folded_stub_modules_with_internal_consumers, pure_reexport_bypass_plan,
};
use crate::runtime_singleton_inline::{RuntimeSingletonInlinePlan, runtime_singleton_inline_plan};
use crate::runtime_var_migration::{RuntimeVarMigrationPlan, compute_runtime_var_migration_plan};
use crate::{
    EmitPlan, PlanError, RuntimePreludeDirectImport, cli_entrypoint, compute_modules,
    detect_folded_lazy_helper_use, runtime_edge_direct_prelude_imports, runtime_entrypoint,
    runtime_helper_emission, runtime_prelude_direct_imports,
};

pub(crate) fn run_planner_pipeline(context: &PlannerContext<'_>) -> Result<EmitPlan, PlanError> {
    let mut state = PlanningState::new(context);
    PlanModulesPass.run(context, &mut state)?;
    EmitPackageRuntimePass.run(context, &mut state)?;
    MarkEntrypointRuntimePass.run(context, &mut state)?;
    EmitRuntimeHelpersPass.run(context, &mut state)?;
    EmitCliEntrypointPass.run(context, &mut state)?;
    Ok(state.plan)
}

trait PlanningPass {
    fn run(&self, context: &PlannerContext<'_>, state: &mut PlanningState)
    -> Result<(), PlanError>;
}

struct PlanningState {
    plan: EmitPlan,
    runtime: RuntimePlanPreparation,
    runtime_helpers: RuntimeHelperUsageAccumulator,
    package_runtime: PackageRuntimeAccumulator,
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

/// Planner-wide derived runtime/package decisions prepared once before any
/// mutating pass runs. Keeping these immutable separates expensive analysis
/// from stateful emission bookkeeping.
struct RuntimePlanPreparation {
    omitted_folded_stub_modules: BTreeSet<ModuleId>,
    pure_reexport_bypasses: PureReexportBypassPlan,
    runtime_var_migrations: RuntimeVarMigrationPlan,
    runtime_prelude_direct_imports:
        BTreeMap<u32, BTreeMap<BindingName, RuntimePreludeDirectImport>>,
    runtime_singleton_inlines: RuntimeSingletonInlinePlan,
    runtime_edge_direct_prelude_imports: BTreeMap<u32, BTreeSet<BindingName>>,
    binding_owners: BindingOwnerPlan,
}

impl RuntimePlanPreparation {
    fn from_context(context: &PlannerContext<'_>) -> Self {
        let program = context.program();
        let analysis = context.analysis();
        let source_module_wiring = &analysis.source_module_wiring;
        let lowered_runtime_sources = &analysis.lowered_runtime_sources;
        let runtime_lazy_folds = &analysis.runtime_lazy_folds;
        let source_suppressed_packages = &analysis.source_suppressed_packages;
        let externalized_packages = &analysis.externalized_packages;

        let omitted_folded_stub_modules =
            folded_stub_modules_with_internal_consumers(runtime_lazy_folds, source_module_wiring);
        let pure_reexport_bypasses =
            pure_reexport_bypass_plan(program, source_module_wiring, externalized_packages);
        let runtime_var_migrations = compute_runtime_var_migration_plan(
            program,
            source_module_wiring,
            lowered_runtime_sources,
            runtime_lazy_folds,
            source_suppressed_packages,
        );
        let package_runtime_islands = package_runtime_island_plan(
            program,
            lowered_runtime_sources,
            runtime_lazy_folds,
            &runtime_var_migrations,
            source_suppressed_packages,
        );
        let runtime_prelude_direct_imports = runtime_prelude_direct_imports(program);
        let runtime_singleton_inlines = runtime_singleton_inline_plan(
            program,
            source_module_wiring,
            lowered_runtime_sources,
            runtime_lazy_folds,
            &runtime_var_migrations,
            &runtime_prelude_direct_imports,
            source_suppressed_packages,
        );
        let runtime_edge_direct_prelude_imports = runtime_edge_direct_prelude_imports(
            program,
            lowered_runtime_sources,
            runtime_lazy_folds,
            &runtime_prelude_direct_imports,
        );
        let binding_owners = BindingOwnerPlan::from_parts(
            &runtime_var_migrations,
            &runtime_prelude_direct_imports,
            &package_runtime_islands,
        );

        Self {
            omitted_folded_stub_modules,
            pure_reexport_bypasses,
            runtime_var_migrations,
            runtime_prelude_direct_imports,
            runtime_singleton_inlines,
            runtime_edge_direct_prelude_imports,
            binding_owners,
        }
    }
}

/// Mutable usage discovered while module files are planned. Runtime helper
/// emission consumes this accumulator after all modules have had a chance to
/// request helper files, setters, and lazy wrappers.
#[derive(Default)]
struct RuntimeHelperUsageAccumulator {
    used_runtime_helper_files: BTreeMap<u32, BTreeSet<BindingName>>,
    exported_runtime_helper_bindings: BTreeMap<u32, BTreeSet<BindingName>>,
    required_runtime_helper_bindings: BTreeMap<u32, BTreeSet<BindingName>>,
    used_runtime_helper_setters: BTreeMap<u32, BTreeSet<BindingName>>,
    used_lazy_module: BTreeSet<u32>,
    used_lazy_value: BTreeSet<u32>,
    exported_lazy_module: BTreeSet<u32>,
    exported_lazy_value: BTreeSet<u32>,
}

impl RuntimeHelperUsageAccumulator {
    fn from_runtime_lazy_folds(runtime_lazy_folds: &crate::RuntimeLazyFoldPlan) -> Self {
        let mut usage = Self::default();
        detect_folded_lazy_helper_use(
            runtime_lazy_folds,
            &mut usage.used_lazy_module,
            &mut usage.used_lazy_value,
        );
        usage
    }

    fn mark_entrypoint(&mut self, source_file_id: u32, callee: &BindingName) {
        self.used_runtime_helper_files
            .entry(source_file_id)
            .or_default()
            .insert(callee.clone());
        self.exported_runtime_helper_bindings
            .entry(source_file_id)
            .or_default()
            .insert(callee.clone());
        self.required_runtime_helper_bindings
            .entry(source_file_id)
            .or_default()
            .insert(callee.clone());
    }
}

/// Package-runtime helper usage discovered during module planning.
#[derive(Default)]
struct PackageRuntimeAccumulator {
    used_helper_files: BTreeMap<PackageRuntimeHelperKey, PackageRuntimeHelperUsage>,
}

struct ModulePlanningContext<'a> {
    context: &'a PlannerContext<'a>,
    state: &'a mut PlanningState,
}

impl ModulePlanningContext<'_> {
    fn plan_all_modules(&mut self) -> Result<(), PlanError> {
        let analysis = self.context.analysis();
        for module in self.context.program().model().modules() {
            compute_modules::plan_one_module(
                self.context.program(),
                module,
                &mut self.state.plan,
                &mut self.state.runtime_helpers.used_runtime_helper_files,
                &mut self.state.runtime_helpers.exported_runtime_helper_bindings,
                &mut self.state.runtime_helpers.required_runtime_helper_bindings,
                &mut self.state.runtime_helpers.used_runtime_helper_setters,
                &mut self.state.runtime_helpers.used_lazy_module,
                &mut self.state.runtime_helpers.used_lazy_value,
                &mut self.state.runtime_helpers.exported_lazy_module,
                &mut self.state.runtime_helpers.exported_lazy_value,
                &mut self.state.package_runtime.used_helper_files,
                &analysis.external_package_adapters,
                &analysis.externalized_packages,
                &analysis.source_suppressed_packages,
                &analysis.source_module_wiring,
                &analysis.lowered_runtime_sources,
                &analysis.runtime_lazy_folds,
                &self.state.runtime.omitted_folded_stub_modules,
                &self.state.runtime.pure_reexport_bypasses,
                &self.state.runtime.runtime_var_migrations,
                &self.state.runtime.runtime_prelude_direct_imports,
                &self.state.runtime.runtime_singleton_inlines,
                &self.state.runtime.runtime_edge_direct_prelude_imports,
                &self.state.runtime.binding_owners,
            )?;
        }
        Ok(())
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

struct MarkEntrypointRuntimePass;

impl PlanningPass for MarkEntrypointRuntimePass {
    fn run(
        &self,
        context: &PlannerContext<'_>,
        state: &mut PlanningState,
    ) -> Result<(), PlanError> {
        if let Some((_prelude, entrypoint)) = runtime_entrypoint(context.program()) {
            state
                .runtime_helpers
                .mark_entrypoint(entrypoint.source_file_id, &entrypoint.callee);
        }
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
        cli_entrypoint::emit_cli_entrypoint(context.program(), &mut state.plan);
        Ok(())
    }
}
