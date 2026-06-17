//! Runtime-helper usage accumulated while module planning runs.

use std::collections::{BTreeMap, BTreeSet};

use reverts_ir::BindingName;

use crate::{RuntimeLazyFoldPlan, detect_folded_lazy_helper_use};

/// Mutable usage discovered while module files are planned. Runtime helper
/// emission consumes this accumulator after all modules have had a chance to
/// request helper files, setters, and lazy wrappers.
#[derive(Default)]
pub(crate) struct RuntimeHelperUsageAccumulator {
    pub(crate) used_runtime_helper_files: BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) exported_runtime_helper_bindings: BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) required_runtime_helper_bindings: BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) used_runtime_helper_setters: BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) used_lazy_module: BTreeSet<u32>,
    pub(crate) used_lazy_value: BTreeSet<u32>,
    pub(crate) exported_lazy_module: BTreeSet<u32>,
    pub(crate) exported_lazy_value: BTreeSet<u32>,
}

impl RuntimeHelperUsageAccumulator {
    pub(crate) fn from_runtime_lazy_folds(runtime_lazy_folds: &RuntimeLazyFoldPlan) -> Self {
        let mut usage = Self::default();
        detect_folded_lazy_helper_use(
            runtime_lazy_folds,
            &mut usage.used_lazy_module,
            &mut usage.used_lazy_value,
        );
        usage
    }

    pub(crate) fn mark_entrypoint(&mut self, source_file_id: u32, callee: &BindingName) {
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
