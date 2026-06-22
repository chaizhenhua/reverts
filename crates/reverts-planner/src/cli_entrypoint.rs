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
    call_identifiers_in_source, runtime_import_identifiers_in_source,
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

/// Per-file line budget for emitted island cluster files. A cluster whose body
/// exceeds this is split into size-bounded sub-cluster files so no generated
/// file is unwieldy to read or load.
const MAX_ISLAND_CLUSTER_LINES: usize = 5000;

/// True for a name that can never be a real cross-module binding emitted as an
/// `import`/`export` specifier: a reserved word (JS keyword or a TS/strict-mode
/// reserved word the flat binding scanner mis-reads off top-level control flow)
/// or the `arguments` pseudo-identifier.
fn is_reserved_or_pseudo_binding(name: &str) -> bool {
    name == "arguments"
        || reverts_js::is_js_keyword(name)
        || matches!(
            name,
            "enum"
                | "await"
                | "implements"
                | "interface"
                | "let"
                | "package"
                | "private"
                | "protected"
                | "public"
                | "static"
                | "yield"
                | "as"
                | "from"
                | "get"
                | "set"
                | "of"
                | "async"
        )
}

/// A recognized inlined CommonJS module unit cleared for relocation into its own
/// file: the memoized init thunk plus the exports object and (optional) guard
/// variable it owns. Moving all three together keeps the writer co-located with
/// the state it writes.
struct RelocatableCjsModule {
    init_fn: BindingName,
    exports: BindingName,
    guard: Option<BindingName>,
}

/// Recognize inlined CJS module units (`var EXPORTS = {}; var GUARD; function
/// INIT() {…}`) that are SAFE to relocate whole. A unit qualifies only when each
/// of its exports/guard variables is declared by a `var` statement that declares
/// nothing else (so relocating it drags no unrelated binding along) and the
/// guard is referenced nowhere outside the init body (so moving the guard with
/// the init leaves no dangling read). Units failing either check are left inline.
fn recognize_relocatable_cjs_modules(island_source: &str) -> Vec<RelocatableCjsModule> {
    use reverts_js::{
        ParseGoal, TopLevelStatementKind, collect_identifier_read_facts,
        collect_top_level_statement_facts,
    };

    let modules = reverts_graph::recognize_cjs_island_modules(island_source);
    if modules.is_empty() {
        return Vec::new();
    }
    let Ok(facts) = collect_top_level_statement_facts(island_source, None, ParseGoal::TypeScript)
    else {
        return Vec::new();
    };
    let Ok(reads) = collect_identifier_read_facts(island_source, None, ParseGoal::TypeScript)
    else {
        return Vec::new();
    };

    // A `var` declaration is a clean owner of `name` when it declares `name` and
    // every binding it declares is in `allowed` (the unit's own exports/guard).
    let var_declares_only = |name: &str, allowed: &BTreeSet<&str>| -> bool {
        facts.iter().any(|fact| {
            matches!(fact.kind, TopLevelStatementKind::Variable)
                && fact.bindings.iter().any(|binding| binding == name)
                && fact
                    .bindings
                    .iter()
                    .all(|binding| allowed.contains(binding.as_str()))
        })
    };

    let mut relocatable = Vec::new();
    for module in modules {
        let mut allowed: BTreeSet<&str> = BTreeSet::new();
        allowed.insert(module.exports.as_str());
        if let Some(guard) = module.guard.as_deref() {
            allowed.insert(guard);
        }

        // Each of the unit's variables must be declared by a `var` statement that
        // declares only the unit's own bindings.
        if !var_declares_only(module.exports.as_str(), &allowed) {
            continue;
        }
        if let Some(guard) = module.guard.as_deref()
            && guard != module.exports.as_str()
            && !var_declares_only(guard, &allowed)
        {
            continue;
        }

        // The guard is private to the memoization: it must be read nowhere
        // outside the init function body, or relocating it would dangle a read.
        if let Some(guard) = module.guard.as_deref()
            && guard != module.exports.as_str()
        {
            let (init_start, init_end) = module.body_span;
            let leaks = reads.iter().any(|read| {
                read.name == guard && !(read.byte_start >= init_start && read.byte_end <= init_end)
            });
            if leaks {
                continue;
            }
        }

        relocatable.push(RelocatableCjsModule {
            init_fn: BindingName::new(module.init_fn.as_str()),
            exports: BindingName::new(module.exports.as_str()),
            guard: module.guard.as_deref().map(BindingName::new),
        });
    }
    relocatable
}

/// The top-level classes that are eval-order-safe to relocate into a cluster
/// file (which loads before the entry). Safe iff none of a class's
/// definition-time references (`extends`, decorators, static initializers,
/// static blocks, computed keys) is an EAGER island binding — a top-level
/// `var`/`const`/`let`/`class`. Hoisted functions and imports are excluded from
/// the eager set: both are available before the entry body runs, so referencing
/// them at class-definition time across the cluster boundary is safe.
/// Top-level island bindings that initialize EAGERLY when the entry body runs —
/// `var`/`const`/`let`/`class` (everything except hoisted functions and imports,
/// which are available before any module body executes). Referencing one of these
/// at a moved cluster's load time (which precedes the entry body) is unsafe.
fn eager_island_bindings(island_source: &str) -> BTreeSet<BindingName> {
    use reverts_js::{ParseGoal, TopLevelStatementKind};
    let Ok(facts) =
        reverts_js::collect_top_level_statement_facts(island_source, None, ParseGoal::TypeScript)
    else {
        return BTreeSet::new();
    };
    facts
        .into_iter()
        .filter(|fact| {
            !matches!(
                fact.kind,
                TopLevelStatementKind::Function | TopLevelStatementKind::Import
            )
        })
        .flat_map(|fact| {
            fact.bindings
                .into_iter()
                .map(|name| BindingName::new(name.as_str()))
        })
        .collect()
}

/// Top-level eager `var`/`const`/`let` bindings that are SELF-CONTAINED constants:
/// a side-effect-free initializer that reads nothing external at evaluation time
/// (only literals and inert function/arrow bodies). Such a value is computed from
/// nothing but itself, so it is identical no matter when it evaluates — relocating
/// it ahead of the entry is provably eval-order-independent. This is stricter than
/// "reads no eager island binding": a const reading global mutable state
/// (`globalThis.foo.add`) is excluded, because an eager STATEMENT in the entry may
/// set that state only later. Pure data literals and function-expression constants
/// dominate this set.
fn eval_order_safe_island_eager_bindings(island_source: &str) -> BTreeSet<BindingName> {
    use reverts_js::ParseGoal;
    let Ok(bindings) =
        reverts_js::collect_top_level_eager_bindings(island_source, None, ParseGoal::TypeScript)
    else {
        return BTreeSet::new();
    };
    bindings
        .into_iter()
        .filter(|binding| binding.pure && binding.eager_references.is_empty())
        .map(|binding| BindingName::new(binding.name.as_str()))
        .collect()
}

fn eval_order_safe_island_classes(island_source: &str) -> BTreeSet<BindingName> {
    use reverts_js::ParseGoal;
    let eager = eager_island_bindings(island_source);
    let Ok(classes) = reverts_js::collect_top_level_class_eager_references(
        island_source,
        None,
        ParseGoal::TypeScript,
    ) else {
        return BTreeSet::new();
    };
    classes
        .into_iter()
        .filter(|class| {
            class
                .references
                .iter()
                .all(|reference| !eager.contains(&BindingName::new(reference.as_str())))
        })
        .map(|class| BindingName::new(class.class_name.as_str()))
        .collect()
}

/// Where a chain-split chunk obtains one of its free variables. The chunk import
/// graph is kept a DAG aligned with source order so esbuild evaluates chunks in
/// the original monolith order (see `route_chunk_need`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChunkNeedRoute {
    /// Declared in this chunk — already in scope, no import.
    Local,
    /// Import directly from the owning (necessarily earlier) chunk.
    Chunk(usize),
    /// Import directly from the owning extracted cluster.
    Cluster(usize),
    /// Import directly from the owning emitted source module (by output path) —
    /// an esbuild lazy-init module binding the entry imports from that module.
    SourceModule(String),
    /// Route through the entry hub as a re-exported live binding.
    Entry,
}

/// Decide how chunk `current_chunk_id` should obtain free variable `need`.
///
/// Chunk ids are assigned sequentially in chain (source) order, so a chunk-owned
/// binding is in an EARLIER chunk iff its owner id is smaller. The rule that keeps
/// the chunk import graph a source-order DAG — and so preserves the monolith's
/// evaluation order under esbuild — is:
/// - own binding → `Local`;
/// - earlier chunk → direct `Chunk` import (only forces the order source already
///   has, so no reordering and no cycle);
/// - later chunk → `Entry`: a direct import would be a back-edge that drags the
///   later chunk's eager statements ahead of this one's (and, with mutual deferred
///   refs, forms an init-order cycle — the `rd`/`lze` crash). The reference is
///   necessarily deferred, so a live binding through the entry hub resolves by
///   call time;
/// - cluster-owned → direct `Cluster` import (clusters load before any chunk and
///   never import a chunk, so this is always DAG-safe);
/// - source-module-owned → direct `SourceModule` import from the owning module
///   (the same edge the entry has; bypasses the entry hub, whose own import would
///   be pruned as unused once the eager body moved out);
/// - otherwise (entry-resident / runtime binding) → `Entry`.
fn route_chunk_need(
    need: &BindingName,
    current_chunk_id: usize,
    chunk_owner: &BTreeMap<BindingName, usize>,
    binding_owner: &BTreeMap<BindingName, usize>,
    source_module_owner: &BTreeMap<BindingName, String>,
) -> ChunkNeedRoute {
    if let Some(&owner) = chunk_owner.get(need) {
        if owner == current_chunk_id {
            ChunkNeedRoute::Local
        } else if owner < current_chunk_id {
            ChunkNeedRoute::Chunk(owner)
        } else {
            ChunkNeedRoute::Entry
        }
    } else if let Some(&owner) = binding_owner.get(need) {
        ChunkNeedRoute::Cluster(owner)
    } else if let Some(module_path) = source_module_owner.get(need) {
        ChunkNeedRoute::SourceModule(module_path.clone())
    } else {
        ChunkNeedRoute::Entry
    }
}

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
    source_module_owner: &BTreeMap<BindingName, String>,
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
    let mut binding_to_cluster: BTreeMap<BindingName, usize> = clusters_by_binding
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

    // Recognize inlined CommonJS module units (the bundler's memoized
    // `var EXPORTS = {}; var GUARD; function INIT() {…}` triples) and relocate
    // each into its own file. The init writes EXPORTS/GUARD, so it is pinned as a
    // shared-state writer above; but moving the init TOGETHER WITH its exports
    // and guard variable declarations co-locates the writer with the state it
    // writes, so nothing is forked. We therefore un-pin each such init and force
    // its (init, exports, guard) triple into a dedicated cluster, letting the
    // normal cluster emission below wire the imports. `force_move_variables`
    // tells the partitioner those exports/guard `var` declarations may move (it
    // otherwise relocates only `function` declarations).
    let mut force_move_variables: BTreeSet<BindingName> = BTreeSet::new();
    let mut next_forced_cluster_id = binding_to_cluster.values().copied().max().unwrap_or(0) + 1;
    for module in recognize_relocatable_cjs_modules(island_source) {
        let cluster_id = next_forced_cluster_id;
        next_forced_cluster_id += 1;
        pinned.remove(&module.init_fn);
        binding_to_cluster.insert(module.init_fn, cluster_id);
        binding_to_cluster.insert(module.exports.clone(), cluster_id);
        force_move_variables.insert(module.exports);
        if let Some(guard) = module.guard {
            binding_to_cluster.insert(guard.clone(), cluster_id);
            force_move_variables.insert(guard);
        }
    }

    // Pin every binding reassigned (`x = …`, `x += …`, `x++`) anywhere in the
    // island: relocating it would make it a read-only import that its reassignment
    // can no longer write. `implicit_global_writes_in_source` only catches writes
    // to undeclared globals, so module-scope mutable counters slip through without
    // this. CJS guard vars are exempt — they are reassigned only inside the init
    // that co-moves with them, so they were already added to `force_move_variables`
    // above and the partitioner moves them regardless of this pin.
    if let Ok(reassigned) = reverts_js::collect_reassigned_binding_names(
        island_source,
        None,
        reverts_js::ParseGoal::TypeScript,
    ) {
        pinned.extend(
            reassigned
                .into_iter()
                .map(|name| BindingName::new(name.as_str()))
                .filter(|binding| !force_move_variables.contains(binding)),
        );
    }

    // Determine which top-level classes are eval-order-safe to relocate. A moved
    // cluster loads BEFORE the entry, so a `class X extends base {}` (and any
    // static initializer/decorator/computed key) that references an EAGER island
    // binding — one that initializes only when the entry body runs — would touch
    // it too early and crash. Globals, entry imports, and hoisted functions are
    // all available when the cluster loads, so a class is safe iff none of its
    // definition-time references is an eager island binding (a top-level `var`/
    // `const`/`let`/`class`, i.e. neither a hoisted function nor an import).
    let movable_classes = eval_order_safe_island_classes(island_source);
    for class in &movable_classes {
        // A safe class that landed in no surviving Louvain cluster still gets its
        // own file; one already clustered moves with its cohort.
        if !binding_to_cluster.contains_key(class) {
            let cluster_id = next_forced_cluster_id;
            next_forced_cluster_id += 1;
            binding_to_cluster.insert(class.clone(), cluster_id);
        }
    }

    // Drain EVERY remaining movable function out of the entry, not just those in
    // community clusters large enough to earn their own file. A hoisted function
    // is always eval-order-safe to relocate; leaving the sub-threshold ones inline
    // is what keeps the entry huge. Functions already in a surviving semantic
    // cluster keep it (cohesive grouping); the rest join one shared overflow
    // cluster that the size cap then splits into budget-bounded files. Pinned
    // functions (entrypoint, reassigned, shared-state writers) are never drained.
    let function_overflow_cluster_id = next_forced_cluster_id;
    next_forced_cluster_id += 1;
    if let Ok(facts) = reverts_js::collect_top_level_statement_facts(
        island_source,
        None,
        reverts_js::ParseGoal::TypeScript,
    ) {
        for fact in facts {
            if !matches!(fact.kind, reverts_js::TopLevelStatementKind::Function) {
                continue;
            }
            for name in fact.bindings {
                let binding = BindingName::new(name.as_str());
                if pinned.contains(&binding) {
                    continue;
                }
                binding_to_cluster
                    .entry(binding)
                    .or_insert(function_overflow_cluster_id);
            }
        }
    }

    // Drain self-contained eager constants (pure data literals, function-expression
    // constants) out of the entry. Their `var`/`const` statements move only because
    // they are listed in `force_move_variables`; reassigned bindings stay pinned (an
    // ES import is read-only). They share one overflow cluster the size cap bounds.
    let eager_overflow_cluster_id = next_forced_cluster_id;
    next_forced_cluster_id += 1;
    for binding in eval_order_safe_island_eager_bindings(island_source) {
        if pinned.contains(&binding) {
            continue;
        }
        force_move_variables.insert(binding.clone());
        binding_to_cluster
            .entry(binding)
            .or_insert(eager_overflow_cluster_id);
    }

    let Some(partition) = crate::island_split::partition_island_into_clusters(
        island_source,
        &binding_to_cluster,
        &pinned,
        &force_move_variables,
        &movable_classes,
    ) else {
        return false;
    };
    if partition.clusters.is_empty() {
        return false;
    }

    // Cap every emitted cluster file at the per-file line budget. A semantic
    // (community-detected) cluster can still be far larger than the budget; since
    // everything it holds is eval-order-independent, splitting it into
    // size-bounded sub-clusters keeps behavior identical while no file exceeds the
    // budget. Sub-cluster cross-references resolve through the same binding-owner
    // wiring below.
    let crate::island_split::IslandPartition {
        remaining_source,
        clusters,
    } = partition;
    let clusters = crate::island_split::split_oversized_clusters(
        clusters,
        MAX_ISLAND_CLUSTER_LINES,
        &mut next_forced_cluster_id,
    );

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
    let binding_owner: BTreeMap<BindingName, usize> = clusters
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
    for group in &clusters {
        let cluster_path = island_cluster_path(group.cluster_id);
        let entry_imports_from_cluster =
            relative_import_specifier(ENTRYPOINT_ISLAND_PATH, cluster_path.as_str());

        // What the cluster references that it does not declare itself: the
        // source's free variables (module-scope unresolved references) by OXC
        // scope resolution. A flat lexical "reads minus locals" scan misreads
        // large relocated module bodies — it can miss a deeply nested local
        // declaration and then route that local through the entry hub as a
        // phantom import (entry re-exports a name it never declares). Scope-aware
        // free-variable analysis excludes every local, however nested.
        let cluster_needs: BTreeSet<BindingName> = reverts_js::free_identifiers_in_source(
            group.cluster_source.as_str(),
            None,
            reverts_js::ParseGoal::TypeScript,
        )
        .unwrap_or_default()
        .into_iter()
        // `arguments` is a function-scope pseudo-identifier, never a real
        // cross-module binding; the flat-scan `entry_available` injects it for
        // every function, so without this guard a relocated body that reads
        // `arguments` would route it through the entry hub as a phantom export.
        .filter(|name| name != "arguments")
        .map(BindingName::new)
        .filter(|name| entry_available.contains(name) || source_module_owner.contains_key(name))
        .collect();

        // Route each need to its source: another cluster, its owning source
        // module (direct import), or the entry hub (for eager/entry-resident
        // bindings only).
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
                    if let Some(module_path) = source_module_owner.get(need) {
                        // An esbuild source-module binding: import it directly from
                        // its owning module, exactly as the entry would.
                        let specifier =
                            relative_import_specifier(cluster_path.as_str(), module_path.as_str());
                        imports.entry(specifier).or_default().insert(need.clone());
                    } else {
                        let specifier = relative_import_specifier(
                            cluster_path.as_str(),
                            ENTRYPOINT_ISLAND_PATH,
                        );
                        imports.entry(specifier).or_default().insert(need.clone());
                        entry_reexports.insert(need.clone());
                    }
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
    // statements. If the residual eager body still exceeds the per-file budget,
    // split it into contiguous, order-preserving chunks emitted as an ORDERED
    // import chain — the entry imports each chunk in source order (named imports
    // both load the chunk's side effects in order and expose its bindings), so
    // evaluation order is unchanged while no file exceeds the budget. A chunk's
    // free variables route to the owning chunk/cluster/entry exactly like a
    // cluster's. Contiguous chunking keeps each reassigned binding's declaration
    // with its writers, so no chunk reassigns a read-only import.
    entry_file.push_source(entry_imports);
    let eager_chunks =
        crate::island_split::chain_split_eager_body(&remaining_source, MAX_ISLAND_CLUSTER_LINES);
    if eager_chunks.len() <= 1 {
        entry_file.push_source(remaining_source);
    } else {
        let chunk_ids: Vec<usize> = eager_chunks
            .iter()
            .map(|_| {
                let id = next_forced_cluster_id;
                next_forced_cluster_id += 1;
                id
            })
            .collect();
        // A chunk's exportable bindings are its TOP-LEVEL declarations only — use
        // the statement-fact bindings (top-level), NOT the flat local scanner which
        // also returns nested function params/locals (and mis-reads control-flow
        // keywords). Exporting a nested local would be "not declared in this file".
        let chunk_locals: Vec<BTreeSet<BindingName>> = eager_chunks
            .iter()
            .map(|chunk| {
                reverts_js::collect_top_level_statement_facts(
                    chunk,
                    None,
                    reverts_js::ParseGoal::TypeScript,
                )
                .unwrap_or_default()
                .into_iter()
                .flat_map(|fact| fact.bindings)
                .filter(|name| !is_reserved_or_pseudo_binding(name))
                .map(|name| BindingName::new(name.as_str()))
                .collect()
            })
            .collect();
        let mut chunk_owner: BTreeMap<BindingName, usize> = BTreeMap::new();
        for (index, locals) in chunk_locals.iter().enumerate() {
            for binding in locals {
                chunk_owner.entry(binding.clone()).or_insert(chunk_ids[index]);
            }
        }
        let mut chain_imports = String::new();
        for (index, chunk) in eager_chunks.iter().enumerate() {
            let chunk_path = island_cluster_path(chunk_ids[index]);
            let locals = &chunk_locals[index];
            let needs: BTreeSet<BindingName> = reverts_js::free_identifiers_in_source(
                chunk,
                None,
                reverts_js::ParseGoal::TypeScript,
            )
            .unwrap_or_default()
            .into_iter()
            .filter(|name| name != "arguments")
            .map(BindingName::new)
            // Wire only ROUTABLE free variables: a binding owned by another chunk,
            // by a cluster, or imported by the entry. A free name that is none of
            // these is a phantom (a pre-existing undefined reference the source
            // already carried — benign as a module-scope undefined, never executed)
            // or a runtime global; importing it would be a "no matching export"
            // error, so it stays an undefined reference exactly as in the unsplit
            // entry.
            .filter(|name| {
                !locals.contains(name)
                    && (chunk_owner.contains_key(name)
                        || binding_owner.contains_key(name)
                        || source_module_owner.contains_key(name)
                        || planned_bindings.contains(name))
            })
            .collect();
            // Route each free variable to its source, keeping the chunk import
            // graph a source-order DAG so esbuild evaluates chunks in monolith
            // order (see `route_chunk_need`).
            let mut imports: BTreeMap<String, BTreeSet<BindingName>> = BTreeMap::new();
            for need in &needs {
                match route_chunk_need(
                    need,
                    chunk_ids[index],
                    &chunk_owner,
                    &binding_owner,
                    source_module_owner,
                ) {
                    ChunkNeedRoute::Local => {}
                    ChunkNeedRoute::Chunk(owner) | ChunkNeedRoute::Cluster(owner) => {
                        let specifier = relative_import_specifier(
                            chunk_path.as_str(),
                            island_cluster_path(owner).as_str(),
                        );
                        imports.entry(specifier).or_default().insert(need.clone());
                    }
                    ChunkNeedRoute::SourceModule(module_path) => {
                        let specifier =
                            relative_import_specifier(chunk_path.as_str(), module_path.as_str());
                        imports.entry(specifier).or_default().insert(need.clone());
                    }
                    ChunkNeedRoute::Entry => {
                        let specifier =
                            relative_import_specifier(chunk_path.as_str(), ENTRYPOINT_ISLAND_PATH);
                        imports.entry(specifier).or_default().insert(need.clone());
                        entry_reexports.insert(need.clone());
                    }
                }
            }
            let chunk_source =
                crate::island_split::assemble_cluster_file(chunk, locals, &imports);
            if std::env::var("REVERTS_DEBUG_CHUNKS").is_ok() {
                let _ = std::fs::create_dir_all("/tmp/chunks");
                let _ = std::fs::write(format!("/tmp/chunks/chunk-{index}.ts"), &chunk_source);
            }
            let mut chunk_file = PlannedFile::new(chunk_path.clone());
            chunk_file.unmodularized_recovered_code = true;
            chunk_file.push_source(chunk_source);
            for binding in locals {
                chunk_file.add_binding(PlannedBinding::new(
                    binding.clone(),
                    binding.clone(),
                    BindingShape::Unknown,
                    true,
                ));
                chunk_file.add_export_with_source_backed(binding.clone(), true);
            }
            crate::finalize_planned_file(&mut chunk_file);
            plan.push_file(chunk_file);

            // The entry imports the chunk IN ORDER: a named import of its bindings
            // both runs its side effects (loading it) and exposes its bindings for
            // the entry's own export block; an empty chunk needs a bare import.
            let from_entry = relative_import_specifier(ENTRYPOINT_ISLAND_PATH, chunk_path.as_str());
            if locals.is_empty() {
                chain_imports.push_str(&format!("import '{from_entry}';\n"));
            } else {
                chain_imports.push_str(
                    crate::island_split::entry_import_for_cluster(locals, from_entry.as_str())
                        .as_str(),
                );
                chain_imports.push('\n');
            }
            moved_all.extend(locals.iter().cloned());
        }
        if std::env::var("REVERTS_DEBUG_CHUNKS").is_ok() {
            let _ = std::fs::create_dir_all("/tmp/chunks");
            let _ = std::fs::write("/tmp/chunks/_chain_imports.ts", &chain_imports);
        }
        entry_file.push_source(chain_imports);
    }
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
        && let Some(externalized) = crate::island_split::externalize_island_packages(
            island_source.as_str(),
            externalizations,
        )
    {
        // Prepend the externalization imports + `const <exports> = ns.default ?? ns`
        // rebindings to the island body (not the entry header): if the body is
        // later split into an ordered import chain, these eager consts must live in
        // the FIRST chunk so they are assigned before any chunk that reads them —
        // entry-body consts run AFTER the (hoisted) chunk imports and would be
        // undefined at chunk load.
        let mut prelude = externalized.imports.join("\n");
        prelude.push('\n');
        island_source = format!("{prelude}{}", externalized.source);
        for binding in &externalized.entry_bindings {
            planned_bindings.insert(binding.clone());
            file.add_binding(PlannedBinding::new(
                binding.clone(),
                binding.clone(),
                BindingShape::Unknown,
                true,
            ));
        }
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
    //
    // The eager body references bindings the entry imports from emitted source
    // modules — both esbuild lazy-init module thunks like `XY` (imported via
    // `push_packed_runtime_helper_imports` from `source_module_imports`) and
    // direct owner imports like `cdA` (`emit_direct_owner_imports` from
    // `direct_imports`). When the body is split, the chunk that inherits such a
    // reference must import the binding too. Routing it through the entry hub
    // fails: the entry's own import of it is unused in the residual entry body
    // and gets pruned, so a re-export would dangle. Instead give the split each
    // binding's OWNING module path so the chunk imports it DIRECTLY — the same
    // edge the entry would have, with no hub indirection.
    let source_module_owner: BTreeMap<BindingName, String> = island
        .source_module_imports
        .iter()
        .chain(island.direct_imports.iter())
        .filter_map(|(module_id, bindings)| {
            module_output_path(program, *module_id).map(|path| (path, bindings))
        })
        .flat_map(|(path, bindings)| {
            bindings
                .iter()
                .map(move |binding| (binding.clone(), path.clone()))
        })
        .collect();
    if !emit_island_clusters(
        prelude,
        plan,
        &mut file,
        island_source.as_str(),
        &planned_bindings,
        &entrypoint.callee,
        &source_module_owner,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn owners(pairs: &[(&str, usize)]) -> BTreeMap<BindingName, usize> {
        pairs
            .iter()
            .map(|(name, id)| (BindingName::new(*name), *id))
            .collect()
    }

    fn module_owners(pairs: &[(&str, &str)]) -> BTreeMap<BindingName, String> {
        pairs
            .iter()
            .map(|(name, path)| (BindingName::new(*name), (*path).to_string()))
            .collect()
    }

    /// The chain-split routing rule that keeps the chunk import graph a
    /// source-order DAG. Chunk ids ascend with chain position, so chunk 20 is the
    /// "current" chunk: earlier chunks have smaller ids, later chunks larger ones.
    #[test]
    fn chunk_needs_route_to_keep_a_source_order_dag() {
        // chunk_owner: `self` lives here (20), `back` earlier (10), `fwd` later (30).
        let chunk_owner = owners(&[("self", 20), ("back", 10), ("fwd", 30)]);
        // binding_owner: an extracted cluster owns `cl`.
        let binding_owner = owners(&[("cl", 7)]);
        // source_module_owner: an esbuild module owns `XY`.
        let source_module_owner = module_owners(&[("XY", "modules/390-telemetry/als.ts")]);
        let route = |name: &str| {
            route_chunk_need(
                &BindingName::new(name),
                20,
                &chunk_owner,
                &binding_owner,
                &source_module_owner,
            )
        };

        // A binding declared in this chunk needs no import.
        assert_eq!(route("self"), ChunkNeedRoute::Local);
        // A backward reference imports directly from the earlier chunk (DAG-safe).
        assert_eq!(route("back"), ChunkNeedRoute::Chunk(10));
        // A FORWARD reference must NOT import the later chunk directly (that is the
        // back-edge that reorders eager init / forms the `rd`/`lze` cycle) — it
        // routes through the entry hub instead.
        assert_eq!(route("fwd"), ChunkNeedRoute::Entry);
        // A cluster-owned binding imports directly from its cluster.
        assert_eq!(route("cl"), ChunkNeedRoute::Cluster(7));
        // A source-module binding imports directly from its owning module — never
        // through the entry hub (whose own import would be pruned as unused).
        assert_eq!(
            route("XY"),
            ChunkNeedRoute::SourceModule("modules/390-telemetry/als.ts".to_string())
        );
        // An entry-resident / runtime binding routes through the entry hub.
        assert_eq!(route("glob"), ChunkNeedRoute::Entry);
    }
}
