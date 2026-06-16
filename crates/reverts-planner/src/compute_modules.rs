//! Per-module emit-planning loop body, lifted out of
//! `lib.rs::ImportExportPlanner::plan_enriched_program` so that loop
//! can be read as a linear sequence of named operations. `plan_one_module`
//! is the only public entry point; it consumes the planner-wide
//! analysis maps and accumulates into the helper-file usage maps that
//! drive downstream runtime-helper-file emission.

use std::collections::{BTreeMap, BTreeSet};

use reverts_input::ModuleInput;
use reverts_ir::{BindingName, ModuleId, ModuleKind};
use reverts_model::EnrichedProgram;

use crate::binding_owner::BindingOwnerPlan;
use crate::compiler_recovery::CompilerRecoveryDecision;
use crate::package_runtime::{
    PackageRuntimeHelperKey, PackageRuntimeHelperUsage, package_runtime_owner_for_module,
};
use crate::pure_reexport_bypass::PureReexportBypassPlan;
use crate::runtime_singleton_inline::{
    RuntimeSingletonInlineEmitContext, RuntimeSingletonInlinePlan,
    emit_runtime_singleton_inline_helpers, partition_runtime_singleton_inline_bindings,
};
use crate::runtime_var_migration::RuntimeVarMigrationPlan;
use crate::{
    EmitPlan, ExternalPackageAdapterPlan, LoweredRuntimeModuleSource, OwnerMigrationState,
    PlanError, PlannedFile, RuntimeLazyFoldPlan, RuntimePreludeDirectImport, SourceModuleWiring,
    add_migrated_local_binding_declarations, adjust_remaining_runtime_helpers,
    adjust_written_runtime_helpers, build_lowered_module_source, build_runtime_import_partitions,
    compute_localized_noop_runtime_helpers, compute_namespace_member_rewrite,
    compute_node_builtin_require_helpers, compute_node_builtin_require_rewrite,
    compute_runtime_sources_for_module, compute_source_runtime_refs, contains_call_to_identifier,
    emit_direct_owner_import_aliases, emit_direct_owner_imports, emit_direct_prelude_imports,
    emit_folded_direct_stub_reexports, emit_folded_runtime_stub_reexports,
    emit_lowered_package_runtime_imports, emit_lowered_runtime_helper_import,
    emit_migrated_extra_chunks, emit_migrated_extra_owner_imports,
    emit_migrated_extra_runtime_reexport_imports, emit_migrated_locally_var_declarations,
    emit_migrated_runtime_extra_alias_imports, emit_module_definition_bindings,
    emit_node_builtin_default_imports, emit_runtime_extra_alias_imports,
    emit_runtime_extra_deps_imports, emit_runtime_import_partitions, emit_source_import_bindings,
    emit_source_module_imports, external_package_adapter_emit, extra_exports_for_module,
    filter_remaining_helpers_by_write_rewrite, filter_remaining_helpers_namespace_and_require,
    filter_unreferenced_namespace_helpers, folded_runtime_required_bindings, group_runtime_imports,
    implicit_global_declarations_for_module, lazy_helper_import_names_for_source,
    named_export_statement, namespace_export_helpers_for_source, node_require_prelude_statement,
    noop_function_statement, normalize_source_for_emit, partition_folded_stub_exports,
    partition_runtime_owner_bindings, push_folded_noop_and_migrated_exports,
    push_migrated_runtime_snippets_and_namespaces, push_package_imports,
    record_lowered_runtime_helper_usage, route_prelude_imports_for_runtime_sources,
    try_localize_lazy_value, try_post_inline_localize_lazy_value, variable_declaration_statement,
};

/// Plan a single module's emit output. The body of the per-module
/// loop, lifted out of `ImportExportPlanner::plan_enriched_program`
/// so that loop can be skimmed as a linear sequence of named
/// operations. Mutates the accumulator maps that downstream
/// runtime-helper-file emission relies on. Returns early (with
/// `Ok(())`) when the module is an externalized package, a pure-
/// reexport bypass, or a folded stub with no internal consumers.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_one_module(
    program: &EnrichedProgram,
    module: &ModuleInput,
    plan: &mut EmitPlan,
    used_runtime_helper_files: &mut BTreeMap<u32, BTreeSet<BindingName>>,
    exported_runtime_helper_bindings: &mut BTreeMap<u32, BTreeSet<BindingName>>,
    required_runtime_helper_bindings: &mut BTreeMap<u32, BTreeSet<BindingName>>,
    used_runtime_helper_setters: &mut BTreeMap<u32, BTreeSet<BindingName>>,
    used_lazy_module: &mut BTreeSet<u32>,
    used_lazy_value: &mut BTreeSet<u32>,
    exported_lazy_module: &mut BTreeSet<u32>,
    exported_lazy_value: &mut BTreeSet<u32>,
    used_package_runtime_helper_files: &mut BTreeMap<
        PackageRuntimeHelperKey,
        PackageRuntimeHelperUsage,
    >,
    external_package_adapters: &BTreeMap<ModuleId, ExternalPackageAdapterPlan>,
    externalized_packages: &BTreeSet<ModuleId>,
    source_suppressed_packages: &BTreeSet<ModuleId>,
    source_module_wiring: &SourceModuleWiring,
    lowered_runtime_sources: &BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
    runtime_lazy_folds: &RuntimeLazyFoldPlan,
    omitted_folded_stub_modules: &BTreeSet<ModuleId>,
    pure_reexport_bypasses: &PureReexportBypassPlan,
    runtime_var_migrations: &RuntimeVarMigrationPlan,
    runtime_prelude_direct_imports: &BTreeMap<
        u32,
        BTreeMap<BindingName, RuntimePreludeDirectImport>,
    >,
    runtime_singleton_inlines: &RuntimeSingletonInlinePlan,
    runtime_edge_direct_prelude_imports: &BTreeMap<u32, BTreeSet<BindingName>>,
    binding_owners: &BindingOwnerPlan,
) -> Result<(), PlanError> {
    if module.kind == ModuleKind::Package && externalized_packages.contains(&module.id) {
        return Ok(());
    }

    let path = program
        .semantic_names()
        .module_path(module.id)
        .unwrap_or(module.semantic_path.as_str());
    let package_runtime_owner =
        package_runtime_owner_for_module(module, source_suppressed_packages);
    let compiler_profile = program.compiler_profile().module(module.id);
    let compiler_recovery = CompilerRecoveryDecision::from_profile(&compiler_profile);
    let mut adapter_file = PlannedFile::new(path);
    adapter_file.set_compiler_recovery(compiler_recovery.clone());
    if external_package_adapter_emit::try_emit_external_package_adapter(
        program,
        module,
        external_package_adapters,
        adapter_file,
        plan,
    ) {
        return Ok(());
    }
    let mut file = PlannedFile::new(path);
    file.set_compiler_recovery(compiler_recovery);
    let mut planned_bindings = BTreeSet::<BindingName>::new();
    let mut emitted_inline_runtime_helpers = BTreeSet::<(u32, BindingName)>::new();

    if pure_reexport_bypasses.omitted_modules.contains(&module.id) {
        return Ok(());
    }

    if let Some(folded) = runtime_lazy_folds.modules.get(&module.id) {
        let migrated_extra_snippets = runtime_var_migrations.extra_snippets_for_owner(module.id);
        let migrated_extra_namespace_exports =
            runtime_var_migrations.extra_namespace_exports_for_owner(module.id);
        let migrated_extra_namespace_bindings = migrated_extra_namespace_exports
            .iter()
            .map(|(_, binding)| binding.clone())
            .collect::<BTreeSet<_>>();
        let migrated_extra_noop_deps = runtime_var_migrations.extra_noop_deps_for_owner(module.id);
        let migrated_extra_runtime_deps_by_source =
            runtime_var_migrations.extra_runtime_deps_by_source_for_owner(module.id);
        let migrated_extra_runtime_owner_deps =
            runtime_var_migrations.migrated_extra_runtime_deps_for_owner(module.id);
        let migrated_extra_runtime_owner_dep_aliases =
            runtime_var_migrations.migrated_aliased_extra_runtime_deps_for_owner(module.id);
        let migrated_extra_runtime_dep_aliases =
            runtime_var_migrations.extra_runtime_dep_aliases_for_owner(module.id);
        let migrated_runtime_extra_runtime_dep_aliases =
            runtime_var_migrations.runtime_extra_runtime_dep_aliases_for_owner(module.id);
        let migrated_local_bindings = runtime_var_migrations.local_bindings_for_owner(module.id);
        let (runtime_stub_exports, direct_stub_exports) =
            partition_folded_stub_exports(folded, module.id, binding_owners);
        let runtime_required_bindings =
            folded_runtime_required_bindings(folded, module.id, binding_owners);
        if !runtime_required_bindings.is_empty() {
            required_runtime_helper_bindings
                .entry(folded.source_file_id)
                .or_default()
                .extend(runtime_required_bindings);
        }
        // Phase 13: a folded module whose only in-project purpose is
        // `export { X } from runtime` is a pure forwarding shim. When
        // we can rewrite every internal consumer to import the folded
        // binding directly from the runtime helper, omit that shim
        // entirely. Modules with no internal consumers keep their stub
        // file so an explicit/source export surface is still visible.
        if omitted_folded_stub_modules.contains(&module.id) && migrated_local_bindings.is_empty() {
            return Ok(());
        }
        emit_folded_runtime_stub_reexports(
            folded.source_file_id,
            path,
            &runtime_stub_exports,
            &mut file,
            used_runtime_helper_files,
            exported_runtime_helper_bindings,
        );
        emit_folded_direct_stub_reexports(program, path, &direct_stub_exports, &mut file);
        if !migrated_extra_runtime_owner_deps.is_empty() {
            emit_direct_owner_imports(
                program,
                module.id,
                path,
                &mut file,
                &mut planned_bindings,
                &migrated_extra_runtime_owner_deps,
            );
        }
        if !migrated_extra_runtime_owner_dep_aliases.is_empty() {
            emit_direct_owner_import_aliases(
                program,
                path,
                &mut file,
                &mut planned_bindings,
                &migrated_extra_runtime_owner_dep_aliases,
            );
        }
        emit_runtime_extra_alias_imports(
            path,
            &migrated_runtime_extra_runtime_dep_aliases,
            &mut file,
            &mut planned_bindings,
            used_runtime_helper_files,
            exported_runtime_helper_bindings,
            required_runtime_helper_bindings,
        );
        emit_runtime_extra_deps_imports(
            program,
            module.id,
            path,
            &migrated_extra_runtime_deps_by_source,
            &mut file,
            &mut planned_bindings,
            used_runtime_helper_files,
            exported_runtime_helper_bindings,
            required_runtime_helper_bindings,
        );
        push_migrated_runtime_snippets_and_namespaces(
            program,
            &migrated_extra_snippets,
            &migrated_extra_namespace_exports,
            &migrated_extra_runtime_dep_aliases,
            &mut file,
        );
        push_folded_noop_and_migrated_exports(
            folded,
            &runtime_stub_exports,
            &direct_stub_exports,
            &migrated_extra_noop_deps,
            &migrated_local_bindings,
            &migrated_extra_namespace_bindings,
            &mut file,
            &mut planned_bindings,
        );
        crate::finalize_planned_file(&mut file);
        plan.push_file(file);
        return Ok(());
    }

    push_package_imports(program, module.id, &mut file);

    let has_runtime_edge_before_lazy_helpers = emit_source_module_imports(
        program,
        module.id,
        path,
        source_module_wiring,
        pure_reexport_bypasses,
        runtime_lazy_folds,
        omitted_folded_stub_modules,
        binding_owners,
        &mut file,
        &mut planned_bindings,
        used_runtime_helper_files,
        exported_runtime_helper_bindings,
    );

    let runtime_imports = program.model().graph().runtime_imports_for(module.id);
    let runtime_import_groups = group_runtime_imports(runtime_imports);
    let lowered_source = lowered_runtime_sources.get(&module.id);
    let lowered_helpers = lowered_source
        .map(|source| source.lowered_helpers.clone())
        .unwrap_or_default();
    let local_source_definitions = lowered_source
        .as_ref()
        .map(|source| source.local_definitions.clone())
        .unwrap_or_default();
    let local_source_writes = lowered_source
        .as_ref()
        .map(|source| source.local_writes.clone())
        .unwrap_or_default();
    let source_imports = program.model().graph().ast_imports_for(module.id);
    let remaining_runtime_helpers = lowered_source
        .map(|source| source.remaining_helpers.clone())
        .unwrap_or_default();
    let written_runtime_helpers = lowered_source
        .map(|source| source.written_helpers.clone())
        .unwrap_or_default();
    let OwnerMigrationState {
        migrated_locally,
        migrated_extra_snippets,
        migrated_extra_namespace_exports,
        migrated_extra_namespace_bindings,
        migrated_extra_runtime_deps,
        migrated_extra_runtime_setter_deps,
        migrated_extra_runtime_setter_deps_by_source,
        migrated_extra_runtime_owner_deps,
        migrated_extra_runtime_owner_dep_aliases,
        migrated_extra_runtime_dep_aliases,
        migrated_runtime_extra_runtime_dep_aliases,
        migrated_extra_source_deps,
        migrated_extra_runtime_reexport_deps,
        migrated_extra_noop_deps,
        migrated_local_bindings,
    } = OwnerMigrationState::from_plan(runtime_var_migrations, module.id);
    let remaining_runtime_helpers = adjust_remaining_runtime_helpers(
        &remaining_runtime_helpers,
        &migrated_extra_runtime_deps,
        &migrated_local_bindings,
    );
    let written_runtime_helpers = adjust_written_runtime_helpers(
        &written_runtime_helpers,
        &migrated_locally,
        &migrated_extra_runtime_setter_deps,
    );
    let namespace_member_rewrite = compute_namespace_member_rewrite(
        program,
        lowered_source,
        &runtime_import_groups,
        &local_source_definitions,
        &source_imports,
        &planned_bindings,
    );
    let node_builtin_require_helpers = compute_node_builtin_require_helpers(
        program,
        lowered_source,
        runtime_prelude_direct_imports,
    );
    let node_builtin_require_rewrite = compute_node_builtin_require_rewrite(
        lowered_source,
        namespace_member_rewrite.as_ref(),
        &node_builtin_require_helpers,
        &local_source_definitions,
        &source_imports,
        &planned_bindings,
    );
    let source_runtime_refs = compute_source_runtime_refs(
        program,
        module.id,
        source_module_wiring,
        lowered_source,
        namespace_member_rewrite.as_ref(),
        node_builtin_require_rewrite.as_ref(),
    );
    let remaining_runtime_helpers = filter_remaining_helpers_by_write_rewrite(
        program,
        module.id,
        source_module_wiring,
        lowered_source,
        namespace_member_rewrite.as_ref(),
        node_builtin_require_rewrite.as_ref(),
        &written_runtime_helpers,
        &migrated_extra_runtime_deps,
        remaining_runtime_helpers,
    );
    let namespace_export_helpers_for_source =
        namespace_export_helpers_for_source(program, lowered_source);
    let remaining_runtime_helpers = filter_unreferenced_namespace_helpers(
        remaining_runtime_helpers,
        &namespace_export_helpers_for_source,
        &source_runtime_refs,
    );
    let (remaining_runtime_helpers, consumed_node_builtin_require_helpers) =
        filter_remaining_helpers_namespace_and_require(
            remaining_runtime_helpers,
            lowered_source,
            namespace_member_rewrite.as_ref(),
            node_builtin_require_rewrite.as_ref(),
        );
    let localized_noop_runtime_helpers = compute_localized_noop_runtime_helpers(
        program,
        module.id,
        source_module_wiring,
        lowered_source,
        namespace_member_rewrite.as_ref(),
        node_builtin_require_rewrite.as_ref(),
        &remaining_runtime_helpers,
    );
    let remaining_runtime_helpers: BTreeSet<BindingName> = remaining_runtime_helpers
        .difference(&localized_noop_runtime_helpers)
        .cloned()
        .collect();
    let mut runtime_import_partitions = build_runtime_import_partitions(
        program,
        module.id,
        runtime_import_groups,
        binding_owners,
        namespace_member_rewrite.as_ref(),
        &source_runtime_refs,
        &lowered_helpers,
        &written_runtime_helpers,
        &consumed_node_builtin_require_helpers,
        &localized_noop_runtime_helpers,
        &remaining_runtime_helpers,
        &planned_bindings,
        &local_source_definitions,
        &local_source_writes,
    );
    let has_runtime_group_imports = runtime_import_partitions
        .iter()
        .any(|(_, partition)| !partition.runtime_bindings.is_empty());
    let mut lowered_import_partition = lowered_source
        .map(|lowered_source| {
            partition_runtime_owner_bindings(
                binding_owners,
                lowered_source.source_file_id,
                module.id,
                remaining_runtime_helpers.clone(),
            )
        })
        .unwrap_or_default();
    let mut lazy_helper_names = lowered_source
        .map(lazy_helper_import_names_for_source)
        .unwrap_or_default();
    let mut localized_lazy_value_source = try_localize_lazy_value(
        lowered_source,
        namespace_member_rewrite.as_ref(),
        has_runtime_edge_before_lazy_helpers,
        &written_runtime_helpers,
        has_runtime_group_imports,
        runtime_singleton_inlines,
        module.id,
        &lowered_import_partition.runtime_bindings,
    );
    if localized_lazy_value_source.is_some() {
        lazy_helper_names.retain(|name| *name != "lazyValue");
    }
    let runtime_sources_for_module = compute_runtime_sources_for_module(
        lowered_source,
        &lowered_import_partition,
        &runtime_import_partitions,
        runtime_singleton_inlines,
        module.id,
        &written_runtime_helpers,
        &lazy_helper_names,
    );
    route_prelude_imports_for_runtime_sources(
        lowered_source,
        &mut lowered_import_partition,
        &mut runtime_import_partitions,
        &runtime_sources_for_module,
        runtime_edge_direct_prelude_imports,
    );
    emit_migrated_extra_owner_imports(
        program,
        module.id,
        path,
        &mut file,
        &mut planned_bindings,
        &migrated_extra_source_deps,
        &migrated_extra_runtime_owner_deps,
        &migrated_extra_runtime_owner_dep_aliases,
    );
    emit_migrated_runtime_extra_alias_imports(
        path,
        &mut file,
        &mut planned_bindings,
        used_runtime_helper_files,
        exported_runtime_helper_bindings,
        required_runtime_helper_bindings,
        &migrated_runtime_extra_runtime_dep_aliases,
    );
    emit_migrated_extra_runtime_reexport_imports(
        program,
        module.id,
        path,
        &mut file,
        &mut planned_bindings,
        used_runtime_helper_files,
        exported_runtime_helper_bindings,
        &migrated_extra_runtime_reexport_deps,
    );
    if let Some(lowered_source) = lowered_source
        && (!remaining_runtime_helpers.is_empty()
            || !written_runtime_helpers.is_empty()
            || !lazy_helper_names.is_empty())
    {
        emit_direct_owner_imports(
            program,
            module.id,
            path,
            &mut file,
            &mut planned_bindings,
            &lowered_import_partition.direct_imports,
        );
        emit_direct_prelude_imports(
            &mut file,
            &mut planned_bindings,
            &lowered_import_partition.direct_prelude_imports,
        );
        let (inline_remaining_helpers, remaining_runtime_helpers) =
            partition_runtime_singleton_inline_bindings(
                runtime_singleton_inlines,
                module.id,
                lowered_source.source_file_id,
                &lowered_import_partition.runtime_bindings,
            );
        emit_runtime_singleton_inline_helpers(
            RuntimeSingletonInlineEmitContext {
                program,
                module_id: module.id,
                module_path: path,
            },
            runtime_singleton_inlines,
            &mut file,
            &mut planned_bindings,
            &mut emitted_inline_runtime_helpers,
            lowered_source.source_file_id,
            &inline_remaining_helpers,
        );
        let (
            package_remaining_helpers,
            remaining_runtime_helpers,
            package_written_helpers,
            written_runtime_helpers,
        ) = emit_lowered_package_runtime_imports(
            program,
            module.id,
            path,
            &mut file,
            &mut planned_bindings,
            used_package_runtime_helper_files,
            lowered_source.source_file_id,
            binding_owners,
            package_runtime_owner.as_ref(),
            &remaining_runtime_helpers,
            &written_runtime_helpers,
        );
        if let Some(localized) = try_post_inline_localize_lazy_value(
            lowered_source,
            namespace_member_rewrite.as_ref(),
            localized_lazy_value_source.as_ref(),
            has_runtime_edge_before_lazy_helpers,
            &remaining_runtime_helpers,
            &written_runtime_helpers,
            &package_remaining_helpers,
            &package_written_helpers,
            &lazy_helper_names,
            &runtime_import_partitions,
            runtime_singleton_inlines,
            binding_owners,
            package_runtime_owner.as_ref(),
            module.id,
        ) {
            localized_lazy_value_source = Some(localized);
            lazy_helper_names.retain(|name| *name != "lazyValue");
        }
        record_lowered_runtime_helper_usage(
            lowered_source,
            &remaining_runtime_helpers,
            &written_runtime_helpers,
            &lazy_helper_names,
            used_runtime_helper_files,
            exported_runtime_helper_bindings,
            required_runtime_helper_bindings,
            used_runtime_helper_setters,
            used_lazy_module,
            used_lazy_value,
            exported_lazy_module,
            exported_lazy_value,
        );
        emit_lowered_runtime_helper_import(
            program,
            module.id,
            path,
            &mut file,
            &mut planned_bindings,
            lowered_source.source_file_id,
            &remaining_runtime_helpers,
            &written_runtime_helpers,
            &lazy_helper_names,
        );
    }

    emit_runtime_import_partitions(
        program,
        module.id,
        path,
        &mut file,
        &mut planned_bindings,
        used_runtime_helper_files,
        exported_runtime_helper_bindings,
        required_runtime_helper_bindings,
        used_package_runtime_helper_files,
        &mut emitted_inline_runtime_helpers,
        runtime_import_partitions,
        runtime_singleton_inlines,
        binding_owners,
        package_runtime_owner.as_ref(),
    );

    if let Some(rewrite) = &node_builtin_require_rewrite
        && !rewrite.imports.is_empty()
    {
        emit_node_builtin_default_imports(&mut file, &mut planned_bindings, &rewrite.imports);
    }

    let source_definitions = program.model().graph().ast_definitions_for(module.id);
    emit_module_definition_bindings(
        program,
        module.id,
        &mut file,
        &mut planned_bindings,
        lowered_source,
    );

    emit_source_import_bindings(
        program,
        module.id,
        &mut file,
        &mut planned_bindings,
        &source_imports,
    );

    if lowered_source.is_none() && !migrated_local_bindings.is_empty() {
        add_migrated_local_binding_declarations(
            &mut file,
            &mut planned_bindings,
            &migrated_local_bindings,
            &migrated_locally,
            &migrated_extra_namespace_bindings,
        );
        emit_migrated_locally_var_declarations(
            &mut file,
            &migrated_locally,
            runtime_var_migrations,
        );
        emit_migrated_extra_chunks(
            program,
            &mut file,
            &migrated_extra_snippets,
            &migrated_extra_namespace_exports,
            &migrated_extra_runtime_dep_aliases,
            &migrated_extra_runtime_setter_deps_by_source,
        );
        for binding in &migrated_extra_noop_deps {
            file.push_source(noop_function_statement(binding));
        }
    }

    if let Some(lowered_source) = lowered_source {
        let source_file_path = lowered_source.source_file_path.as_str();
        let source = build_lowered_module_source(
            lowered_source,
            localized_lazy_value_source.as_deref(),
            namespace_member_rewrite.as_ref(),
            &localized_noop_runtime_helpers,
            node_builtin_require_rewrite.as_ref(),
            &node_builtin_require_helpers,
            &written_runtime_helpers,
        );
        if contains_call_to_identifier(source.as_str(), "require")
            && !local_source_definitions.contains(&BindingName::new("require"))
        {
            file.push_source(node_require_prelude_statement());
            planned_bindings.insert(BindingName::new("require"));
        }
        // Phase 10b: register migrated bindings as planned BEFORE
        // computing implicit globals — otherwise the implicit-
        // globals scan picks the same bindings up as undeclared
        // writes and emits a redundant `var X;` line alongside
        // the migration's own declaration.
        add_migrated_local_binding_declarations(
            &mut file,
            &mut planned_bindings,
            &migrated_local_bindings,
            &migrated_locally,
            &migrated_extra_namespace_bindings,
        );
        let implicit_globals = implicit_global_declarations_for_module(
            source.as_str(),
            &source_definitions,
            &source_imports,
            &planned_bindings,
        );
        if !implicit_globals.is_empty() {
            file.push_source(variable_declaration_statement(implicit_globals.iter()));
        }
        // Declare the bindings the runtime helpers module no
        // longer hosts. Their `X = value` writes (left
        // unrewritten above because we excluded them from
        // `written_runtime_helpers`) now bind to the same-module
        // `var X;` slot. Cross-module readers are routed directly to
        // this owner module instead of through the runtime helpers
        // barrel. Bindings that came with a literal initializer emit
        // `var X = INIT;` so the writer keeps the original
        // initial value the runtime used to set at load.
        emit_migrated_locally_var_declarations(
            &mut file,
            &migrated_locally,
            runtime_var_migrations,
        );
        emit_migrated_extra_chunks(
            program,
            &mut file,
            &migrated_extra_snippets,
            &migrated_extra_namespace_exports,
            &migrated_extra_runtime_dep_aliases,
            &migrated_extra_runtime_setter_deps_by_source,
        );
        for binding in &migrated_extra_noop_deps {
            file.push_source(noop_function_statement(binding));
        }
        let normalized = normalize_source_for_emit(
            module.id,
            source_file_path,
            source.as_str(),
            file.source_strategy(),
        )?;
        file.push_source(normalized);
    }

    for export in program
        .model()
        .graph()
        .import_export()
        .exports_for(module.id)
    {
        file.add_export_with_source_backed(export, true);
    }
    // Phase 10b: bindings this module now owns must be exported so
    // direct-routed consumers resolve through the live binding. Skip
    // any binding that is already exported through the module's
    // regular AST or wiring path to avoid duplicate-export audit
    // failures.
    if !migrated_local_bindings.is_empty() {
        let already_exported: BTreeSet<BindingName> = program
            .model()
            .graph()
            .import_export()
            .exports_for(module.id)
            .into_iter()
            .chain(
                source_module_wiring
                    .exports_by_module
                    .get(&module.id)
                    .cloned()
                    .unwrap_or_default(),
            )
            .collect();
        let new_exports: BTreeSet<BindingName> = migrated_local_bindings
            .difference(&already_exported)
            .cloned()
            .collect();
        if !new_exports.is_empty() {
            file.push_source(named_export_statement(new_exports.iter()));
            for binding in &new_exports {
                file.add_export_with_source_backed(binding.clone(), true);
            }
        }
    }
    let migration_source_exports = runtime_var_migrations.source_dep_exports_for_module(module.id);
    if !migration_source_exports.is_empty() {
        let already_exported = file
            .exports
            .iter()
            .map(|export| export.binding.clone())
            .collect::<BTreeSet<_>>();
        let new_exports = migration_source_exports
            .difference(&already_exported)
            .cloned()
            .collect::<BTreeSet<_>>();
        if !new_exports.is_empty() {
            file.push_source(named_export_statement(new_exports.iter()));
            for binding in &new_exports {
                file.add_export_with_source_backed(binding.clone(), true);
            }
        }
    }
    let extra_exports = extra_exports_for_module(
        program,
        module.id,
        [source_module_wiring.exports_by_module.get(&module.id)],
    );
    if !extra_exports.is_empty() {
        let existing_exports = program
            .model()
            .graph()
            .import_export()
            .exports_for(module.id)
            .into_iter()
            .collect::<BTreeSet<_>>();
        let extra_exports = extra_exports
            .into_iter()
            .filter(|binding| !existing_exports.contains(binding))
            .collect::<BTreeSet<_>>();
        if !extra_exports.is_empty() {
            file.push_source(named_export_statement(extra_exports.iter()));
            for export in extra_exports {
                file.add_export_with_source_backed(export, true);
            }
        }
    }

    crate::finalize_planned_file(&mut file);
    plan.push_file(file);

    Ok(())
}
