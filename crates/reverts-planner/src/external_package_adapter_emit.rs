//! Short-circuit emission for externalized-package adapter modules.
//!
//! When a `Package` module has an accepted `ExternalPackageAdapterPlan`
//! and a matching attribution, the planner emits a tiny adapter file
//! that re-exports the package's surface from the bare-specifier
//! import — and that's the whole story for that module. There's no
//! source body to recover and no further phases of the per-module loop
//! apply. `try_emit_external_package_adapter` packages that check so
//! the per-module loop's `if module.kind == Package && let Some(...) `
//! cascade collapses into a single `try_emit_external_package_adapter(…)?
//! { continue; }` line.

use std::collections::BTreeMap;

use reverts_input::ModuleInput;
use reverts_ir::{ModuleId, ModuleKind};
use reverts_model::EnrichedProgram;
use reverts_package::accepted_external_attribution_for_module;

use crate::{
    EmitPlan, ExternalPackageAdapterPlan, PlannedFile, populate_external_package_adapter_file,
};

#[must_use]
pub(crate) fn try_emit_external_package_adapter(
    program: &EnrichedProgram,
    module: &ModuleInput,
    external_package_adapters: &BTreeMap<ModuleId, ExternalPackageAdapterPlan>,
    file: PlannedFile,
    plan: &mut EmitPlan,
) -> bool {
    if module.kind != ModuleKind::Package {
        return false;
    }
    let Some(adapter_plan) = external_package_adapters.get(&module.id) else {
        return false;
    };
    let Some(attribution) = accepted_external_attribution_for_module(
        &program.model().input().package_attributions,
        module.id,
    ) else {
        return false;
    };
    let mut file = file;
    populate_external_package_adapter_file(
        &mut file,
        program,
        module.id,
        attribution,
        &adapter_plan.bindings,
        adapter_plan.kind,
        adapter_plan.member_proof.as_ref(),
    );
    plan.push_file(file);
    true
}
