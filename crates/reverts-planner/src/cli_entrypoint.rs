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
use crate::runtime_helper_writes::rewrite_runtime_helper_writes;
use crate::runtime_source_scan::{
    call_identifiers_in_source, runtime_import_identifiers_in_source, value_identifiers_in_source,
};
use crate::runtime_var_migration::RuntimeVarMigrationPlan;
use crate::statements::{
    named_export_statement, named_import_statement, runtime_helper_setter_name,
    runtime_helpers_path,
};
use crate::top_level_definitions::implicit_global_writes_in_source;
use crate::{
    EmitPlan, PlannedBinding, PlannedFile, emit_direct_owner_imports, emit_direct_prelude_imports,
    erase_rewritable_package_init_shim_calls, module_output_path, partition_runtime_owner_bindings,
    retain_runtime_imports_referenced_in_source, runtime_entrypoint,
    runtime_entrypoint_side_effects, scan_runtime_externalized_bindings,
};

/// Fixed emit path of the entrypoint island: the single file aggregating the
/// eager top-level (non-module) code reachable from the runtime entrypoint. It
/// is synthesized at plan time and is NOT a model module — downstream passes
/// must recognize it via [`PlannedFile::unmodularized_recovered_code`], never
/// by comparing paths.
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

    // Bindings migrated to a concrete owner module must be IMPORTED from that
    // owner, never inlined into the island. The root-selection loop below
    // already refuses to seed them as roots; the snippet closure must exclude
    // them too, otherwise a read dependency of an inlined root drags the
    // owner's `var X;` declaration back into the island as an unassigned local,
    // shadowing the owner's real, assigned copy (the React `__toESM` `b5`
    // phantom: written in `main-entry-init`, read here, but left undefined).
    let module_owned_bindings = binding_owners
        .module_owners_for_source(entrypoint.source_file_id)
        .into_keys()
        .collect::<BTreeSet<_>>();
    let closure_excluded = occupied_runtime_bindings
        .union(&module_owned_bindings)
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut local_runtime_roots = BTreeSet::from([entrypoint.callee.clone()]);
    let mut source = loop {
        let helper_closure = close_runtime_helper_source_excluding(
            prelude,
            &local_runtime_roots,
            Some(entrypoint),
            &[],
            &closure_excluded,
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
    let mut source_module_imports = externalized_scan.source_module_imports;
    let mut package_init_shims = externalized_scan.package_init_shims;
    source = erase_rewritable_package_init_shim_calls(source.as_str(), &mut package_init_shims);
    retain_runtime_imports_referenced_in_source(source.as_str(), &mut source_module_imports);
    let (source_module_imports, missing_source_imports) =
        split_planned_source_module_imports(program, plan, source_module_imports);
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

/// Imported runtime-helper bindings that the inlined island source ASSIGNS to.
/// The island excludes these from inlining (they stay in the helper file and are
/// imported), but a snippet it DID inline — e.g. the per-frame render-state reset
/// `R14` doing `zh1 = sl6, sl6 = []` — writes them. Assigning to an ESM import is
/// illegal (`TypeError: Assignment to constant variable`), so the write must go
/// through the helper's `__reverts_set_X` setter, exactly like normal modules do
/// via `record_lowered_runtime_helper_usage`. The island bypasses that path, so
/// we recover the written set here from the island source's implicit writes.
fn island_written_runtime_setter_bindings(island: &EntrypointIslandPlan) -> BTreeSet<BindingName> {
    implicit_global_writes_in_source(island.source.as_str())
        .into_iter()
        .filter(|binding| island.runtime_bindings.contains(binding))
        .collect()
}

/// Register the island's writes to imported runtime bindings as setter targets on
/// their owner runtime-helper file. MUST run before the helper file is emitted,
/// otherwise the helper never declares/exports `__reverts_set_X` for the state the
/// island mutates and the island's setter calls dangle. Mirrors the branch logic
/// of `emit_cli_entrypoint`: only the island path needs this (the direct-owner
/// path imports the entrypoint from a real module and inlines nothing).
pub(crate) fn register_entrypoint_island_setters(
    program: &EnrichedProgram,
    runtime_var_migrations: &RuntimeVarMigrationPlan,
    binding_owners: &BindingOwnerPlan,
    occupied_runtime_bindings: &BTreeSet<BindingName>,
    externalized_packages: &BTreeSet<ModuleId>,
    plan: &EmitPlan,
    used_runtime_helper_setters: &mut BTreeMap<u32, BTreeSet<BindingName>>,
) {
    if entrypoint_direct_owner_path(program, runtime_var_migrations).is_some() {
        return;
    }
    let Some(island) = entrypoint_island_plan(
        program,
        binding_owners,
        occupied_runtime_bindings,
        externalized_packages,
        Some(plan),
    ) else {
        return;
    };
    let written = island_written_runtime_setter_bindings(&island);
    if written.is_empty() {
        return;
    }
    used_runtime_helper_setters
        .entry(island.source_file_id)
        .or_default()
        .extend(written);
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

/// Emit path of one island cluster file.
fn island_cluster_path(cluster_id: usize) -> String {
    format!("modules/island/cluster-{cluster_id}.ts")
}

/// Smallest cluster (in moved bindings) emitted as its own file. Tiny clusters
/// stay inline in the entry: a separate module per two-function cluster is pure
/// overhead and balloons the import graph for no readability gain.
const MIN_ISLAND_CLUSTER_SIZE: usize = 24;

/// Split the eager island's hoistable declarations into per-cluster files,
/// pushing each cluster file onto `plan` and writing the rewritten entry body
/// (cluster imports + the original eager statements in order + the re-exports
/// clusters import back) onto `entry_file`. Behavior-preserving: only hoisted,
/// side-effect-free function/class declarations move; every eager statement
/// stays in `entry_file` in its original position; cross-file references route
/// through the entry hub so a moved function still sees the same values. Returns
/// `false` (caller emits the unsplit island) when there is nothing worth
/// splitting.
fn emit_island_clusters(
    prelude: &reverts_graph::RuntimePrelude,
    plan: &mut EmitPlan,
    entry_file: &mut PlannedFile,
    island_source: &str,
    planned_bindings: &BTreeSet<BindingName>,
    entrypoint_callee: &BindingName,
) -> bool {
    use std::collections::BTreeMap;

    let clusters_by_binding = crate::island_clustering::cluster_island_prelude(prelude);
    if clusters_by_binding.is_empty() {
        return false;
    }

    // Keep only clusters large enough to be worth a file; bindings in smaller
    // (or singleton) clusters drop out of the map and so stay in the entry.
    let mut cluster_sizes: BTreeMap<usize, usize> = BTreeMap::new();
    for &cluster in clusters_by_binding.values() {
        *cluster_sizes.entry(cluster).or_default() += 1;
    }
    let binding_to_cluster: BTreeMap<BindingName, usize> = clusters_by_binding
        .into_iter()
        .filter(|(_, cluster)| cluster_sizes[cluster] >= MIN_ISLAND_CLUSTER_SIZE)
        .collect();
    if binding_to_cluster.is_empty() {
        return false;
    }

    // Never move the entrypoint itself, any reassigned binding (an ES import is
    // read-only), or any function that mutates shared module state — relocating
    // the latter would fork the state it writes and silently diverge.
    let mut pinned = implicit_global_writes_in_source(island_source);
    pinned.insert(entrypoint_callee.clone());
    if let Ok(writers) = reverts_js::top_level_functions_writing_module_state(
        island_source,
        None,
        reverts_js::ParseGoal::TypeScript,
    ) {
        pinned.extend(
            writers
                .into_iter()
                .map(|name| BindingName::new(name.as_str())),
        );
    }

    let Some(partition) = crate::island_split::partition_island_into_clusters(
        island_source,
        &binding_to_cluster,
        &pinned,
    ) else {
        return false;
    };
    if partition.clusters.is_empty() {
        return false;
    }

    // Names reachable from the entry namespace: every island binding (moved ones
    // are imported back) plus what the entry already imports.
    let entry_available: BTreeSet<BindingName> = local_bindings_in_source(island_source)
        .into_iter()
        .map(BindingName::new)
        .chain(planned_bindings.iter().cloned())
        .collect();

    // Which cluster file owns (defines + exports) each moved binding. Cross-cluster
    // references resolve to a DIRECT import from the owning cluster — normal
    // module-to-module `import`/`export` — instead of routing through the entry
    // hub. Only bindings that stay in the entry (eager statements, pinned
    // shared-state writers, module-owned imports) are imported from the entry.
    let binding_owner: BTreeMap<BindingName, usize> = partition
        .clusters
        .iter()
        .flat_map(|group| {
            group
                .moved_bindings
                .iter()
                .map(move |binding| (binding.clone(), group.cluster_id))
        })
        .collect();

    let mut entry_imports = String::new();
    let mut entry_reexports: BTreeSet<BindingName> = BTreeSet::new();
    let mut moved_all: BTreeSet<BindingName> = BTreeSet::new();
    for group in &partition.clusters {
        let cluster_path = island_cluster_path(group.cluster_id);
        let entry_imports_from_cluster =
            relative_import_specifier(ENTRYPOINT_ISLAND_PATH, cluster_path.as_str());

        // What the cluster references that it does not declare itself.
        let cluster_local: BTreeSet<BindingName> = local_bindings_in_source(&group.cluster_source)
            .into_iter()
            .map(BindingName::new)
            .collect();
        let cluster_needs: BTreeSet<BindingName> =
            value_identifiers_in_source(group.cluster_source.as_str())
                .into_iter()
                .map(BindingName::new)
                .filter(|name| !cluster_local.contains(name) && entry_available.contains(name))
                .collect();

        // Route each need to its source: another cluster (direct import) or the
        // entry (hub, for eager/entry-resident bindings only).
        let mut imports: BTreeMap<String, BTreeSet<BindingName>> = BTreeMap::new();
        for need in &cluster_needs {
            match binding_owner.get(need) {
                Some(&owner_id) if owner_id != group.cluster_id => {
                    let specifier = relative_import_specifier(
                        cluster_path.as_str(),
                        island_cluster_path(owner_id).as_str(),
                    );
                    imports.entry(specifier).or_default().insert(need.clone());
                }
                Some(_) => {} // owned by this cluster (already local) — nothing to import
                None => {
                    let specifier =
                        relative_import_specifier(cluster_path.as_str(), ENTRYPOINT_ISLAND_PATH);
                    imports.entry(specifier).or_default().insert(need.clone());
                    entry_reexports.insert(need.clone());
                }
            }
        }
        let cluster_source = crate::island_split::assemble_cluster_file(
            group.cluster_source.as_str(),
            &group.moved_bindings,
            &imports,
        );

        let mut cluster_file = PlannedFile::new(cluster_path);
        cluster_file.unmodularized_recovered_code = true;
        cluster_file.push_source(cluster_source);
        for binding in &group.moved_bindings {
            cluster_file.add_binding(PlannedBinding::new(
                binding.clone(),
                binding.clone(),
                BindingShape::Unknown,
                true,
            ));
            cluster_file.add_export_with_source_backed(binding.clone(), true);
        }
        crate::finalize_planned_file(&mut cluster_file);
        plan.push_file(cluster_file);

        entry_imports.push_str(
            crate::island_split::entry_import_for_cluster(
                &group.moved_bindings,
                entry_imports_from_cluster.as_str(),
            )
            .as_str(),
        );
        entry_imports.push('\n');
        // `entry_reexports` is populated per-need above (only entry-resident
        // bindings); cross-cluster needs are imported directly and need no
        // re-export from the entry.
        moved_all.extend(group.moved_bindings.iter().cloned());
    }

    // Entry body: import the moved declarations back, then the original eager
    // statements in their original order (preserving side effects).
    entry_file.push_source(entry_imports);
    entry_file.push_source(partition.remaining_source);
    for binding in &moved_all {
        entry_file.add_binding(PlannedBinding::new(
            binding.clone(),
            binding.clone(),
            BindingShape::Unknown,
            true,
        ));
    }
    // Re-export what clusters import from the entry hub (the entrypoint export is
    // emitted separately by the caller).
    let reexports: BTreeSet<BindingName> = entry_reexports
        .into_iter()
        .filter(|binding| binding != entrypoint_callee)
        .collect();
    if !reexports.is_empty() {
        entry_file.push_source(named_export_statement(reexports.iter()));
        for binding in &reexports {
            entry_file.add_export_with_source_backed(binding.clone(), true);
        }
    }
    true
}

pub(crate) fn emit_planned_entrypoint_island(
    program: &EnrichedProgram,
    plan: &mut EmitPlan,
    island: EntrypointIslandPlan,
) -> bool {
    if entrypoint_island_is_planned(plan) {
        return true;
    }
    let Some((prelude, entrypoint)) = runtime_entrypoint(program) else {
        return false;
    };
    let mut file = PlannedFile::new(ENTRYPOINT_ISLAND_PATH);
    // The island aggregates recovered application code that no model module
    // owns; mark it so symbol indexing/naming include its declarations.
    file.unmodularized_recovered_code = true;
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
    // An imported runtime binding the island also WRITES (e.g. `R14` doing
    // `zh1 = sl6`) cannot be assigned directly — ESM imports are read-only. Route
    // those writes through the helper's `__reverts_set_X` setter and import the
    // setter alongside the (still-read) raw binding. The setter is declared and
    // exported by the owner helper because `register_entrypoint_island_setters`
    // recorded the same write set before the helper file was emitted.
    let written_runtime_bindings = island_written_runtime_setter_bindings(&island);
    let mut runtime_imports = island.runtime_bindings.clone();
    runtime_imports.extend(
        written_runtime_bindings
            .iter()
            .map(|binding| BindingName::new(runtime_helper_setter_name(binding))),
    );
    if !runtime_imports.is_empty() {
        let specifier = relative_import_specifier(
            ENTRYPOINT_ISLAND_PATH,
            runtime_helpers_path(island.source_file_id).as_str(),
        );
        file.push_source(named_import_statement(
            runtime_imports.iter(),
            specifier.as_str(),
        ));
        for binding in &runtime_imports {
            planned_bindings.insert(binding.clone());
            file.add_binding(PlannedBinding::new(
                binding.clone(),
                binding.clone(),
                BindingShape::Unknown,
                true,
            ));
        }
    }
    let mut island_source = if written_runtime_bindings.is_empty() {
        island.source
    } else {
        rewrite_runtime_helper_writes(island.source.as_str(), &written_runtime_bindings)
    };
    // Replace inlined third-party packages (recovered by the matcher's island
    // aggregation, attached to the program) with bare imports: delete each
    // package's inlined unit declarations, emit `import * as <exports> from
    // '<pkg>'`, and keep a barrel-init shim. The imported barrel-exports bindings
    // become island-provided imports; the removed members leave the island.
    let externalizations = program.island_package_externalizations();
    if !externalizations.is_empty()
        && let Some(externalized) =
            crate::island_split::externalize_island_packages(island_source.as_str(), externalizations)
    {
        for import in &externalized.imports {
            file.push_source(import.clone());
        }
        for binding in &externalized.entry_bindings {
            planned_bindings.insert(binding.clone());
            file.add_binding(PlannedBinding::new(
                binding.clone(),
                binding.clone(),
                BindingShape::Unknown,
                true,
            ));
        }
        island_source = externalized.source;
    }
    // The entrypoint island carries the main bundle's recovered esbuild node-ESM
    // banner, which uses the CommonJS globals `require`/`__filename`/`__dirname`
    // (undefined in the emitted ES module) — prepend `import.meta.url`-based
    // polyfills for the ones it uses and does not bind itself.
    let island_binds = |name: &str| planned_bindings.contains(&BindingName::new(name));
    if let Some(prelude) = crate::node_cjs_environment_prelude(
        crate::contains_call_to_identifier(island_source.as_str(), "require")
            && !island_binds("require"),
        crate::contains_identifier_reference(island_source.as_str(), "__filename")
            && !island_binds("__filename"),
        crate::contains_identifier_reference(island_source.as_str(), "__dirname")
            && !island_binds("__dirname"),
    ) {
        file.push_source(prelude);
    }
    // Decompose the eager island into per-cluster files where worthwhile;
    // otherwise emit the single aggregate island unchanged.
    if !emit_island_clusters(
        prelude,
        plan,
        &mut file,
        island_source.as_str(),
        &planned_bindings,
        &entrypoint.callee,
    ) {
        file.push_source(island_source);
    }
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
    let mut source =
        runtime_helper_source(prelude, &local_runtime_roots, &[], Some(entrypoint), &[]);
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
    let mut source_module_imports = externalized_scan.source_module_imports;
    let mut package_init_shims = externalized_scan.package_init_shims;
    source = erase_rewritable_package_init_shim_calls(source.as_str(), &mut package_init_shims);
    retain_runtime_imports_referenced_in_source(source.as_str(), &mut source_module_imports);
    let (source_module_imports, missing_source_imports) =
        split_planned_source_module_imports(program, plan, source_module_imports);
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
