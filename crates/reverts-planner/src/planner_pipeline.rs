//! Pass-oriented planner pipeline.
//!
//! The historical planner lived as one large `plan_enriched_program` body.
//! This module now contains only pass ordering and state threading; runtime
//! preparation, module-loop context, and usage accumulators live in named
//! modules so each stage has an explicit owner.

use crate::module_planning_context::ModulePlanningContext;
use crate::package_runtime::emit_package_runtime_helper_files;
use crate::package_runtime_accumulator::PackageRuntimeAccumulator;
use crate::planner_context::PlannerContext;
use crate::runtime_helper_usage::RuntimeHelperUsageAccumulator;
use crate::runtime_plan_preparation::RuntimePlanPreparation;
use crate::{EmitPlan, PlanError, cli_entrypoint, runtime_entrypoint, runtime_helper_emission};

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
