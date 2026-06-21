//! Runtime-var migration plan.
//!
//! Each bundled runtime helper file (`source-<N>-helpers.ts`) starts out
//! holding every cross-module mutable binding plus a `__reverts_set_X`
//! function for each — that's how ESM gets around its assignment-to-
//! imported-binding restriction. Many of those bindings have a single
//! writer in application code and aren't read inside the helper body
//! itself; for those, the writer module can declare and export the
//! binding directly, the helper file strips the var + setter, and the
//! consumer route shortens by one hop. `compute_runtime_var_migration_plan`
//! finds those bindings and packages every secondary effect (extra
//! reader snippets, namespace-export getters, alias renames, source/
//! runtime dep adjustments) into a single `RuntimeVarMigrationPlan`.
//!
//! Three related kinds of migration are recorded:
//!
//! - `migrations_by_binding` — primary runtime vars that become owned
//!   by an application module. `RuntimeVarMigration` carries everything
//!   that has to move with the var: paired reader snippets, namespace
//!   exports, runtime helper imports the moved code still needs.
//! - `owned_snippets_by_binding` — source-backed standalone snippets
//!   (a private helper function, a namespace object) whose owner can
//!   be rebuilt without a writable primary var. They use the smaller
//!   `RuntimeOwnedSnippetMigration` shape.
//! - The reverse indexes (`migrations_by_owner`, `owned_snippets_by_owner`)
//!   are populated alongside so per-owner queries are O(1).
//!
//! The impl block's many `*_for_owner` / `*_for_source` accessors are
//! the per-module loop's read API: each one extracts the per-module
//! view of one cluster (extra runtime deps, source-module deps,
//! aliases, namespace export bindings, etc.) from the merged plan.

use std::collections::{BTreeMap, BTreeSet};

use reverts_ir::{BindingName, ModuleId};
use reverts_model::EnrichedProgram;

use crate::runtime_helper_strip::migratable_runtime_var_initializer;
use crate::runtime_source_read::{
    RuntimeBindingReadProfile, runtime_binding_read_profile, runtime_readers_for_binding,
    runtime_source_read_index,
};
use crate::{
    LoweredRuntimeModuleSource, ReaderNonSnippetUseKind, RuntimeLazyFoldPlan,
    RuntimeReaderClusterBlocker, RuntimeReaderClusterContext, RuntimeReaderClusterMigration,
    RuntimeReaderClusterMigrationProposal, SourceModuleWiring,
    add_global_owned_runtime_snippet_migrations, folded_runtime_chunk_definitions,
    localize_reader_runtime_setter_deps, merge_same_owner_overlapping_reader_migrations,
    migratable_folded_non_snippet_runtime_read_result,
    migratable_runtime_primary_with_retained_readers_result,
    migratable_runtime_reader_cluster_result, module_dependency_modules_by_owner,
    module_dependency_path_exists, owner_module_source_lines,
    runtime_owner_definition_modules_by_source, runtime_prelude_direct_import_consumers,
    runtime_reader_folded_non_snippet_use_can_move, runtime_reader_migration_source_lines,
    runtime_reader_owner_available_bindings, runtime_reader_owner_runtime_state,
    select_non_conflicting_reader_migration_proposals,
    sort_reader_migration_proposals_by_preference, unique_source_definition_modules,
};

/// Phase 10: vars currently declared inside `source-<N>-helpers.ts` that
/// can be relocated to their writer module.
///
/// The runtime helper file traditionally holds every cross-module mutable
/// binding plus a `__reverts_set_X(value)` thunk for each, because ESM
/// forbids direct assignment to imported bindings. When a binding's value
/// is only WRITTEN by a single application module AND the runtime body
/// itself never READS that binding, the setter becomes a workaround for a
/// problem that no longer exists — the writer module can declare and
/// export the binding directly.
///
/// `migrations_by_binding` maps each migrated binding to its new owner
/// module. Consumers are routed directly to that owner; the runtime helper
/// file strips the migrated declaration and setter instead of emitting a
/// compatibility re-export barrel.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeVarMigrationPlan {
    /// Binding name → (owner module id, runtime source file id the
    /// binding originally lived in).
    pub(crate) migrations_by_binding: BTreeMap<BindingName, RuntimeVarMigration>,
    /// Reverse index: owner module → set of bindings it now owns.
    /// This index contains primary runtime vars only; extra moved
    /// snippets are derived from `migrations_by_binding`.
    pub(crate) migrations_by_owner: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    /// Source-backed runtime snippets whose owner can be rebuilt without a
    /// writable primary var. These are whole closed helper components (for
    /// example private functions/classes and their namespace export objects)
    /// that no retained runtime snippet, folded chunk, namespace export, or
    /// entrypoint side effect reads.
    pub(crate) owned_snippets_by_binding:
        BTreeMap<(u32, BindingName), RuntimeOwnedSnippetMigration>,
    /// Reverse index for `owned_snippets_by_binding`.
    pub(crate) owned_snippets_by_owner: BTreeMap<ModuleId, BTreeSet<(u32, BindingName)>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeVarMigration {
    pub(crate) owner_module: ModuleId,
    pub(crate) source_file_id: u32,
    /// Additional runtime prelude snippets that must move with the
    /// primary var. The first conservative use is the single reader
    /// function that is the var's only runtime read.
    pub(crate) extra_snippets: BTreeSet<BindingName>,
    /// Runtime namespace export initializers whose namespace object
    /// snippet moved with the primary var. These are emitted in the
    /// writer module as `Object.defineProperties(...)` statements so the
    /// namespace getter no longer reads the migrated var from runtime.
    pub(crate) extra_namespace_exports: BTreeSet<BindingName>,
    /// Runtime helper bindings read by `extra_snippets` after excluding
    /// the migrated primary var. The writer module imports these back
    /// from the helper file.
    pub(crate) extra_runtime_deps: BTreeSet<BindingName>,
    /// Runtime helper bindings written by moved reader snippets but not
    /// moved with the cluster. The moved source is rewritten to call the
    /// helper setter (`__reverts_set_X(value)`) and the owner imports that
    /// setter from runtime.
    pub(crate) extra_runtime_setter_deps: BTreeSet<BindingName>,
    /// Import aliases for `extra_runtime_deps` whose original names collide
    /// with existing bindings in the writer module. Moved snippets are
    /// rewritten to reference the alias.
    pub(crate) extra_runtime_dep_aliases: BTreeMap<BindingName, BindingName>,
    /// Source-module bindings read by `extra_snippets` that are already
    /// represented by a source dependency edge from the writer. The writer
    /// imports these from their source module instead of forcing the reader
    /// cluster to stay in runtime.
    pub(crate) extra_source_deps: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    /// Source-module bindings read by `extra_snippets` where a direct
    /// writer -> source import would create a source-module cycle. The
    /// runtime helper imports and re-exports these bindings, preserving the
    /// existing writer -> runtime -> source route while still allowing the
    /// migrated var/reader/setter cluster to leave runtime.
    pub(crate) extra_runtime_reexport_source_deps: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    /// No-op package initializer shims referenced by moved readers. These
    /// mirror the runtime helper's externalized-package init shims locally so
    /// the reader can move without importing runtime only for a stub call.
    pub(crate) extra_noop_deps: BTreeSet<BindingName>,
    /// Optional initializer expression preserved from the runtime
    /// declaration. `None` means the original `var X;` had no
    /// initializer; `Some(text)` carries a side-effect-free literal
    /// (`null`, `0`, `!1`, `void 0`, etc.) to be emitted alongside the
    /// writer module's `var X = INIT;`.
    pub(crate) initializer: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeOwnedSnippetMigration {
    pub(crate) owner_module: ModuleId,
    pub(crate) source_file_id: u32,
    /// Runtime helper bindings read by this moved standalone snippet/namespace
    /// component but not moved with it. The owner imports these back from the
    /// helper file (or from a different rebuilt owner if that dep also moved).
    pub(crate) extra_runtime_deps: BTreeSet<BindingName>,
    /// Import aliases for `extra_runtime_deps` whose original names collide
    /// with existing bindings in the owner module. Moved snippets are rewritten
    /// to reference the alias.
    pub(crate) extra_runtime_dep_aliases: BTreeMap<BindingName, BindingName>,
    /// No-op runtime helpers read by this moved snippet. Runtime helper
    /// emission erases private no-ops, so owners get local stubs instead of
    /// importing bindings that may disappear.
    pub(crate) extra_noop_deps: BTreeSet<BindingName>,
    /// Whether the snippet is the namespace object side of a recovered
    /// `Object.defineProperties(ns, { ... })` export initializer. When true,
    /// move the namespace initializer statement with the object declaration.
    pub(crate) moves_namespace_export: bool,
}

impl RuntimeVarMigrationPlan {
    pub(crate) fn insert(&mut self, binding: BindingName, migration: RuntimeVarMigration) {
        self.migrations_by_owner
            .entry(migration.owner_module)
            .or_default()
            .insert(binding.clone());
        self.migrations_by_binding.insert(binding, migration);
    }

    pub(crate) fn insert_owned_snippet(
        &mut self,
        binding: BindingName,
        migration: RuntimeOwnedSnippetMigration,
    ) {
        self.owned_snippets_by_owner
            .entry(migration.owner_module)
            .or_default()
            .insert((migration.source_file_id, binding.clone()));
        self.owned_snippets_by_binding
            .insert((migration.source_file_id, binding), migration);
    }

    pub(crate) fn extra_snippets_for_owner(
        &self,
        owner_module: ModuleId,
    ) -> BTreeSet<(u32, BindingName)> {
        let mut snippets = self
            .migrations_by_binding
            .values()
            .filter(|migration| migration.owner_module == owner_module)
            .flat_map(|migration| {
                migration
                    .extra_snippets
                    .iter()
                    .cloned()
                    .map(|binding| (migration.source_file_id, binding))
            })
            .collect::<BTreeSet<_>>();
        snippets.extend(
            self.owned_snippets_by_owner
                .get(&owner_module)
                .into_iter()
                .flatten()
                .cloned(),
        );
        snippets
    }

    pub(crate) fn extra_runtime_deps_for_owner(
        &self,
        owner_module: ModuleId,
    ) -> BTreeSet<BindingName> {
        let mut deps = self
            .migrations_by_binding
            .values()
            .filter(|migration| migration.owner_module == owner_module)
            .flat_map(|migration| {
                migration
                    .extra_runtime_deps
                    .iter()
                    .filter(|dep| !migration.extra_runtime_dep_aliases.contains_key(*dep))
                    .filter(|dep| self.migrated_owner(migration.source_file_id, dep).is_none())
                    .cloned()
            })
            .collect::<BTreeSet<_>>();
        deps.extend(
            self.owned_snippets_by_binding
                .values()
                .filter(|migration| migration.owner_module == owner_module)
                .flat_map(|migration| {
                    migration
                        .extra_runtime_deps
                        .iter()
                        .filter(|dep| !migration.extra_runtime_dep_aliases.contains_key(*dep))
                        .filter(|dep| self.migrated_owner(migration.source_file_id, dep).is_none())
                        .cloned()
                }),
        );
        deps
    }

    pub(crate) fn extra_runtime_deps_by_source_for_owner(
        &self,
        owner_module: ModuleId,
    ) -> BTreeMap<u32, BTreeSet<BindingName>> {
        let mut deps_by_source = BTreeMap::<u32, BTreeSet<BindingName>>::new();
        for migration in self
            .migrations_by_binding
            .values()
            .filter(|migration| migration.owner_module == owner_module)
        {
            let deps = migration
                .extra_runtime_deps
                .iter()
                .filter(|dep| !migration.extra_runtime_dep_aliases.contains_key(*dep))
                .filter(|dep| self.migrated_owner(migration.source_file_id, dep).is_none())
                .cloned()
                .collect::<BTreeSet<_>>();
            if !deps.is_empty() {
                deps_by_source
                    .entry(migration.source_file_id)
                    .or_default()
                    .extend(deps);
            }
        }
        for migration in self
            .owned_snippets_by_binding
            .values()
            .filter(|migration| migration.owner_module == owner_module)
        {
            let deps = migration
                .extra_runtime_deps
                .iter()
                .filter(|dep| !migration.extra_runtime_dep_aliases.contains_key(*dep))
                .filter(|dep| self.migrated_owner(migration.source_file_id, dep).is_none())
                .cloned()
                .collect::<BTreeSet<_>>();
            if !deps.is_empty() {
                deps_by_source
                    .entry(migration.source_file_id)
                    .or_default()
                    .extend(deps);
            }
        }
        deps_by_source
    }

    pub(crate) fn extra_runtime_setter_deps_for_owner(
        &self,
        owner_module: ModuleId,
    ) -> BTreeSet<BindingName> {
        self.migrations_by_binding
            .values()
            .filter(|migration| migration.owner_module == owner_module)
            .flat_map(|migration| migration.extra_runtime_setter_deps.iter().cloned())
            .collect()
    }

    pub(crate) fn extra_runtime_setter_deps_by_source_for_owner(
        &self,
        owner_module: ModuleId,
    ) -> BTreeMap<u32, BTreeSet<BindingName>> {
        let mut deps = BTreeMap::<u32, BTreeSet<BindingName>>::new();
        for migration in self
            .migrations_by_binding
            .values()
            .filter(|migration| migration.owner_module == owner_module)
        {
            if migration.extra_runtime_setter_deps.is_empty() {
                continue;
            }
            deps.entry(migration.source_file_id)
                .or_default()
                .extend(migration.extra_runtime_setter_deps.iter().cloned());
        }
        deps
    }

    pub(crate) fn migrated_extra_runtime_deps_for_owner(
        &self,
        owner_module: ModuleId,
    ) -> BTreeMap<ModuleId, BTreeSet<BindingName>> {
        // A reader cluster may first be selected while one of its cross-writer
        // deps still lives in runtime. A later "primary-only" migration can
        // move that dep to its own writer. Route the already-moved reader's
        // import directly to the new owner instead of keeping a stale
        // writer -> runtime edge.
        let mut deps = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
        for migration in self
            .migrations_by_binding
            .values()
            .filter(|migration| migration.owner_module == owner_module)
        {
            for dep in &migration.extra_runtime_deps {
                if migration.extra_runtime_dep_aliases.contains_key(dep) {
                    continue;
                }
                let Some(dep_owner) = self.migrated_owner(migration.source_file_id, dep) else {
                    continue;
                };
                if dep_owner == owner_module {
                    continue;
                }
                deps.entry(dep_owner).or_default().insert(dep.clone());
            }
        }
        for migration in self
            .owned_snippets_by_binding
            .values()
            .filter(|migration| migration.owner_module == owner_module)
        {
            for dep in &migration.extra_runtime_deps {
                if migration.extra_runtime_dep_aliases.contains_key(dep) {
                    continue;
                }
                let Some(dep_owner) = self.migrated_owner(migration.source_file_id, dep) else {
                    continue;
                };
                if dep_owner == owner_module {
                    continue;
                }
                deps.entry(dep_owner).or_default().insert(dep.clone());
            }
        }
        deps
    }

    pub(crate) fn migrated_aliased_extra_runtime_deps_for_owner(
        &self,
        owner_module: ModuleId,
    ) -> BTreeMap<ModuleId, BTreeMap<BindingName, BindingName>> {
        let mut deps = BTreeMap::<ModuleId, BTreeMap<BindingName, BindingName>>::new();
        for migration in self
            .migrations_by_binding
            .values()
            .filter(|migration| migration.owner_module == owner_module)
        {
            for (dep, alias) in &migration.extra_runtime_dep_aliases {
                let Some(dep_owner) = self.migrated_owner(migration.source_file_id, dep) else {
                    continue;
                };
                if dep_owner == owner_module {
                    continue;
                }
                deps.entry(dep_owner)
                    .or_default()
                    .insert(dep.clone(), alias.clone());
            }
        }
        for migration in self
            .owned_snippets_by_binding
            .values()
            .filter(|migration| migration.owner_module == owner_module)
        {
            for (dep, alias) in &migration.extra_runtime_dep_aliases {
                let Some(dep_owner) = self.migrated_owner(migration.source_file_id, dep) else {
                    continue;
                };
                if dep_owner == owner_module {
                    continue;
                }
                deps.entry(dep_owner)
                    .or_default()
                    .insert(dep.clone(), alias.clone());
            }
        }
        deps
    }

    pub(crate) fn extra_runtime_dep_aliases_for_owner(
        &self,
        owner_module: ModuleId,
    ) -> BTreeMap<u32, BTreeMap<BindingName, BindingName>> {
        let mut aliases = BTreeMap::<u32, BTreeMap<BindingName, BindingName>>::new();
        for migration in self
            .migrations_by_binding
            .values()
            .filter(|migration| migration.owner_module == owner_module)
        {
            if migration.extra_runtime_dep_aliases.is_empty() {
                continue;
            }
            aliases
                .entry(migration.source_file_id)
                .or_default()
                .extend(migration.extra_runtime_dep_aliases.clone());
        }
        for migration in self
            .owned_snippets_by_binding
            .values()
            .filter(|migration| migration.owner_module == owner_module)
        {
            if migration.extra_runtime_dep_aliases.is_empty() {
                continue;
            }
            aliases
                .entry(migration.source_file_id)
                .or_default()
                .extend(migration.extra_runtime_dep_aliases.clone());
        }
        aliases
    }

    pub(crate) fn runtime_extra_runtime_dep_aliases_for_owner(
        &self,
        owner_module: ModuleId,
    ) -> BTreeMap<u32, BTreeMap<BindingName, BindingName>> {
        let mut aliases = BTreeMap::<u32, BTreeMap<BindingName, BindingName>>::new();
        for migration in self
            .migrations_by_binding
            .values()
            .filter(|migration| migration.owner_module == owner_module)
        {
            if migration.extra_runtime_dep_aliases.is_empty() {
                continue;
            }
            let runtime_aliases = migration
                .extra_runtime_dep_aliases
                .iter()
                .filter(|(dep, _alias)| {
                    self.migrated_owner(migration.source_file_id, dep).is_none()
                })
                .map(|(dep, alias)| (dep.clone(), alias.clone()))
                .collect::<BTreeMap<_, _>>();
            if !runtime_aliases.is_empty() {
                aliases
                    .entry(migration.source_file_id)
                    .or_default()
                    .extend(runtime_aliases);
            }
        }
        for migration in self
            .owned_snippets_by_binding
            .values()
            .filter(|migration| migration.owner_module == owner_module)
        {
            if migration.extra_runtime_dep_aliases.is_empty() {
                continue;
            }
            let runtime_aliases = migration
                .extra_runtime_dep_aliases
                .iter()
                .filter(|(dep, _alias)| {
                    self.migrated_owner(migration.source_file_id, dep).is_none()
                })
                .map(|(dep, alias)| (dep.clone(), alias.clone()))
                .collect::<BTreeMap<_, _>>();
            if !runtime_aliases.is_empty() {
                aliases
                    .entry(migration.source_file_id)
                    .or_default()
                    .extend(runtime_aliases);
            }
        }
        aliases
    }

    pub(crate) fn extra_source_deps_for_owner(
        &self,
        owner_module: ModuleId,
    ) -> BTreeMap<ModuleId, BTreeSet<BindingName>> {
        let mut deps = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
        for migration in self
            .migrations_by_binding
            .values()
            .filter(|migration| migration.owner_module == owner_module)
        {
            for (module_id, bindings) in &migration.extra_source_deps {
                deps.entry(*module_id)
                    .or_default()
                    .extend(bindings.iter().cloned());
            }
        }
        deps
    }

    pub(crate) fn extra_runtime_reexport_source_deps_for_owner(
        &self,
        owner_module: ModuleId,
    ) -> BTreeMap<u32, BTreeSet<BindingName>> {
        let mut deps = BTreeMap::<u32, BTreeSet<BindingName>>::new();
        for migration in self
            .migrations_by_binding
            .values()
            .filter(|migration| migration.owner_module == owner_module)
        {
            let bindings = migration
                .extra_runtime_reexport_source_deps
                .values()
                .flatten()
                .cloned()
                .collect::<BTreeSet<_>>();
            if !bindings.is_empty() {
                deps.entry(migration.source_file_id)
                    .or_default()
                    .extend(bindings);
            }
        }
        deps
    }

    pub(crate) fn extra_noop_deps_for_owner(
        &self,
        owner_module: ModuleId,
    ) -> BTreeSet<BindingName> {
        let mut deps = self
            .migrations_by_binding
            .values()
            .filter(|migration| migration.owner_module == owner_module)
            .flat_map(|migration| migration.extra_noop_deps.iter().cloned())
            .collect::<BTreeSet<_>>();
        deps.extend(
            self.owned_snippets_by_binding
                .values()
                .filter(|migration| migration.owner_module == owner_module)
                .flat_map(|migration| migration.extra_noop_deps.iter().cloned()),
        );
        deps
    }

    pub(crate) fn source_dep_exports_for_module(
        &self,
        module_id: ModuleId,
    ) -> BTreeSet<BindingName> {
        self.migrations_by_binding
            .values()
            .flat_map(|migration| {
                migration
                    .extra_source_deps
                    .get(&module_id)
                    .into_iter()
                    .flatten()
                    .cloned()
            })
            .collect()
    }

    pub(crate) fn runtime_reexport_source_deps_for_source(
        &self,
        source_file_id: u32,
    ) -> BTreeMap<ModuleId, BTreeSet<BindingName>> {
        let mut deps = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
        for migration in self
            .migrations_by_binding
            .values()
            .filter(|migration| migration.source_file_id == source_file_id)
        {
            for (module_id, bindings) in &migration.extra_runtime_reexport_source_deps {
                deps.entry(*module_id)
                    .or_default()
                    .extend(bindings.iter().cloned());
            }
        }
        deps
    }

    pub(crate) fn extra_namespace_exports_for_owner(
        &self,
        owner_module: ModuleId,
    ) -> BTreeSet<(u32, BindingName)> {
        let mut exports = self
            .migrations_by_binding
            .values()
            .filter(|migration| migration.owner_module == owner_module)
            .flat_map(|migration| {
                migration
                    .extra_namespace_exports
                    .iter()
                    .cloned()
                    .map(|binding| (migration.source_file_id, binding))
            })
            .collect::<BTreeSet<_>>();
        exports.extend(
            self.owned_snippets_by_binding
                .iter()
                .filter(|(_, migration)| {
                    migration.owner_module == owner_module && migration.moves_namespace_export
                })
                .map(|((source_file_id, binding), _)| (*source_file_id, binding.clone())),
        );
        exports
    }

    pub(crate) fn extra_namespace_export_bindings_for_source(
        &self,
        source_file_id: u32,
    ) -> BTreeSet<BindingName> {
        let mut exports = self
            .migrations_by_binding
            .values()
            .filter(|migration| migration.source_file_id == source_file_id)
            .flat_map(|migration| migration.extra_namespace_exports.iter().cloned())
            .collect::<BTreeSet<_>>();
        exports.extend(
            self.owned_snippets_by_binding
                .iter()
                .filter(|((owned_source_file_id, _), migration)| {
                    *owned_source_file_id == source_file_id && migration.moves_namespace_export
                })
                .map(|((_, binding), _)| binding.clone()),
        );
        exports
    }

    pub(crate) fn extra_snippet_bindings_for_source(
        &self,
        source_file_id: u32,
    ) -> BTreeSet<BindingName> {
        let mut snippets = self
            .migrations_by_binding
            .values()
            .filter(|migration| migration.source_file_id == source_file_id)
            .flat_map(|migration| migration.extra_snippets.iter().cloned())
            .collect::<BTreeSet<_>>();
        snippets.extend(
            self.owned_snippets_by_binding
                .keys()
                .filter(|(snippet_source_file_id, _binding)| {
                    *snippet_source_file_id == source_file_id
                })
                .map(|(_, binding)| binding.clone()),
        );
        snippets
    }

    pub(crate) fn local_bindings_for_owner(&self, owner_module: ModuleId) -> BTreeSet<BindingName> {
        let mut bindings = self
            .migrations_by_owner
            .get(&owner_module)
            .cloned()
            .unwrap_or_default();
        bindings.extend(
            self.extra_snippets_for_owner(owner_module)
                .into_iter()
                .map(|(_, binding)| binding),
        );
        bindings.extend(
            self.extra_namespace_exports_for_owner(owner_module)
                .into_iter()
                .map(|(_, binding)| binding),
        );
        bindings
    }

    pub(crate) fn primary_bindings_for_source(
        &self,
        source_file_id: u32,
    ) -> BTreeMap<BindingName, ModuleId> {
        self.migrations_by_binding
            .iter()
            .filter(|(_, migration)| migration.source_file_id == source_file_id)
            .map(|(binding, migration)| (binding.clone(), migration.owner_module))
            .collect()
    }

    pub(crate) fn migrated_owner(
        &self,
        source_file_id: u32,
        binding: &BindingName,
    ) -> Option<ModuleId> {
        for (primary, migration) in &self.migrations_by_binding {
            if migration.source_file_id != source_file_id {
                continue;
            }
            if primary == binding
                || migration.extra_snippets.contains(binding)
                || migration.extra_namespace_exports.contains(binding)
            {
                return Some(migration.owner_module);
            }
        }
        if let Some(migration) = self
            .owned_snippets_by_binding
            .get(&(source_file_id, binding.clone()))
        {
            return Some(migration.owner_module);
        }
        None
    }
}

pub(crate) fn compute_runtime_var_migration_plan(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
    lowered_runtime_sources: &BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
    runtime_lazy_folds: &RuntimeLazyFoldPlan,
    externalized_packages: &BTreeSet<ModuleId>,
) -> RuntimeVarMigrationPlan {
    // Modules whose lazy initializer bodies have already been folded
    // into the runtime helper file. Their consumer file is an empty
    // re-export stub; there is no source body to host a migrated
    // declaration or to absorb same-module assignments. Skip them.
    let folded_modules: BTreeSet<ModuleId> = runtime_lazy_folds.modules.keys().copied().collect();
    let source_definition_modules_by_source =
        runtime_owner_definition_modules_by_source(program, externalized_packages);
    let all_source_definition_modules = unique_source_definition_modules(program, &BTreeSet::new());
    let module_dependencies_by_owner = module_dependency_modules_by_owner(program);
    let runtime_source_consumers =
        runtime_prelude_direct_import_consumers(program, lowered_runtime_sources);
    // Invert `written_helpers` to find single-writer bindings — but
    // exclude writes that came from a module that was later folded.
    let mut writers: BTreeMap<BindingName, BTreeSet<(ModuleId, u32)>> = BTreeMap::new();
    for (module_id, source) in lowered_runtime_sources {
        if folded_modules.contains(module_id) {
            continue;
        }
        if externalized_packages.contains(module_id) {
            continue;
        }
        for binding in &source.written_helpers {
            writers
                .entry(binding.clone())
                .or_default()
                .insert((*module_id, source.source_file_id));
        }
    }
    let single_writers: BTreeMap<BindingName, (ModuleId, u32)> = writers
        .into_iter()
        .filter_map(|(binding, writers)| {
            if writers.len() == 1 {
                writers.into_iter().next().map(|w| (binding, w))
            } else {
                None
            }
        })
        .collect();
    // Group single-writer candidates by source file id so each runtime
    // prelude is scanned once.
    let mut by_source: BTreeMap<u32, Vec<(BindingName, ModuleId)>> = BTreeMap::new();
    for (binding, (module_id, source_id)) in single_writers {
        by_source
            .entry(source_id)
            .or_default()
            .push((binding, module_id));
    }
    let mut plan = RuntimeVarMigrationPlan::default();
    for (source_id, candidates) in by_source {
        let Some(prelude) = program.model().graph().runtime_prelude(source_id) else {
            continue;
        };
        let candidates = candidates
            .into_iter()
            .filter_map(|(binding, owner_module)| {
                let initializer = migratable_runtime_var_initializer(prelude, &binding)?;
                Some((binding, owner_module, initializer))
            })
            .collect::<Vec<_>>();
        let movable_bindings = candidates
            .iter()
            .map(|(binding, _, _)| binding.clone())
            .collect::<BTreeSet<_>>();
        let candidate_owners = candidates
            .iter()
            .map(|(binding, owner_module, _)| (binding.clone(), *owner_module))
            .collect::<BTreeMap<_, _>>();
        let candidate_initializers = candidates
            .iter()
            .map(|(binding, _, initializer)| (binding.clone(), initializer.clone()))
            .collect::<BTreeMap<_, _>>();
        let candidate_owner_runtime_state = runtime_reader_owner_runtime_state(
            lowered_runtime_sources,
            candidate_owners.values().copied(),
        );
        let folded_chunks = runtime_lazy_folds
            .chunks_by_source_file
            .get(&source_id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let folded_runtime_definitions = folded_runtime_chunk_definitions(folded_chunks);
        let read_index = runtime_source_read_index(prelude, folded_chunks);
        let owner_available_bindings = runtime_reader_owner_available_bindings(
            program,
            source_module_wiring,
            lowered_runtime_sources,
            candidate_owners.values().copied(),
        );
        let source_definition_modules = source_definition_modules_by_source
            .get(&source_id)
            .cloned()
            .unwrap_or_default();
        let owner_source_lines =
            owner_module_source_lines(program, candidate_owners.values().copied());
        let reader_cluster_context = RuntimeReaderClusterContext {
            source_file_id: source_id,
            owner_available_bindings: &owner_available_bindings,
            source_consumers_by_runtime_binding: &runtime_source_consumers,
            source_definition_modules: &source_definition_modules,
            all_source_definition_modules: &all_source_definition_modules,
            externalized_packages,
            module_dependencies_by_owner: &module_dependencies_by_owner,
            folded_modules: &folded_modules,
            folded_runtime_definitions: &folded_runtime_definitions,
            owner_runtime_state: &candidate_owner_runtime_state,
            owner_source_lines: &owner_source_lines,
            prelude,
            read_index: &read_index,
            movable_bindings: &movable_bindings,
            candidate_owners: &candidate_owners,
        };
        let mut migration_proposals = Vec::<RuntimeReaderClusterMigrationProposal>::new();
        for (binding, owner_module, _initializer) in &candidates {
            // The binding must have a setter (zero-writer or shared
            // bindings never enter `written_helpers`, but the prelude may
            // also expose source-backed reads for which there is no setter
            // — skip those).
            // The binding must be a real prelude declaration. The
            // migration accepts bare `var X;` and also `var X = LITERAL;`
            // where LITERAL is a side-effect-free literal that the
            // writer can re-emit verbatim. Anything more complex (calls,
            // identifier references, member access) stays put — moving
            // such an initializer would require dragging its
            // dependencies along.
            // The setter function is synthesized by the planner at emit
            // time — it doesn't appear in the prelude snippets. Any
            // OTHER prelude snippet, namespace export, or folded chunk
            // that references X counts as a runtime read.
            let migration = match runtime_binding_read_profile(&read_index, binding) {
                RuntimeBindingReadProfile::NoReads => RuntimeReaderClusterMigration {
                    primary_bindings: BTreeSet::from([binding.clone()]),
                    extra_snippets: BTreeSet::new(),
                    extra_namespace_exports: BTreeSet::new(),
                    extra_runtime_deps: BTreeSet::new(),
                    extra_runtime_setter_deps: BTreeSet::new(),
                    extra_runtime_dep_aliases: BTreeMap::new(),
                    pinned_runtime_deps: BTreeSet::new(),
                    extra_source_deps: BTreeMap::new(),
                    extra_runtime_reexport_source_deps: BTreeMap::new(),
                    extra_noop_deps: BTreeSet::new(),
                },
                RuntimeBindingReadProfile::SnippetReaders(readers) => {
                    match migratable_runtime_reader_cluster_result(
                        &reader_cluster_context,
                        *owner_module,
                        binding,
                        readers,
                    ) {
                        Ok(migration) => migration,
                        Err(RuntimeReaderClusterBlocker::NonSnippetUse(
                            ReaderNonSnippetUseKind::UnfoldableEntrypointNonSnippetRead,
                        )) => {
                            continue;
                        }
                        Err(_) => {
                            let Some(migration) =
                                migratable_runtime_primary_with_retained_readers_result(
                                    &reader_cluster_context,
                                    *owner_module,
                                    binding,
                                )
                            else {
                                continue;
                            };
                            migration
                        }
                    }
                }
                RuntimeBindingReadProfile::Rejected => {
                    if let Some(migration) = migratable_folded_non_snippet_runtime_read_result(
                        &reader_cluster_context,
                        *owner_module,
                        binding,
                    ) {
                        migration
                    } else if !runtime_reader_folded_non_snippet_use_can_move(
                        &reader_cluster_context,
                        binding,
                    ) {
                        let Some(migration) =
                            migratable_runtime_primary_with_retained_readers_result(
                                &reader_cluster_context,
                                *owner_module,
                                binding,
                            )
                        else {
                            continue;
                        };
                        migration
                    } else {
                        let readers = runtime_readers_for_binding(&read_index, binding);
                        if readers.is_empty() {
                            let Some(migration) =
                                migratable_runtime_primary_with_retained_readers_result(
                                    &reader_cluster_context,
                                    *owner_module,
                                    binding,
                                )
                            else {
                                continue;
                            };
                            migration
                        } else {
                            match migratable_runtime_reader_cluster_result(
                                &reader_cluster_context,
                                *owner_module,
                                binding,
                                readers,
                            ) {
                                Ok(migration) => migration,
                                Err(RuntimeReaderClusterBlocker::NonSnippetUse(
                                    ReaderNonSnippetUseKind::UnfoldableEntrypointNonSnippetRead,
                                )) => {
                                    continue;
                                }
                                Err(_) => {
                                    let Some(migration) =
                                        migratable_runtime_primary_with_retained_readers_result(
                                            &reader_cluster_context,
                                            *owner_module,
                                            binding,
                                        )
                                    else {
                                        continue;
                                    };
                                    migration
                                }
                            }
                        }
                    }
                }
            };
            if !migration
                .primary_bindings
                .iter()
                .all(|primary| candidate_initializers.contains_key(primary))
            {
                continue;
            }
            migration_proposals.push(RuntimeReaderClusterMigrationProposal {
                seed_binding: binding.clone(),
                owner_module: *owner_module,
                source_lines: runtime_reader_migration_source_lines(
                    &reader_cluster_context,
                    &migration,
                ),
                migration,
            });
        }
        let mut migration_proposals = merge_same_owner_overlapping_reader_migrations(
            &reader_cluster_context,
            migration_proposals,
        );
        sort_reader_migration_proposals_by_preference(&mut migration_proposals);
        let mut selected_migration_proposals =
            select_non_conflicting_reader_migration_proposals(&migration_proposals);
        let localized_setter_deps = localize_reader_runtime_setter_deps(
            &reader_cluster_context,
            &mut selected_migration_proposals,
        );

        let mut migrated_primary_bindings = BTreeSet::<BindingName>::new();
        let mut pinned_primary_bindings = BTreeSet::<BindingName>::new();
        let mut migrated_reader_owners = BTreeMap::<BindingName, ModuleId>::new();
        let mut aliased_runtime_deps = BTreeSet::<BindingName>::new();
        let mut aliased_runtime_dep_owners = BTreeMap::<BindingName, BTreeSet<ModuleId>>::new();
        for proposal in &selected_migration_proposals {
            let owner_module = proposal.owner_module;
            let migration = &proposal.migration;
            if migration.primary_bindings.iter().any(|primary| {
                migrated_primary_bindings.contains(primary)
                    || pinned_primary_bindings.contains(primary)
                    || aliased_runtime_deps.contains(primary)
            }) || migration
                .pinned_runtime_deps
                .iter()
                .any(|dep| migrated_primary_bindings.contains(dep))
                || migration
                    .extra_runtime_dep_aliases
                    .keys()
                    .any(|dep| migrated_primary_bindings.contains(dep))
                || migration
                    .extra_snippets
                    .iter()
                    .chain(migration.extra_namespace_exports.iter())
                    .any(|reader| {
                        migrated_reader_owners
                            .get(reader)
                            .is_some_and(|existing_owner| *existing_owner != owner_module)
                    })
            {
                continue;
            }
            for dep in migration.extra_runtime_dep_aliases.keys() {
                aliased_runtime_deps.insert(dep.clone());
                aliased_runtime_dep_owners
                    .entry(dep.clone())
                    .or_default()
                    .insert(owner_module);
            }
            for primary in &migration.primary_bindings {
                let Some(primary_initializer) = candidate_initializers.get(primary).cloned() else {
                    continue;
                };
                plan.insert(
                    primary.clone(),
                    RuntimeVarMigration {
                        owner_module,
                        source_file_id: source_id,
                        extra_snippets: migration.extra_snippets.clone(),
                        extra_namespace_exports: migration.extra_namespace_exports.clone(),
                        extra_runtime_deps: migration.extra_runtime_deps.clone(),
                        extra_runtime_setter_deps: migration.extra_runtime_setter_deps.clone(),
                        extra_runtime_dep_aliases: migration.extra_runtime_dep_aliases.clone(),
                        extra_source_deps: migration.extra_source_deps.clone(),
                        extra_runtime_reexport_source_deps: migration
                            .extra_runtime_reexport_source_deps
                            .clone(),
                        extra_noop_deps: migration.extra_noop_deps.clone(),
                        initializer: primary_initializer,
                    },
                );
                migrated_primary_bindings.insert(primary.clone());
            }
            for reader in migration
                .extra_snippets
                .iter()
                .chain(migration.extra_namespace_exports.iter())
            {
                migrated_reader_owners.insert(reader.clone(), owner_module);
            }
            pinned_primary_bindings.extend(migration.pinned_runtime_deps.iter().cloned());
        }
        for (binding, localized) in localized_setter_deps {
            plan.insert(
                binding.clone(),
                RuntimeVarMigration {
                    owner_module: localized.owner_module,
                    source_file_id: source_id,
                    extra_snippets: BTreeSet::new(),
                    extra_namespace_exports: BTreeSet::new(),
                    extra_runtime_deps: BTreeSet::new(),
                    extra_runtime_setter_deps: BTreeSet::new(),
                    extra_runtime_dep_aliases: BTreeMap::new(),
                    extra_source_deps: BTreeMap::new(),
                    extra_runtime_reexport_source_deps: BTreeMap::new(),
                    extra_noop_deps: BTreeSet::new(),
                    initializer: localized.initializer,
                },
            );
            migrated_primary_bindings.insert(binding);
        }
        // Some clusters share the same reader function across different
        // writer-owned vars:
        //
        //   function pair() { return [left, right]; }
        //
        // The first selected cluster moves `pair` with one writer and records
        // the other var as an extra runtime dep. If the other var's writer can
        // be imported by that reader owner without adding a source cycle, move
        // the second var as a primary-only migration too. The moved reader is
        // then rewired by `migrated_extra_runtime_deps_for_owner` to import the
        // var from its real writer, so both setters disappear.
        for proposal in &migration_proposals {
            let owner_module = proposal.owner_module;
            let migration = &proposal.migration;
            if migration.primary_bindings.iter().any(|primary| {
                migrated_primary_bindings.contains(primary)
                    || pinned_primary_bindings.contains(primary)
            }) || migration
                .pinned_runtime_deps
                .iter()
                .any(|dep| migrated_primary_bindings.contains(dep))
            {
                continue;
            }
            let alias_user_owners = migration
                .primary_bindings
                .iter()
                .filter_map(|primary| aliased_runtime_dep_owners.get(primary))
                .flatten()
                .copied()
                .collect::<BTreeSet<_>>();
            let moved_reader_owners = migration
                .extra_snippets
                .iter()
                .chain(migration.extra_namespace_exports.iter())
                .filter_map(|reader| migrated_reader_owners.get(reader).copied())
                .collect::<BTreeSet<_>>();
            if moved_reader_owners.is_empty()
                || migration
                    .extra_snippets
                    .iter()
                    .chain(migration.extra_namespace_exports.iter())
                    .any(|reader| !migrated_reader_owners.contains_key(reader))
                || moved_reader_owners.iter().any(|reader_owner| {
                    module_dependency_path_exists(
                        &module_dependencies_by_owner,
                        owner_module,
                        *reader_owner,
                    )
                })
                || alias_user_owners.iter().any(|alias_owner| {
                    *alias_owner == owner_module
                        || module_dependency_path_exists(
                            &module_dependencies_by_owner,
                            owner_module,
                            *alias_owner,
                        )
                })
            {
                continue;
            }
            for primary in &migration.primary_bindings {
                let Some(primary_initializer) = candidate_initializers.get(primary).cloned() else {
                    continue;
                };
                plan.insert(
                    primary.clone(),
                    RuntimeVarMigration {
                        owner_module,
                        source_file_id: source_id,
                        extra_snippets: BTreeSet::new(),
                        extra_namespace_exports: BTreeSet::new(),
                        extra_runtime_deps: BTreeSet::new(),
                        extra_runtime_setter_deps: BTreeSet::new(),
                        extra_runtime_dep_aliases: BTreeMap::new(),
                        extra_source_deps: BTreeMap::new(),
                        extra_runtime_reexport_source_deps: BTreeMap::new(),
                        extra_noop_deps: BTreeSet::new(),
                        initializer: primary_initializer,
                    },
                );
                migrated_primary_bindings.insert(primary.clone());
            }
        }
    }
    add_global_owned_runtime_snippet_migrations(
        program,
        source_module_wiring,
        lowered_runtime_sources,
        runtime_lazy_folds,
        externalized_packages,
        &mut plan,
    );
    plan
}
