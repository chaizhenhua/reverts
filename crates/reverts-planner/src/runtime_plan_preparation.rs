//! Immutable runtime/package planning facts prepared before emit passes mutate state.

use std::collections::{BTreeMap, BTreeSet};

use reverts_ir::{BindingName, ModuleId};

use crate::binding_owner::BindingOwnerPlan;
use crate::package_runtime::package_runtime_island_plan;
use crate::planner_context::PlannerContext;
use crate::pure_reexport_bypass::{
    PureReexportBypassPlan, folded_stub_modules_with_internal_consumers, pure_reexport_bypass_plan,
};
use crate::runtime_singleton_inline::{RuntimeSingletonInlinePlan, runtime_singleton_inline_plan};
use crate::runtime_var_migration::{RuntimeVarMigrationPlan, compute_runtime_var_migration_plan};
use crate::{
    RuntimePreludeDirectImport, runtime_edge_direct_prelude_imports, runtime_prelude_direct_imports,
};

/// Planner-wide derived runtime/package decisions prepared once before any
/// mutating pass runs. Keeping these immutable separates expensive analysis
/// from stateful emission bookkeeping.
pub(crate) struct RuntimePlanPreparation {
    pub(crate) omitted_folded_stub_modules: BTreeSet<ModuleId>,
    pub(crate) pure_reexport_bypasses: PureReexportBypassPlan,
    pub(crate) runtime_var_migrations: RuntimeVarMigrationPlan,
    pub(crate) runtime_prelude_direct_imports:
        BTreeMap<u32, BTreeMap<BindingName, RuntimePreludeDirectImport>>,
    pub(crate) runtime_singleton_inlines: RuntimeSingletonInlinePlan,
    pub(crate) runtime_edge_direct_prelude_imports: BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) binding_owners: BindingOwnerPlan,
}

impl RuntimePlanPreparation {
    pub(crate) fn from_context(context: &PlannerContext<'_>) -> Self {
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
