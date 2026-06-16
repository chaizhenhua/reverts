//! Decides when a "pure barrel re-export" module can be omitted entirely.
//!
//! A barrel module like `export { a, b, c } from './x';` (and nothing
//! else) adds no value once we have the import graph: consumers can
//! import `a`, `b`, `c` directly from `./x`. This module computes which
//! such modules are safe to omit and where to redirect their consumers
//! to.
//!
//! Conservative gating:
//!
//! 1. The module's source must look like a *pure* set of named
//!    re-exports — `pure_named_barrel_reexports` rejects anything with
//!    additional statements or side effects.
//! 2. `consumed_reexports` (the set the rest of the program actually
//!    pulls through the barrel) must equal the full set the barrel
//!    advertises. We don't split a barrel into "redirect these,
//!    preserve those".
//! 3. Every re-exported binding must have exactly one identifiable
//!    owner module reachable through this barrel's dependency edges.
//!    Ambiguous binding origins disqualify the whole module.
//! 4. Externalized package modules are excluded as redirect targets —
//!    consumers must keep their bare-package imports to those.
//!
//! `folded_stub_modules_with_internal_consumers` is the smaller sibling
//! that lives here for thematic grouping: it identifies modules whose
//! body has been folded into a lazy helper yet still need to be kept
//! around because at least one other source module imports from them.

use std::collections::{BTreeMap, BTreeSet};

use reverts_input::ModuleDependencyTarget;
use reverts_ir::{BindingName, ModuleId, ModuleKind};
use reverts_model::EnrichedProgram;

use crate::{RuntimeLazyFoldPlan, SourceModuleWiring, pure_named_barrel_reexports};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PureReexportBypassPlan {
    pub(crate) omitted_modules: BTreeSet<ModuleId>,
    pub(crate) redirects: BTreeMap<ModuleId, BTreeMap<BindingName, ModuleId>>,
}

pub(crate) fn folded_stub_modules_with_internal_consumers(
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

pub(crate) fn pure_reexport_bypass_plan(
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
