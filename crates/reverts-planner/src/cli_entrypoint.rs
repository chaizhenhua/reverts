//! Synthesize `cli.ts` when the bundle has a runtime entrypoint.
//!
//! Every bundled application has at most one runtime entrypoint —
//! a top-level call recorded in the runtime prelude (see
//! `RuntimeEntrypoint`). When present, the planner emits a tiny
//! `cli.ts` shim with a `#!/usr/bin/env node` shebang so the file is
//! directly executable. Entrypoint callees that are not already owned by a
//! source module are emitted into a dedicated entrypoint island instead of the
//! shared runtime helper; this keeps application bootstrap code out of the
//! runtime helper while still importing shared runtime state explicitly.

use std::collections::{BTreeMap, BTreeSet};

use reverts_ir::{BindingName, BindingShape, ModuleId};
use reverts_model::EnrichedProgram;

use crate::binding_owner::{BindingOwner, BindingOwnerPlan};
use crate::local_bindings::local_bindings_in_source;
use crate::package_runtime::push_packed_runtime_helper_imports;
use crate::relative_paths::relative_import_specifier;
use crate::runtime_helper_source_closure::{
    close_runtime_helper_source_excluding, runtime_helper_source,
};
use crate::runtime_source_scan::{
    call_identifiers_in_source, runtime_import_identifiers_in_source,
};
use crate::runtime_var_migration::RuntimeVarMigrationPlan;
use crate::statements::{named_export_statement, named_import_statement, runtime_helpers_path};
use crate::{
    EmitPlan, PlannedBinding, PlannedFile, emit_direct_owner_imports, emit_direct_prelude_imports,
    module_output_path, partition_runtime_owner_bindings, runtime_entrypoint,
    runtime_entrypoint_side_effects, scan_runtime_externalized_bindings,
};

pub(crate) const ENTRYPOINT_ISLAND_PATH: &str = "modules/entrypoint.ts";

pub(crate) fn emit_cli_entrypoint(
    program: &EnrichedProgram,
    runtime_var_migrations: &RuntimeVarMigrationPlan,
    binding_owners: &BindingOwnerPlan,
    occupied_runtime_bindings: &BTreeSet<BindingName>,
    externalized_packages: &BTreeSet<ModuleId>,
    plan: &mut EmitPlan,
) {
    let Some((_prelude, entrypoint)) = runtime_entrypoint(program) else {
        return;
    };
    let mut file = PlannedFile::new("cli.ts");
    file.push_source("#!/usr/bin/env node");
    let import_path =
        if let Some(path) = entrypoint_direct_owner_path(program, runtime_var_migrations) {
            path
        } else if entrypoint_island_is_planned(plan)
            || emit_entrypoint_island(
                program,
                binding_owners,
                occupied_runtime_bindings,
                externalized_packages,
                plan,
            )
        {
            ENTRYPOINT_ISLAND_PATH.to_string()
        } else {
            runtime_helpers_path(entrypoint.source_file_id)
        };
    let specifier = relative_import_specifier("cli.ts", import_path.as_str());
    let entrypoint_imports = BTreeSet::from([entrypoint.callee.clone()]);
    file.push_source(named_import_statement(
        entrypoint_imports.iter(),
        specifier.as_str(),
    ));
    file.push_source(format!("await {}();", entrypoint.callee.as_str()));
    crate::finalize_planned_file(&mut file);
    plan.push_file(file);
}

pub(crate) fn entrypoint_direct_owner_path(
    program: &EnrichedProgram,
    runtime_var_migrations: &RuntimeVarMigrationPlan,
) -> Option<String> {
    let (prelude, entrypoint) = runtime_entrypoint(program)?;
    if !runtime_entrypoint_side_effects(prelude, entrypoint).is_empty() {
        return None;
    }
    let owner =
        runtime_var_migrations.migrated_owner(entrypoint.source_file_id, &entrypoint.callee)?;
    module_output_path(program, owner)
}

pub(crate) fn entrypoint_can_import_owner_directly(
    program: &EnrichedProgram,
    runtime_var_migrations: &RuntimeVarMigrationPlan,
    source_file_id: u32,
    callee: &BindingName,
) -> bool {
    let Some((prelude, entrypoint)) = runtime_entrypoint(program) else {
        return false;
    };
    entrypoint.source_file_id == source_file_id
        && entrypoint.callee == *callee
        && runtime_entrypoint_side_effects(prelude, entrypoint).is_empty()
        && runtime_var_migrations
            .migrated_owner(source_file_id, callee)
            .is_some()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EntrypointIslandPlan {
    pub(crate) source_file_id: u32,
    pub(crate) source: String,
    pub(crate) runtime_bindings: BTreeSet<BindingName>,
    direct_imports: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    direct_prelude_imports: BTreeMap<BindingName, crate::RuntimePreludeDirectImport>,
    source_module_imports: BTreeMap<ModuleId, BTreeSet<BindingName>>,
}

pub(crate) fn entrypoint_island_plan(
    program: &EnrichedProgram,
    binding_owners: &BindingOwnerPlan,
    occupied_runtime_bindings: &BTreeSet<BindingName>,
    externalized_packages: &BTreeSet<ModuleId>,
    plan: Option<&EmitPlan>,
) -> Option<EntrypointIslandPlan> {
    let (prelude, entrypoint) = runtime_entrypoint(program)?;
    prelude.snippets.get(&entrypoint.callee)?;
    if !crate::global_owned_moved_snippet_is_cycle_safe(prelude, &entrypoint.callee) {
        return None;
    }

    let mut local_runtime_roots = BTreeSet::from([entrypoint.callee.clone()]);
    let source = loop {
        let helper_closure = close_runtime_helper_source_excluding(
            prelude,
            &local_runtime_roots,
            Some(entrypoint),
            &[],
            occupied_runtime_bindings,
        );
        let source = helper_closure.source;
        let local_bindings = local_bindings_in_source(source.as_str());
        let mut referenced = runtime_import_identifiers_in_source(source.as_str());
        referenced.extend(call_identifiers_in_source(source.as_str()));
        let mut added = false;
        for identifier in referenced {
            if local_bindings.contains(identifier.as_str()) {
                continue;
            }
            let binding = BindingName::new(identifier);
            if !prelude.defines(&binding)
                || occupied_runtime_bindings.contains(&binding)
                || matches!(
                    binding_owners.owner_for(entrypoint.source_file_id, &binding),
                    BindingOwner::Module(_)
                        | BindingOwner::PreludeImport(_)
                        | BindingOwner::PackageRuntime(_)
                )
            {
                continue;
            }
            added |= local_runtime_roots.insert(binding);
        }
        if !added {
            break source;
        }
    };

    let local_bindings = local_bindings_in_source(source.as_str());
    let mut referenced = runtime_import_identifiers_in_source(source.as_str());
    referenced.extend(call_identifiers_in_source(source.as_str()));
    let runtime_deps = referenced
        .iter()
        .filter(|identifier| !local_bindings.contains(identifier.as_str()))
        .map(|identifier| BindingName::new(identifier.as_str()))
        .filter(|binding| prelude.defines(binding))
        .filter(|binding| !local_runtime_roots.contains(binding))
        .collect::<BTreeSet<_>>();
    if runtime_deps.iter().any(|dep| {
        matches!(
            binding_owners.owner_for(entrypoint.source_file_id, dep),
            BindingOwner::PackageRuntime(_)
        )
    }) {
        return None;
    }
    let occupied_refs = referenced
        .iter()
        .filter(|identifier| !local_bindings.contains(identifier.as_str()))
        .map(|identifier| BindingName::new(identifier.as_str()))
        .filter(|binding| !prelude.defines(binding))
        .filter(|binding| occupied_runtime_bindings.contains(binding))
        .collect::<BTreeSet<_>>();
    let mut partition = partition_runtime_owner_bindings(
        binding_owners,
        entrypoint.source_file_id,
        ModuleId(0),
        runtime_deps,
    );
    partition.runtime_bindings.extend(occupied_refs);
    if has_missing_module_import(program, plan, &partition.direct_imports) {
        return minimal_entrypoint_island_plan(
            program,
            binding_owners,
            occupied_runtime_bindings,
            externalized_packages,
            plan,
        );
    }
    let mut satisfied_runtime_bindings = local_runtime_roots.clone();
    satisfied_runtime_bindings.extend(partition.runtime_bindings.iter().cloned());
    satisfied_runtime_bindings.extend(partition.direct_imports.values().flatten().cloned());
    satisfied_runtime_bindings.extend(partition.direct_prelude_imports.keys().cloned());
    let externalized_scan = scan_runtime_externalized_bindings(
        program,
        source.as_str(),
        &satisfied_runtime_bindings,
        externalized_packages,
    );
    let (source_module_imports, missing_source_imports) =
        split_planned_source_module_imports(program, plan, externalized_scan.source_module_imports);
    if !missing_source_imports.is_empty() {
        return minimal_entrypoint_island_plan(
            program,
            binding_owners,
            occupied_runtime_bindings,
            externalized_packages,
            plan,
        );
    }
    Some(EntrypointIslandPlan {
        source_file_id: entrypoint.source_file_id,
        source,
        runtime_bindings: partition.runtime_bindings,
        direct_imports: partition.direct_imports,
        direct_prelude_imports: partition.direct_prelude_imports,
        source_module_imports,
    })
}

pub(crate) fn emit_entrypoint_island(
    program: &EnrichedProgram,
    binding_owners: &BindingOwnerPlan,
    occupied_runtime_bindings: &BTreeSet<BindingName>,
    externalized_packages: &BTreeSet<ModuleId>,
    plan: &mut EmitPlan,
) -> bool {
    let Some(island) = entrypoint_island_plan(
        program,
        binding_owners,
        occupied_runtime_bindings,
        externalized_packages,
        Some(plan),
    ) else {
        return false;
    };
    emit_planned_entrypoint_island(program, plan, island)
}

pub(crate) fn emit_planned_entrypoint_island(
    program: &EnrichedProgram,
    plan: &mut EmitPlan,
    island: EntrypointIslandPlan,
) -> bool {
    if entrypoint_island_is_planned(plan) {
        return true;
    }
    let Some((_prelude, entrypoint)) = runtime_entrypoint(program) else {
        return false;
    };
    let mut file = PlannedFile::new(ENTRYPOINT_ISLAND_PATH);
    let mut planned_bindings = BTreeSet::<BindingName>::new();
    push_packed_runtime_helper_imports(
        program,
        plan,
        &mut file,
        ENTRYPOINT_ISLAND_PATH,
        &island.source_module_imports,
    );
    emit_direct_owner_imports(
        program,
        ModuleId(0),
        ENTRYPOINT_ISLAND_PATH,
        &mut file,
        &mut planned_bindings,
        &island.direct_imports,
    );
    emit_direct_prelude_imports(
        &mut file,
        &mut planned_bindings,
        &island.direct_prelude_imports,
    );
    if !island.runtime_bindings.is_empty() {
        let specifier = relative_import_specifier(
            ENTRYPOINT_ISLAND_PATH,
            runtime_helpers_path(island.source_file_id).as_str(),
        );
        file.push_source(named_import_statement(
            island.runtime_bindings.iter(),
            specifier.as_str(),
        ));
        for binding in &island.runtime_bindings {
            planned_bindings.insert(binding.clone());
            file.add_binding(PlannedBinding::new(
                binding.clone(),
                binding.clone(),
                BindingShape::Unknown,
                true,
            ));
        }
    }
    file.push_source(island.source);
    file.push_source(named_export_statement([&entrypoint.callee].into_iter()));
    file.add_binding(PlannedBinding::new(
        entrypoint.callee.clone(),
        entrypoint.callee.clone(),
        BindingShape::Callable,
        true,
    ));
    file.add_export_with_source_backed(entrypoint.callee.clone(), true);
    crate::finalize_planned_file(&mut file);
    plan.push_file(file);
    true
}

pub(crate) fn entrypoint_island_is_planned(plan: &EmitPlan) -> bool {
    plan.files
        .iter()
        .any(|file| file.path == ENTRYPOINT_ISLAND_PATH)
}

fn minimal_entrypoint_island_plan(
    program: &EnrichedProgram,
    binding_owners: &BindingOwnerPlan,
    occupied_runtime_bindings: &BTreeSet<BindingName>,
    externalized_packages: &BTreeSet<ModuleId>,
    plan: Option<&EmitPlan>,
) -> Option<EntrypointIslandPlan> {
    let (prelude, entrypoint) = runtime_entrypoint(program)?;
    let local_runtime_roots = BTreeSet::from([entrypoint.callee.clone()]);
    let source = runtime_helper_source(prelude, &local_runtime_roots, &[], Some(entrypoint), &[]);
    let local_bindings = local_bindings_in_source(source.as_str());
    let mut referenced = runtime_import_identifiers_in_source(source.as_str());
    referenced.extend(call_identifiers_in_source(source.as_str()));
    let runtime_deps = referenced
        .iter()
        .filter(|identifier| !local_bindings.contains(identifier.as_str()))
        .map(|identifier| BindingName::new(identifier.as_str()))
        .filter(|binding| prelude.defines(binding))
        .filter(|binding| !local_runtime_roots.contains(binding))
        .collect::<BTreeSet<_>>();
    if runtime_deps.iter().any(|dep| {
        matches!(
            binding_owners.owner_for(entrypoint.source_file_id, dep),
            BindingOwner::PackageRuntime(_)
        )
    }) {
        return None;
    }
    let mut partition = partition_runtime_owner_bindings(
        binding_owners,
        entrypoint.source_file_id,
        ModuleId(0),
        runtime_deps,
    );
    partition.runtime_bindings.extend(
        referenced
            .iter()
            .filter(|identifier| !local_bindings.contains(identifier.as_str()))
            .map(|identifier| BindingName::new(identifier.as_str()))
            .filter(|binding| !prelude.defines(binding))
            .filter(|binding| occupied_runtime_bindings.contains(binding)),
    );
    if has_missing_module_import(program, plan, &partition.direct_imports) {
        return None;
    }
    let mut satisfied_runtime_bindings = local_runtime_roots;
    satisfied_runtime_bindings.extend(partition.runtime_bindings.iter().cloned());
    satisfied_runtime_bindings.extend(partition.direct_imports.values().flatten().cloned());
    satisfied_runtime_bindings.extend(partition.direct_prelude_imports.keys().cloned());
    let externalized_scan = scan_runtime_externalized_bindings(
        program,
        source.as_str(),
        &satisfied_runtime_bindings,
        externalized_packages,
    );
    let (source_module_imports, missing_source_imports) =
        split_planned_source_module_imports(program, plan, externalized_scan.source_module_imports);
    if !missing_source_imports.is_empty() {
        return None;
    }
    Some(EntrypointIslandPlan {
        source_file_id: entrypoint.source_file_id,
        source,
        runtime_bindings: partition.runtime_bindings,
        direct_imports: partition.direct_imports,
        direct_prelude_imports: partition.direct_prelude_imports,
        source_module_imports,
    })
}

fn has_missing_module_import(
    program: &EnrichedProgram,
    plan: Option<&EmitPlan>,
    direct_imports: &BTreeMap<ModuleId, BTreeSet<BindingName>>,
) -> bool {
    direct_imports
        .keys()
        .any(|module_id| !module_file_is_planned(program, plan, *module_id))
}

fn split_planned_source_module_imports(
    program: &EnrichedProgram,
    plan: Option<&EmitPlan>,
    imports: BTreeMap<ModuleId, BTreeSet<BindingName>>,
) -> (
    BTreeMap<ModuleId, BTreeSet<BindingName>>,
    BTreeSet<BindingName>,
) {
    let mut planned = BTreeMap::new();
    let mut missing = BTreeSet::new();
    for (module_id, bindings) in imports {
        if module_file_is_planned(program, plan, module_id) {
            planned.insert(module_id, bindings);
        } else {
            missing.extend(bindings);
        }
    }
    (planned, missing)
}

fn module_file_is_planned(
    program: &EnrichedProgram,
    plan: Option<&EmitPlan>,
    module_id: ModuleId,
) -> bool {
    let Some(plan) = plan else {
        return true;
    };
    let Some(path) = module_output_path(program, module_id) else {
        return false;
    };
    plan.files.iter().any(|file| file.path == path)
}
