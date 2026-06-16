//! Aggregated per-module source facts used by the planner's analysis bus.
//!
//! `SourceModuleFacts::from_program` walks every module once and
//! prepares three indexes that the later analysis passes (adapter
//! resolution, source-module wiring, runtime-var migration) consume
//! repeatedly:
//!
//! 1. `candidate_reads_by_module` — bindings each module reads that
//!    are candidates for being satisfied by a source-module import.
//! 2. `exportable_bindings_by_module` — bindings each module could
//!    plausibly export (its definitions + AST imports it re-exports +
//!    its `export { X } from './…'` named re-exports).
//! 3. `definition_modules_all` — unique definition module for each
//!    binding, when there is exactly one. Ambiguous bindings store
//!    `None`. The "all" suffix is to differentiate from the externalized-
//!    package-aware variant downstream code uses inside the per-module
//!    loop.
//!
//! Computing these once up-front avoids re-walking the module list per
//! subsystem and keeps the planner's analysis phase linear in the
//! number of modules.

use std::collections::{BTreeMap, BTreeSet};

use reverts_ir::{BindingName, ModuleId};
use reverts_model::EnrichedProgram;

use crate::{
    candidate_source_reads_by_module_with_exportable, named_reexported_bindings,
    source_definition_bindings, unique_source_definition_modules_from_bindings,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceModuleFacts {
    pub(crate) candidate_reads_by_module: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    pub(crate) exportable_bindings_by_module: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    pub(crate) definition_modules_all: BTreeMap<BindingName, Option<ModuleId>>,
}

impl SourceModuleFacts {
    pub(crate) fn from_program(program: &EnrichedProgram) -> Self {
        let mut definition_bindings_by_module = BTreeMap::new();
        let mut exportable_bindings_by_module = BTreeMap::new();
        for module in program.model().modules() {
            let definitions = source_definition_bindings(program, module.id);
            let mut exportable = definitions.clone();
            exportable.extend(program.model().graph().ast_imports_for(module.id));
            if let Some(source) = program.model().input().module_source_slice(module.id) {
                exportable.extend(named_reexported_bindings(source.source));
            }
            definition_bindings_by_module.insert(module.id, definitions);
            exportable_bindings_by_module.insert(module.id, exportable);
        }
        let candidate_reads_by_module = candidate_source_reads_by_module_with_exportable(
            program,
            &exportable_bindings_by_module,
        );
        let definition_modules_all = unique_source_definition_modules_from_bindings(
            program,
            &BTreeSet::new(),
            &definition_bindings_by_module,
        );

        Self {
            candidate_reads_by_module,
            exportable_bindings_by_module,
            definition_modules_all,
        }
    }
}
