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

    pub(crate) fn mark_runtime_bindings(
        &mut self,
        source_file_id: u32,
        bindings: &BTreeSet<BindingName>,
    ) {
        if bindings.is_empty() {
            return;
        }
        self.used_runtime_helper_files
            .entry(source_file_id)
            .or_default()
            .extend(bindings.iter().cloned());
        self.exported_runtime_helper_bindings
            .entry(source_file_id)
            .or_default()
            .extend(bindings.iter().cloned());
        self.required_runtime_helper_bindings
            .entry(source_file_id)
            .or_default()
            .extend(bindings.iter().cloned());
    }

    pub(crate) fn occupied_runtime_bindings(&self, source_file_id: u32) -> BTreeSet<BindingName> {
        let mut bindings = BTreeSet::new();
        if let Some(used) = self.used_runtime_helper_files.get(&source_file_id) {
            bindings.extend(used.iter().cloned());
        }
        if let Some(exported) = self.exported_runtime_helper_bindings.get(&source_file_id) {
            bindings.extend(exported.iter().cloned());
        }
        if let Some(required) = self.required_runtime_helper_bindings.get(&source_file_id) {
            bindings.extend(required.iter().cloned());
        }
        if let Some(setters) = self.used_runtime_helper_setters.get(&source_file_id) {
            bindings.extend(setters.iter().cloned());
        }
        bindings
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
