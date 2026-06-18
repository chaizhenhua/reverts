//! Canonical owner-table for cross-module bindings.
//!
//! Several subsystems independently decide where a binding "lives":
//! `PackageRuntimeIslandPlan` picks a package-runtime helper file for
//! shared package internals, `runtime_prelude_direct_imports` redirects
//! certain bindings to direct top-level package imports, and
//! `RuntimeVarMigrationPlan` moves writable runtime bindings out into
//! a writer module. The per-module emission loop needs a single
//! authoritative answer; `BindingOwnerPlan::from_parts` merges those
//! decisions with a fixed priority order:
//!
//! 1. Package-runtime island ownership (broadest).
//! 2. Direct prelude imports (more specific).
//! 3. Runtime-var migrations and their extra snippets / namespace
//!    exports (final authority — these explicitly relocated the
//!    binding).
//! 4. Owned-snippet migrations (source-backed reader components).
//!
//! Anything not present in the merged table falls through to
//! `BindingOwner::Runtime`.
//!
//! `RuntimeOwnerImportPartition` is the per-module byproduct of
//! consulting the table: given a set of bindings a module needs, it
//! partitions them into runtime-routed bindings, direct same-module
//! imports, and direct-prelude imports (which can be later forced
//! through runtime for compatibility).

use std::collections::{BTreeMap, BTreeSet};

use reverts_ir::{BindingName, ModuleId};

use crate::package_runtime::{PackageRuntimeIslandPlan, PackageRuntimeOwner};
use crate::{RuntimePreludeDirectImport, RuntimeVarMigrationPlan};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BindingOwner {
    Module(ModuleId),
    Runtime,
    PackageRuntime(PackageRuntimeOwner),
    PreludeImport(RuntimePreludeDirectImport),
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct BindingOwnerPlan {
    pub(crate) owners_by_binding: BTreeMap<(u32, BindingName), BindingOwner>,
}

impl BindingOwnerPlan {
    // Rebuild one canonical owner table for runtime-defined bindings. The
    // insertion order is intentional: package islands are the broadest owner,
    // direct prelude imports are more specific, and runtime var migrations
    // (including their moved reader snippets/namespace exports) are the final
    // authority.
    pub(crate) fn from_parts(
        runtime_var_migrations: &RuntimeVarMigrationPlan,
        runtime_prelude_direct_imports: &BTreeMap<
            u32,
            BTreeMap<BindingName, RuntimePreludeDirectImport>,
        >,
        package_runtime_islands: &PackageRuntimeIslandPlan,
    ) -> Self {
        let mut owners_by_binding = BTreeMap::<(u32, BindingName), BindingOwner>::new();

        for ((source_file_id, binding), owner) in &package_runtime_islands.owners_by_binding {
            owners_by_binding.insert(
                (*source_file_id, binding.clone()),
                BindingOwner::PackageRuntime(owner.clone()),
            );
        }

        for (source_file_id, imports) in runtime_prelude_direct_imports {
            for (binding, import) in imports {
                owners_by_binding.insert(
                    (*source_file_id, binding.clone()),
                    BindingOwner::PreludeImport(import.clone()),
                );
            }
        }

        for (binding, migration) in &runtime_var_migrations.migrations_by_binding {
            owners_by_binding.insert(
                (migration.source_file_id, binding.clone()),
                BindingOwner::Module(migration.owner_module),
            );
            for extra in &migration.extra_snippets {
                owners_by_binding.insert(
                    (migration.source_file_id, extra.clone()),
                    BindingOwner::Module(migration.owner_module),
                );
            }
            for namespace in &migration.extra_namespace_exports {
                owners_by_binding.insert(
                    (migration.source_file_id, namespace.clone()),
                    BindingOwner::Module(migration.owner_module),
                );
            }
        }
        for ((source_file_id, binding), migration) in
            &runtime_var_migrations.owned_snippets_by_binding
        {
            owners_by_binding.insert(
                (*source_file_id, binding.clone()),
                BindingOwner::Module(migration.owner_module),
            );
        }

        Self { owners_by_binding }
    }

    pub(crate) fn owner_for(&self, source_file_id: u32, binding: &BindingName) -> BindingOwner {
        self.owners_by_binding
            .get(&(source_file_id, binding.clone()))
            .cloned()
            .unwrap_or(BindingOwner::Runtime)
    }

    pub(crate) fn module_owner(
        &self,
        source_file_id: u32,
        binding: &BindingName,
    ) -> Option<ModuleId> {
        match self.owner_for(source_file_id, binding) {
            BindingOwner::Module(owner) => Some(owner),
            BindingOwner::Runtime
            | BindingOwner::PackageRuntime(_)
            | BindingOwner::PreludeImport(_) => None,
        }
    }

    pub(crate) fn module_owners_for_source(
        &self,
        source_file_id: u32,
    ) -> BTreeMap<BindingName, ModuleId> {
        self.owners_by_binding
            .iter()
            .filter_map(|((owner_source_file_id, binding), owner)| {
                if *owner_source_file_id != source_file_id {
                    return None;
                }
                match owner {
                    BindingOwner::Module(owner_module) => Some((binding.clone(), *owner_module)),
                    BindingOwner::Runtime
                    | BindingOwner::PackageRuntime(_)
                    | BindingOwner::PreludeImport(_) => None,
                }
            })
            .collect()
    }

    pub(crate) fn package_runtime_owner(
        &self,
        source_file_id: u32,
        binding: &BindingName,
    ) -> Option<&PackageRuntimeOwner> {
        match self
            .owners_by_binding
            .get(&(source_file_id, binding.clone()))
        {
            Some(BindingOwner::PackageRuntime(owner)) => Some(owner),
            Some(
                BindingOwner::Module(_) | BindingOwner::Runtime | BindingOwner::PreludeImport(_),
            )
            | None => None,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeOwnerImportPartition {
    pub(crate) runtime_bindings: BTreeSet<BindingName>,
    pub(crate) direct_imports: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    pub(crate) direct_prelude_imports: BTreeMap<BindingName, RuntimePreludeDirectImport>,
}

impl RuntimeOwnerImportPartition {
    pub(crate) fn route_prelude_imports_through_runtime_except(
        &mut self,
        keep_direct: Option<&BTreeSet<BindingName>>,
    ) {
        let direct_prelude_imports = std::mem::take(&mut self.direct_prelude_imports);
        for (binding, import) in direct_prelude_imports {
            if keep_direct.is_some_and(|bindings| bindings.contains(&binding)) {
                self.direct_prelude_imports.insert(binding, import);
            } else {
                self.runtime_bindings.insert(binding);
            }
        }
    }
}
