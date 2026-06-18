//! Runtime externalized binding scan extracted from `lib.rs`.
//!
//! Given a lowered runtime helper source, walks its free identifier set to
//! decide which planner-managed source modules must satisfy each free
//! reference (`source_module_imports`) and which references resolve to a
//! package-init shim because the defining package was externalized
//! (`package_init_shims`). Used by the helper-source emitter to wire the
//! correct import statements before writing the helper to disk.

use std::collections::{BTreeMap, BTreeSet};

use reverts_graph::{RuntimeNamespaceExport, RuntimePrelude};
use reverts_ir::{BindingName, ModuleId};
use reverts_model::EnrichedProgram;

use crate::identifiers::is_planner_synthetic_binding;
use crate::local_bindings::local_bindings_in_source;
use crate::runtime_source_scan::{
    call_identifiers_in_source, runtime_import_identifiers_in_source,
};
use crate::{
    drop_bare_void_zero_top_level_statements, identifiers_in_source,
    rewrite_noop_runtime_helper_calls, unique_source_definition_modules,
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct RuntimeExternalizedBindingScan {
    pub(crate) source_module_imports: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    pub(crate) package_init_shims: BTreeSet<BindingName>,
}

pub(crate) fn scan_runtime_externalized_bindings(
    program: &EnrichedProgram,
    source: &str,
    satisfied_runtime_bindings: &BTreeSet<BindingName>,
    externalized_packages: &BTreeSet<ModuleId>,
) -> RuntimeExternalizedBindingScan {
    let local_bindings = local_bindings_in_source(source);
    let call_identifiers = call_identifiers_in_source(source);

    let definition_modules = unique_source_definition_modules(program, externalized_packages);
    let mut source_module_imports = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
    let mut runtime_import_identifiers = runtime_import_identifiers_in_source(source);
    runtime_import_identifiers.extend(call_identifiers.iter().cloned());
    for identifier in runtime_import_identifiers {
        if local_bindings.contains(identifier.as_str()) {
            continue;
        }
        let binding = BindingName::new(identifier);
        if satisfied_runtime_bindings.contains(&binding) {
            continue;
        }
        let Some(Some(module_id)) = definition_modules.get(&binding) else {
            continue;
        };
        source_module_imports
            .entry(*module_id)
            .or_default()
            .insert(binding);
    }

    let package_init_shims = if externalized_packages.is_empty() {
        BTreeSet::new()
    } else {
        let all_definition_modules = unique_source_definition_modules(program, &BTreeSet::new());
        call_identifiers
            .into_iter()
            .filter(|identifier| !local_bindings.contains(identifier))
            .map(BindingName::new)
            .filter(|binding| {
                all_definition_modules
                    .get(binding)
                    .and_then(|module_id| *module_id)
                    .is_some_and(|module_id| externalized_packages.contains(&module_id))
            })
            .collect()
    };

    RuntimeExternalizedBindingScan {
        source_module_imports,
        package_init_shims,
    }
}

pub(crate) fn erase_rewritable_package_init_shim_calls(
    source: &str,
    package_init_shims: &mut BTreeSet<BindingName>,
) -> String {
    if package_init_shims.is_empty() {
        return source.to_string();
    }
    let rewritten = rewrite_noop_runtime_helper_calls(source, package_init_shims);
    let rewritten = drop_bare_void_zero_top_level_statements(rewritten.as_str());
    let remaining_identifiers = identifiers_in_source(rewritten.as_str());
    package_init_shims.retain(|binding| remaining_identifiers.contains(binding.as_str()));
    rewritten
}

pub(crate) fn retain_runtime_imports_referenced_in_source(
    source: &str,
    imports: &mut BTreeMap<ModuleId, BTreeSet<BindingName>>,
) {
    if imports.is_empty() {
        return;
    }
    let local_bindings = local_bindings_in_source(source);
    let mut referenced = runtime_import_identifiers_in_source(source);
    referenced.extend(call_identifiers_in_source(source));
    imports.retain(|_, bindings| {
        bindings.retain(|binding| {
            referenced.contains(binding.as_str()) && !local_bindings.contains(binding.as_str())
        });
        !bindings.is_empty()
    });
}

pub(crate) fn runtime_module_owner_imports_for_source(
    source: &str,
    satisfied_runtime_bindings: &BTreeSet<BindingName>,
    module_owned_bindings_for_source: &BTreeMap<BindingName, ModuleId>,
    mut imports: BTreeMap<ModuleId, BTreeSet<BindingName>>,
) -> BTreeMap<ModuleId, BTreeSet<BindingName>> {
    let mut identifiers = runtime_import_identifiers_in_source(source);
    identifiers.extend(call_identifiers_in_source(source));
    for identifier in identifiers {
        let binding = BindingName::new(identifier);
        if satisfied_runtime_bindings.contains(&binding) {
            continue;
        }
        let Some(owner_module) = module_owned_bindings_for_source.get(&binding) else {
            continue;
        };
        imports
            .entry(*owner_module)
            .or_default()
            .insert(binding.clone());
    }
    imports
}

pub(crate) fn unresolved_runtime_helper_references(
    prelude: &RuntimePrelude,
    source: &str,
    emitted_runtime_bindings: &BTreeSet<BindingName>,
    imports: &BTreeMap<ModuleId, BTreeSet<BindingName>>,
) -> BTreeSet<BindingName> {
    let imported = imports
        .values()
        .flat_map(|bindings| bindings.iter().cloned())
        .collect::<BTreeSet<_>>();

    runtime_import_identifiers_in_source(source)
        .into_iter()
        .map(BindingName::new)
        .filter(|binding| prelude.defines(binding))
        .filter(|binding| !emitted_runtime_bindings.contains(binding))
        .filter(|binding| !imported.contains(binding))
        .filter(|binding| !is_planner_synthetic_binding(binding.as_str()))
        .collect()
}

pub(crate) fn runtime_namespace_exports_for_helpers(
    prelude: &RuntimePrelude,
    helper_bindings: &BTreeSet<BindingName>,
) -> Vec<RuntimeNamespaceExport> {
    prelude
        .namespace_exports
        .iter()
        .filter(|namespace_export| helper_bindings.contains(&namespace_export.namespace))
        .cloned()
        .collect()
}
