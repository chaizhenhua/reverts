//! Per-module planning pass context.
//!
//! This module is the seam between the top-level pass pipeline and the legacy
//! `compute_modules` implementation. The top-level pass owns ordering; this
//! context owns translating planner state into the typed module-planning input
//! and accumulator bundles.

use crate::planner_context::PlannerContext;
use crate::planner_pipeline::PlanningState;
use crate::{PlanError, compute_modules};

pub(crate) struct ModulePlanningContext<'a> {
    pub(crate) context: &'a PlannerContext<'a>,
    pub(crate) state: &'a mut PlanningState,
}

impl ModulePlanningContext<'_> {
    pub(crate) fn plan_all_modules(&mut self) -> Result<(), PlanError> {
        let analysis = self.context.analysis();
        for module in self.context.program().model().modules() {
            compute_modules::plan_one_module(
                compute_modules::ModulePlanInput {
                    program: self.context.program(),
                    module,
                    external_package_adapters: &analysis.external_package_adapters,
                    externalized_packages: &analysis.externalized_packages,
                    externalized_package_init_bindings: &analysis
                        .externalized_package_init_bindings,
                    source_suppressed_packages: &analysis.source_suppressed_packages,
                    source_module_wiring: &analysis.source_module_wiring,
                    lowered_runtime_sources: &analysis.lowered_runtime_sources,
                    runtime_lazy_folds: &analysis.runtime_lazy_folds,
                    omitted_folded_stub_modules: &self.state.runtime.omitted_folded_stub_modules,
                    pure_reexport_bypasses: &self.state.runtime.pure_reexport_bypasses,
                    runtime_var_migrations: &self.state.runtime.runtime_var_migrations,
                    runtime_prelude_direct_imports: &self
                        .state
                        .runtime
                        .runtime_prelude_direct_imports,
                    runtime_singleton_inlines: &self.state.runtime.runtime_singleton_inlines,
                    runtime_edge_direct_prelude_imports: &self
                        .state
                        .runtime
                        .runtime_edge_direct_prelude_imports,
                    binding_owners: &self.state.runtime.binding_owners,
                },
                compute_modules::ModulePlanAccumulators {
                    plan: &mut self.state.plan,
                    used_runtime_helper_files: &mut self
                        .state
                        .runtime_helpers
                        .used_runtime_helper_files,
                    exported_runtime_helper_bindings: &mut self
                        .state
                        .runtime_helpers
                        .exported_runtime_helper_bindings,
                    required_runtime_helper_bindings: &mut self
                        .state
                        .runtime_helpers
                        .required_runtime_helper_bindings,
                    used_runtime_helper_setters: &mut self
                        .state
                        .runtime_helpers
                        .used_runtime_helper_setters,
                    used_lazy_module: &mut self.state.runtime_helpers.used_lazy_module,
                    used_lazy_value: &mut self.state.runtime_helpers.used_lazy_value,
                    exported_lazy_module: &mut self.state.runtime_helpers.exported_lazy_module,
                    exported_lazy_value: &mut self.state.runtime_helpers.exported_lazy_value,
                    used_package_runtime_helper_files: &mut self
                        .state
                        .package_runtime
                        .used_helper_files,
                },
            )?;
        }
        Ok(())
    }
}
