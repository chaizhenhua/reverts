//! Synthesize the per-source-file runtime helpers TS file.
//!
//! After the per-module emission loop finishes, the planner knows which
//! runtime helper bindings each source-file's prelude must still export
//! to its consumers (the bindings that *didn't* migrate, get inlined as
//! singletons, or get absorbed by a package-runtime island). This pass
//! materializes one `modules/runtime/source-<N>-helpers.ts` file per
//! source-file-id that retains a non-empty helper surface.
//!
//! The body it emits is the closed source for the still-needed bindings
//! (computed via `close_runtime_helper_source_excluding`) plus a
//! collection of inline transformations:
//!
//! - private no-op runtime helpers get stripped or rewritten
//! - lazy initializers get purified where possible
//! - identifier setter declarations are emitted
//! - top-level import declarations are coalesced
//! - pure static runtime literals are compacted
//! - same-module setter calls inline to direct assignments
//!
//! All of those passes are sequenced inside `emit_runtime_helper_files`.

use std::collections::{BTreeMap, BTreeSet};

use reverts_ir::{BindingName, BindingShape, ModuleId};
use reverts_model::EnrichedProgram;

use crate::binding_owner::BindingOwnerPlan;
use crate::import_coalesce::coalesce_top_level_import_declarations;
use crate::package_runtime::push_packed_runtime_helper_imports;
use crate::runtime_helper_strip::{
    strip_runtime_namespace_export_sources, strip_runtime_snippet_sources,
    strip_runtime_var_declarations,
};
use crate::runtime_helper_writes::inline_internal_setter_calls;
use crate::runtime_var_migration::RuntimeVarMigrationPlan;
use crate::statements::{
    lazy_module_helper_source, lazy_value_helper_source, named_export_statement,
    noop_function_statement, runtime_helper_setter_declarations, runtime_helper_setter_name,
    runtime_helpers_path,
};
use crate::{
    EmitPlan, ExternalPackageAdapterPlan, PlanError, PlannedBinding, PlannedFile,
    RuntimeLazyFoldPlan, adapter_owned_runtime_bindings, close_runtime_helper_source_excluding,
    coalesce_runtime_lazy_initializer_call_runs, compact_pure_static_runtime_literals,
    drop_bare_void_zero_top_level_statements, identifiers_in_source,
    inline_single_use_runtime_proxy_functions, noop_runtime_helpers_in_source,
    private_noop_runtime_helpers_in_source, prune_orphan_runtime_bindings,
    purify_private_runtime_lazy_initializers, rewrite_noop_runtime_helper_calls,
    runtime_entrypoint_root_bindings, runtime_module_owner_imports_for_source,
    scan_runtime_externalized_bindings, strip_runtime_noop_declarations,
    unresolved_runtime_helper_references,
};

pub(crate) struct RuntimeHelperEmissionContext<'a> {
    pub(crate) program: &'a EnrichedProgram,
    pub(crate) runtime_var_migrations: &'a RuntimeVarMigrationPlan,
    pub(crate) binding_owners: &'a BindingOwnerPlan,
    pub(crate) runtime_lazy_folds: &'a RuntimeLazyFoldPlan,
    pub(crate) externalized_packages: &'a BTreeSet<ModuleId>,
    pub(crate) external_package_adapters: &'a BTreeMap<ModuleId, ExternalPackageAdapterPlan>,
    pub(crate) used_runtime_helper_files: &'a BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) exported_runtime_helper_bindings: &'a BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) required_runtime_helper_bindings: &'a BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) used_runtime_helper_setters: &'a BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) used_lazy_module: &'a BTreeSet<u32>,
    pub(crate) used_lazy_value: &'a BTreeSet<u32>,
    pub(crate) exported_lazy_module: &'a BTreeSet<u32>,
    pub(crate) exported_lazy_value: &'a BTreeSet<u32>,
}

pub(crate) fn emit_runtime_helper_files(
    ctx: &RuntimeHelperEmissionContext<'_>,
    plan: &mut EmitPlan,
) -> Result<(), PlanError> {
    let program = ctx.program;
    let runtime_var_migrations = ctx.runtime_var_migrations;
    let binding_owners = ctx.binding_owners;
    let runtime_lazy_folds = ctx.runtime_lazy_folds;
    let externalized_packages = ctx.externalized_packages;
    let external_package_adapters = ctx.external_package_adapters;
    let used_runtime_helper_files = ctx.used_runtime_helper_files;
    let exported_runtime_helper_bindings = ctx.exported_runtime_helper_bindings;
    let required_runtime_helper_bindings = ctx.required_runtime_helper_bindings;
    let used_runtime_helper_setters = ctx.used_runtime_helper_setters;
    let used_lazy_module = ctx.used_lazy_module;
    let used_lazy_value = ctx.used_lazy_value;
    let exported_lazy_module = ctx.exported_lazy_module;
    let exported_lazy_value = ctx.exported_lazy_value;

    for (source_file_id, helper_bindings) in used_runtime_helper_files {
        let Some(prelude) = program.model().graph().runtime_prelude(*source_file_id) else {
            continue;
        };
        let mut file = PlannedFile::new(runtime_helpers_path(*source_file_id));
        let mut public_helper_bindings = exported_runtime_helper_bindings
            .get(source_file_id)
            .cloned()
            .unwrap_or_default();
        let extra_snippet_bindings_for_source =
            runtime_var_migrations.extra_snippet_bindings_for_source(*source_file_id);
        let extra_namespace_export_bindings_for_source =
            runtime_var_migrations.extra_namespace_export_bindings_for_source(*source_file_id);
        let migrations_for_source =
            runtime_var_migrations.primary_bindings_for_source(*source_file_id);
        let module_owned_bindings_for_source =
            binding_owners.module_owners_for_source(*source_file_id);
        let module_owned_binding_names_for_source = module_owned_bindings_for_source
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let entrypoint = prelude
            .entrypoint
            .as_ref()
            .filter(|entrypoint| helper_bindings.contains(&entrypoint.callee));
        let entrypoint_callee = entrypoint.map(|entrypoint| entrypoint.callee.clone());
        let consumed_helper_bindings =
            planned_runtime_helper_consumed_bindings(plan, *source_file_id);
        let namespace_export_helpers = prelude
            .namespace_exports
            .iter()
            .map(|export| export.helper.clone())
            .collect::<BTreeSet<_>>();
        public_helper_bindings.retain(|binding| {
            !namespace_export_helpers.contains(binding)
                || consumed_helper_bindings.contains(binding)
                || entrypoint_callee
                    .as_ref()
                    .is_some_and(|callee| callee == binding)
        });
        let mut root_bindings = required_runtime_helper_bindings
            .get(source_file_id)
            .cloned()
            .unwrap_or_else(|| helper_bindings.clone());
        root_bindings.retain(|binding| {
            !namespace_export_helpers.contains(binding)
                || consumed_helper_bindings.contains(binding)
                || entrypoint_callee
                    .as_ref()
                    .is_some_and(|callee| callee == binding)
        });
        if let Some(setter_targets) = used_runtime_helper_setters.get(source_file_id) {
            root_bindings.extend(setter_targets.iter().cloned());
        }
        for binding in &module_owned_binding_names_for_source {
            root_bindings.remove(binding);
        }
        if let Some(entrypoint) = entrypoint {
            root_bindings.extend(runtime_entrypoint_root_bindings(prelude, entrypoint));
        }
        if let Some(folded_chunks) = runtime_lazy_folds.chunks_by_source_file.get(source_file_id) {
            for chunk in folded_chunks {
                for identifier in identifiers_in_source(chunk.source.as_str()) {
                    let binding = BindingName::new(identifier);
                    if prelude.defines(&binding) {
                        root_bindings.insert(binding);
                    }
                }
            }
        }
        for binding in &module_owned_binding_names_for_source {
            root_bindings.remove(binding);
        }
        let folded_chunks = runtime_lazy_folds
            .chunks_by_source_file
            .get(source_file_id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut helper_closure = close_runtime_helper_source_excluding(
            prelude,
            &root_bindings,
            entrypoint,
            folded_chunks,
            &module_owned_binding_names_for_source,
        );
        // Phase 9a: collapse `__reverts_set_X(arg)` → `(X = arg)`
        // whenever the call sits inside the runtime helpers module
        // itself. Same-module direct assignment is observationally
        // equivalent for single-argument invocations, and the form
        // reads as the underlying bundler intent. The cross-module
        // setter function is still emitted below so consumer modules
        // (which can't legally assign through their read-only ESM
        // imports) keep their existing call form unchanged.
        helper_closure.source = inline_internal_setter_calls(&helper_closure.source);
        // Phase 10b: bindings the migration plan reassigned out of
        // this runtime helpers file no longer have their `var X;`
        // declaration here. Consumers are routed directly to the owner
        // module instead of through a runtime re-export barrel.
        if !migrations_for_source.is_empty() {
            helper_closure.source = strip_runtime_var_declarations(
                helper_closure.source.as_str(),
                migrations_for_source.keys(),
            );
            for binding in migrations_for_source.keys() {
                helper_closure.emitted_bindings.remove(binding);
            }
        }
        if !extra_snippet_bindings_for_source.is_empty() {
            helper_closure.source = strip_runtime_snippet_sources(
                helper_closure.source.as_str(),
                prelude,
                &extra_snippet_bindings_for_source,
            );
            for binding in &extra_snippet_bindings_for_source {
                helper_closure.emitted_bindings.remove(binding);
            }
        }
        if !extra_namespace_export_bindings_for_source.is_empty() {
            helper_closure.source = strip_runtime_namespace_export_sources(
                helper_closure.source.as_str(),
                prelude,
                &extra_namespace_export_bindings_for_source,
            );
        }
        let adapter_owned_runtime_bindings = adapter_owned_runtime_bindings(
            program,
            external_package_adapters,
            externalized_packages,
            &helper_closure.emitted_bindings,
        );
        if !adapter_owned_runtime_bindings.is_empty() {
            helper_closure.source = strip_runtime_var_declarations(
                helper_closure.source.as_str(),
                &adapter_owned_runtime_bindings,
            );
            for binding in &adapter_owned_runtime_bindings {
                helper_closure.emitted_bindings.remove(binding);
            }
        }
        // Phase 11a: some runtime lazyValue thunks are not real lazy
        // module boundaries; they only fill private helper vars with
        // pure values and return `undefined`. Make those assignments
        // direct and leave a no-op initializer behind for existing
        // `init()` call sites. This removes lazy loader wrappers
        // without changing the exported runtime surface.
        helper_closure.source = purify_private_runtime_lazy_initializers(
            helper_closure.source.as_str(),
            &helper_closure.emitted_bindings,
        );
        let noop_runtime_helpers = noop_runtime_helpers_in_source(helper_closure.source.as_str());
        let erased_private_noops = private_noop_runtime_helpers_in_source(
            helper_closure.source.as_str(),
            &public_helper_bindings,
        );
        if !noop_runtime_helpers.is_empty() {
            helper_closure.source = rewrite_noop_runtime_helper_calls(
                helper_closure.source.as_str(),
                &noop_runtime_helpers,
            );
            helper_closure.source =
                drop_bare_void_zero_top_level_statements(helper_closure.source.as_str());
        }
        if !erased_private_noops.is_empty() {
            helper_closure.source = strip_runtime_noop_declarations(
                helper_closure.source.as_str(),
                &erased_private_noops,
            );
            for binding in &erased_private_noops {
                helper_closure.emitted_bindings.remove(binding);
            }
        }
        helper_closure.source =
            coalesce_runtime_lazy_initializer_call_runs(helper_closure.source.as_str());
        helper_closure.source =
            compact_pure_static_runtime_literals(helper_closure.source.as_str());
        helper_closure.source = inline_single_use_runtime_proxy_functions(
            helper_closure.source.as_str(),
            &public_helper_bindings,
        );
        helper_closure.source =
            coalesce_top_level_import_declarations(helper_closure.source.as_str());
        let runtime_setter_bindings = used_runtime_helper_setters
            .get(source_file_id)
            .cloned()
            .unwrap_or_default();
        // Phase 14a: after all runtime-local rewrites, rebuild a binding
        // graph from the emitted helper source and delete private helper
        // declarations that are no longer reachable from the public
        // runtime surface, setter targets, or top-level side effects. This
        // is deliberately structural (no symbol-name allowlist): only
        // top-level declarations whose initializer/declaration is
        // side-effect-free are pruned.
        let mut runtime_binding_roots = public_helper_bindings.clone();
        runtime_binding_roots.extend(runtime_setter_bindings.iter().cloned());
        let orphan_prune =
            prune_orphan_runtime_bindings(helper_closure.source.as_str(), &runtime_binding_roots);
        helper_closure.source = orphan_prune.source;
        for binding in &orphan_prune.dropped_bindings {
            helper_closure.emitted_bindings.remove(binding);
        }
        let runtime_externalized_binding_scan = scan_runtime_externalized_bindings(
            program,
            helper_closure.source.as_str(),
            &helper_closure.emitted_bindings,
            externalized_packages,
        );
        let mut helper_imports = runtime_module_owner_imports_for_source(
            helper_closure.source.as_str(),
            &helper_closure.emitted_bindings,
            &module_owned_bindings_for_source,
            runtime_externalized_binding_scan.source_module_imports,
        );
        for (module_id, bindings) in
            runtime_var_migrations.runtime_reexport_source_deps_for_source(*source_file_id)
        {
            helper_imports
                .entry(module_id)
                .or_default()
                .extend(bindings);
        }
        let package_init_shims = runtime_externalized_binding_scan.package_init_shims;
        let mut emitted_runtime_bindings = helper_closure.emitted_bindings.clone();
        emitted_runtime_bindings.extend(package_init_shims.iter().cloned());
        let helper_path = runtime_helpers_path(*source_file_id);
        let unresolved = unresolved_runtime_helper_references(
            prelude,
            helper_closure.source.as_str(),
            &emitted_runtime_bindings,
            &helper_imports,
        );
        if !unresolved.is_empty() {
            return Err(PlanError::UnresolvedRuntimeHelperReferences {
                path: helper_path,
                bindings: unresolved.into_iter().collect(),
            });
        }
        push_packed_runtime_helper_imports(
            program,
            plan,
            &mut file,
            helper_path.as_str(),
            &helper_imports,
        );
        for binding in &package_init_shims {
            file.push_source(noop_function_statement(binding));
        }
        if !helper_closure.source.trim().is_empty() {
            file.push_source(helper_closure.source);
        }
        // Phase 10b: skip setter functions for migrated primary bindings;
        // the writer module now mutates them via direct assignment.
        let setter_bindings: BTreeSet<BindingName> = runtime_setter_bindings
            .difference(
                &migrations_for_source
                    .keys()
                    .cloned()
                    .collect::<BTreeSet<_>>(),
            )
            .cloned()
            .collect();
        if !setter_bindings.is_empty() {
            file.push_source(runtime_helper_setter_declarations(&setter_bindings));
        }
        let emits_lazy_module = used_lazy_module.contains(source_file_id);
        let emits_lazy_value = used_lazy_value.contains(source_file_id);
        let exports_lazy_module = exported_lazy_module.contains(source_file_id);
        let exports_lazy_value = exported_lazy_value.contains(source_file_id);
        if emits_lazy_module {
            file.push_source(lazy_module_helper_source());
        }
        if emits_lazy_value {
            file.push_source(lazy_value_helper_source());
        }
        let mut exported_bindings = public_helper_bindings.clone();
        exported_bindings.extend(
            setter_bindings
                .iter()
                .map(|binding| BindingName::new(runtime_helper_setter_name(binding))),
        );
        // Phase 10b: drop module-owned bindings from the runtime helper's
        // own named export. All consumers should import the owner module
        // directly; the runtime file is no longer a compatibility barrel.
        for binding in &module_owned_binding_names_for_source {
            exported_bindings.remove(binding);
        }
        if exports_lazy_module {
            exported_bindings.insert(BindingName::new("lazyModule"));
        }
        if exports_lazy_value {
            exported_bindings.insert(BindingName::new("lazyValue"));
        }
        if !exported_bindings.is_empty() {
            file.push_source(named_export_statement(exported_bindings.iter()));
        }
        for binding in public_helper_bindings
            .iter()
            .filter(|binding| !module_owned_bindings_for_source.contains_key(*binding))
            .cloned()
        {
            file.add_binding(PlannedBinding::new(
                binding.clone(),
                binding.clone(),
                BindingShape::Unknown,
                true,
            ));
            file.add_export_with_source_backed(binding, true);
        }
        for setter in setter_bindings
            .iter()
            .map(|binding| BindingName::new(runtime_helper_setter_name(binding)))
        {
            file.add_binding(PlannedBinding::new(
                setter.clone(),
                setter.clone(),
                BindingShape::Callable,
                true,
            ));
            file.add_export_with_source_backed(setter, true);
        }
        for lazy_name in [
            exports_lazy_module.then_some("lazyModule"),
            exports_lazy_value.then_some("lazyValue"),
        ]
        .into_iter()
        .flatten()
        {
            let binding = BindingName::new(lazy_name);
            file.add_binding(PlannedBinding::new(
                binding.clone(),
                binding.clone(),
                BindingShape::Callable,
                true,
            ));
            file.add_export_with_source_backed(binding, true);
        }
        if file.body.is_empty() {
            continue;
        }
        plan.push_file(file);
    }
    Ok(())
}

pub(crate) fn planned_runtime_helper_consumed_bindings(
    plan: &EmitPlan,
    source_file_id: u32,
) -> std::collections::BTreeSet<reverts_ir::BindingName> {
    use crate::relative_paths::relative_import_specifier;
    use crate::statement_parsers::{
        parse_generated_named_import_statement, parse_generated_named_reexport_statement,
    };
    use crate::statements::runtime_helpers_path;
    let helper_path = runtime_helpers_path(source_file_id);
    let mut consumed = std::collections::BTreeSet::new();
    for file in &plan.files {
        let specifier = relative_import_specifier(file.path.as_str(), helper_path.as_str());
        for source in &file.body {
            if let Some((bindings, import_specifier)) =
                parse_generated_named_import_statement(source)
                && import_specifier == specifier
            {
                consumed.extend(bindings);
                continue;
            }
            if let Some((bindings, reexport_specifier)) =
                parse_generated_named_reexport_statement(source)
                && reexport_specifier == specifier
            {
                consumed.extend(bindings);
            }
        }
    }
    consumed
}
