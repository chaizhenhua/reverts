//! Per-module emit-planning loop body, lifted out of
//! `lib.rs::ImportExportPlanner::plan_enriched_program` so that loop
//! can be read as a linear sequence of named operations. `plan_one_module`
//! is the only public entry point; it consumes the planner-wide
//! analysis maps and accumulates into the helper-file usage maps that
//! drive downstream runtime-helper-file emission.

use std::collections::{BTreeMap, BTreeSet};

use reverts_input::ModuleInput;
use reverts_ir::{BindingName, ModuleId};
use reverts_js::is_js_keyword;
use reverts_model::EnrichedProgram;

use crate::binding_owner::BindingOwnerPlan;
use crate::compiler_preservation::CompilerPreservationDecision;
use crate::local_bindings::local_bindings_in_source;
use crate::named_specifiers::{
    local_named_export_specifiers, named_reexport_specifiers, source_statements,
};
use crate::node_builtin_require::NodeBuiltinRequireRewrite;
use crate::package_runtime::{
    PackageRuntimeHelperKey, PackageRuntimeHelperUsage, package_runtime_owner_for_module,
};
use crate::pure_reexport_bypass::PureReexportBypassPlan;
use crate::runtime_namespace_rewrite::RuntimeNamespaceMemberAccessRewrite;
use crate::runtime_singleton_inline::{
    RuntimeSingletonInlineEmitContext, RuntimeSingletonInlinePlan,
    emit_runtime_singleton_inline_helpers, partition_runtime_singleton_inline_bindings,
};
use crate::runtime_var_migration::RuntimeVarMigrationPlan;
use crate::{
    EmitPlan, ExternalPackageAdapterPlan, FoldedNoopAndMigratedExportsArgs,
    LazyValueLocalizationArgs, LocalizedNoopRuntimeHelperArgs, LoweredPackageRuntimeImportArgs,
    LoweredRuntimeHelperImportArgs, LoweredRuntimeHelperUsageArgs, LoweredRuntimeModuleSource,
    MigratedExtraOwnerImportArgs, MigratedExtraRuntimeReexportImportArgs,
    MigratedRuntimeExtraAliasImportArgs, OwnerMigrationState, PlanError, PlannedFile,
    PlannedRename, PlannedTypeAnnotation, PostInlineLazyValueLocalizationArgs,
    RemainingHelpersWriteRewriteArgs, RuntimeExtraDepsImportArgs, RuntimeImportPartitionArgs,
    RuntimeImportPartitionEmitArgs, RuntimeLazyFoldPlan, RuntimePreludeDirectImport,
    SourceModuleImportEmitArgs, SourceModuleWiring, add_migrated_local_binding_declarations,
    adjust_remaining_runtime_helpers, adjust_written_runtime_helpers, build_lowered_module_source,
    build_runtime_import_partitions, compute_localized_noop_runtime_helpers,
    compute_namespace_member_rewrite, compute_node_builtin_require_helpers,
    compute_node_builtin_require_rewrite, compute_runtime_sources_for_module,
    compute_source_runtime_refs, contains_call_to_identifier, emit_direct_owner_import_aliases,
    emit_direct_owner_imports, emit_direct_prelude_imports, emit_folded_direct_stub_reexports,
    emit_folded_runtime_stub_reexports, emit_lowered_package_runtime_imports,
    emit_lowered_runtime_helper_import, emit_migrated_extra_chunks,
    emit_migrated_extra_owner_imports, emit_migrated_extra_runtime_reexport_imports,
    emit_migrated_locally_var_declarations, emit_migrated_runtime_extra_alias_imports,
    emit_module_definition_bindings, emit_node_builtin_default_imports,
    emit_runtime_extra_alias_imports, emit_runtime_extra_deps_imports,
    emit_runtime_import_partitions, emit_source_import_bindings, emit_source_module_imports,
    external_package_adapter_emit, extra_exports_for_module,
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

/// Immutable planner facts needed while planning one module.
///
/// Keeping these as one typed argument makes the module planner's boundary
/// explicit: callers can no longer smuggle new horizontal dependencies into
/// `plan_one_module` by adding another positional parameter.
pub(crate) struct ModulePlanInput<'a> {
    pub(crate) program: &'a EnrichedProgram,
    pub(crate) module: &'a ModuleInput,
    pub(crate) external_package_adapters: &'a BTreeMap<ModuleId, ExternalPackageAdapterPlan>,
    pub(crate) externalized_packages: &'a BTreeSet<ModuleId>,
    pub(crate) externalized_package_init_bindings: &'a BTreeSet<BindingName>,
    pub(crate) source_suppressed_packages: &'a BTreeSet<ModuleId>,
    pub(crate) source_module_wiring: &'a SourceModuleWiring,
    pub(crate) lowered_runtime_sources: &'a BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
    pub(crate) runtime_lazy_folds: &'a RuntimeLazyFoldPlan,
    pub(crate) omitted_folded_stub_modules: &'a BTreeSet<ModuleId>,
    pub(crate) pure_reexport_bypasses: &'a PureReexportBypassPlan,
    pub(crate) runtime_var_migrations: &'a RuntimeVarMigrationPlan,
    pub(crate) runtime_prelude_direct_imports:
        &'a BTreeMap<u32, BTreeMap<BindingName, RuntimePreludeDirectImport>>,
    pub(crate) runtime_singleton_inlines: &'a RuntimeSingletonInlinePlan,
    pub(crate) runtime_edge_direct_prelude_imports: &'a BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) binding_owners: &'a BindingOwnerPlan,
}

/// Mutable planner accumulators updated by one module.
///
/// These are grouped separately from immutable inputs so future extraction can
/// split the module planner into smaller passes without expanding its public
/// signature again.
pub(crate) struct ModulePlanAccumulators<'a> {
    pub(crate) plan: &'a mut EmitPlan,
    pub(crate) used_runtime_helper_files: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) exported_runtime_helper_bindings: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) required_runtime_helper_bindings: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) used_runtime_helper_setters: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) used_lazy_module: &'a mut BTreeSet<u32>,
    pub(crate) used_lazy_value: &'a mut BTreeSet<u32>,
    pub(crate) exported_lazy_module: &'a mut BTreeSet<u32>,
    pub(crate) exported_lazy_value: &'a mut BTreeSet<u32>,
    pub(crate) used_package_runtime_helper_files:
        &'a mut BTreeMap<PackageRuntimeHelperKey, PackageRuntimeHelperUsage>,
}

/// Plan a single module's emit output. The body of the per-module
/// loop, lifted out of `ImportExportPlanner::plan_enriched_program`
/// so that loop can be skimmed as a linear sequence of named
/// operations. Mutates the accumulator maps that downstream
/// runtime-helper-file emission relies on. Returns early (with
/// `Ok(())`) when the module is an externalized package, a pure-
/// reexport bypass, or a folded stub with no internal consumers.
pub(crate) fn plan_one_module(
    input: ModulePlanInput<'_>,
    accumulators: ModulePlanAccumulators<'_>,
) -> Result<(), PlanError> {
    let ModulePlanInput {
        program,
        module,
        external_package_adapters,
        externalized_packages,
        externalized_package_init_bindings,
        source_suppressed_packages,
        source_module_wiring,
        lowered_runtime_sources,
        runtime_lazy_folds,
        omitted_folded_stub_modules,
        pure_reexport_bypasses,
        runtime_var_migrations,
        runtime_prelude_direct_imports,
        runtime_singleton_inlines,
        runtime_edge_direct_prelude_imports,
        binding_owners,
    } = input;
    let ModulePlanAccumulators {
        plan,
        used_runtime_helper_files,
        exported_runtime_helper_bindings,
        required_runtime_helper_bindings,
        used_runtime_helper_setters,
        used_lazy_module,
        used_lazy_value,
        exported_lazy_module,
        exported_lazy_value,
        used_package_runtime_helper_files,
    } = accumulators;
    if externalized_packages.contains(&module.id) {
        return Ok(());
    }

    let path = crate::module_output_path(program, module.id)
        .expect("module from program must have an output path");
    let path = path.as_str();
    let package_runtime_owner =
        package_runtime_owner_for_module(module, source_suppressed_packages);
    let compiler_profile = program.compiler_profile().module(module.id);
    let compiler_preservation = CompilerPreservationDecision::from_profile(&compiler_profile);
    let mut adapter_file = PlannedFile::new(path);
    adapter_file.set_compiler_preservation(compiler_preservation.clone());
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
    file.set_compiler_preservation(compiler_preservation);
    let mut planned_bindings = BTreeSet::<BindingName>::new();
    let mut emitted_inline_runtime_helpers = BTreeSet::<(u32, BindingName)>::new();

    if pure_reexport_bypasses.omitted_modules.contains(&module.id) {
        return Ok(());
    }

    if (FoldedModulePass {
        program,
        module,
        path,
        runtime_lazy_folds,
        runtime_var_migrations,
        omitted_folded_stub_modules,
        binding_owners,
        used_runtime_helper_files,
        exported_runtime_helper_bindings,
        required_runtime_helper_bindings,
    })
    .run(&mut file, &mut planned_bindings, plan)
    {
        return Ok(());
    }

    push_package_imports(program, module.id, &mut file);

    let has_runtime_edge_before_lazy_helpers =
        emit_source_module_imports(SourceModuleImportEmitArgs {
            program,
            module_id: module.id,
            path,
            source_module_wiring,
            pure_reexport_bypasses,
            runtime_lazy_folds,
            omitted_folded_stub_modules,
            binding_owners,
            file: &mut file,
            planned_bindings: &mut planned_bindings,
            used_runtime_helper_files,
            exported_runtime_helper_bindings,
        });

    let migration = OwnerMigrationState::from_plan(runtime_var_migrations, module.id);
    let runtime_output = NormalRuntimePass {
        program,
        module,
        path,
        lowered_runtime_sources,
        source_module_wiring,
        runtime_prelude_direct_imports,
        runtime_singleton_inlines,
        runtime_edge_direct_prelude_imports,
        binding_owners,
        externalized_packages,
        externalized_package_init_bindings,
        package_runtime_owner: package_runtime_owner.as_ref(),
        has_runtime_edge_before_lazy_helpers,
        migration: &migration,
        used_runtime_helper_files,
        exported_runtime_helper_bindings,
        required_runtime_helper_bindings,
        used_runtime_helper_setters,
        used_lazy_module,
        used_lazy_value,
        exported_lazy_module,
        exported_lazy_value,
        used_package_runtime_helper_files,
    }
    .run(
        &mut file,
        &mut planned_bindings,
        &mut emitted_inline_runtime_helpers,
    );

    NormalModuleBodyPass {
        program,
        module_id: module.id,
        lowered_source: runtime_output.lowered_source,
        runtime_var_migrations,
        source_imports: &runtime_output.source_imports,
        local_source_definitions: &runtime_output.local_source_definitions,
        namespace_member_rewrite: runtime_output.namespace_member_rewrite.as_ref(),
        localized_lazy_value_source: runtime_output.localized_lazy_value_source.as_deref(),
        localized_noop_runtime_helpers: &runtime_output.localized_noop_runtime_helpers,
        node_builtin_require_rewrite: runtime_output.node_builtin_require_rewrite.as_ref(),
        node_builtin_require_helpers: &runtime_output.node_builtin_require_helpers,
        written_runtime_helpers: &runtime_output.written_runtime_helpers,
        migration: &migration,
    }
    .run(&mut file, &mut planned_bindings)?;

    add_inferred_type_annotations(program, module.id, &mut file);

    emit_normal_module_exports(
        program,
        module.id,
        source_module_wiring,
        runtime_var_migrations,
        &migration.migrated_local_bindings,
        &mut file,
    );

    crate::finalize_planned_file(&mut file);
    plan.push_file(file);

    Ok(())
}

fn add_inferred_type_annotations(
    program: &EnrichedProgram,
    module_id: ModuleId,
    file: &mut PlannedFile,
) {
    for (binding, ty) in program.inferred_types_for_module(module_id) {
        file.add_type_annotation(PlannedTypeAnnotation::new(binding, ty));
    }
}

struct FoldedModulePass<'a> {
    program: &'a EnrichedProgram,
    module: &'a ModuleInput,
    path: &'a str,
    runtime_lazy_folds: &'a RuntimeLazyFoldPlan,
    runtime_var_migrations: &'a RuntimeVarMigrationPlan,
    omitted_folded_stub_modules: &'a BTreeSet<ModuleId>,
    binding_owners: &'a BindingOwnerPlan,
    used_runtime_helper_files: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    exported_runtime_helper_bindings: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    required_runtime_helper_bindings: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
}

impl FoldedModulePass<'_> {
    fn run(
        self,
        file: &mut PlannedFile,
        planned_bindings: &mut BTreeSet<BindingName>,
        plan: &mut EmitPlan,
    ) -> bool {
        let Some(folded) = self.runtime_lazy_folds.modules.get(&self.module.id) else {
            return false;
        };
        let migrated_extra_snippets = self
            .runtime_var_migrations
            .extra_snippets_for_owner(self.module.id);
        let migrated_extra_namespace_exports = self
            .runtime_var_migrations
            .extra_namespace_exports_for_owner(self.module.id);
        let migrated_extra_namespace_bindings = migrated_extra_namespace_exports
            .iter()
            .map(|(_, binding)| binding.clone())
            .collect::<BTreeSet<_>>();
        let migrated_extra_noop_deps = self
            .runtime_var_migrations
            .extra_noop_deps_for_owner(self.module.id);
        let migrated_extra_runtime_deps_by_source = self
            .runtime_var_migrations
            .extra_runtime_deps_by_source_for_owner(self.module.id);
        let migrated_extra_runtime_owner_deps = self
            .runtime_var_migrations
            .migrated_extra_runtime_deps_for_owner(self.module.id);
        let migrated_extra_runtime_owner_dep_aliases = self
            .runtime_var_migrations
            .migrated_aliased_extra_runtime_deps_for_owner(self.module.id);
        let migrated_extra_runtime_dep_aliases = self
            .runtime_var_migrations
            .extra_runtime_dep_aliases_for_owner(self.module.id);
        let migrated_runtime_extra_runtime_dep_aliases = self
            .runtime_var_migrations
            .runtime_extra_runtime_dep_aliases_for_owner(self.module.id);
        let migrated_local_bindings = self
            .runtime_var_migrations
            .local_bindings_for_owner(self.module.id);
        let (runtime_stub_exports, direct_stub_exports) =
            partition_folded_stub_exports(folded, self.module.id, self.binding_owners);
        let runtime_required_bindings =
            folded_runtime_required_bindings(folded, self.module.id, self.binding_owners);
        if !runtime_required_bindings.is_empty() {
            self.required_runtime_helper_bindings
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
        if self.omitted_folded_stub_modules.contains(&self.module.id)
            && migrated_local_bindings.is_empty()
        {
            return true;
        }
        emit_folded_runtime_stub_reexports(
            folded.source_file_id,
            self.path,
            &runtime_stub_exports,
            file,
            self.used_runtime_helper_files,
            self.exported_runtime_helper_bindings,
        );
        emit_folded_direct_stub_reexports(self.program, self.path, &direct_stub_exports, file);
        if !migrated_extra_runtime_owner_deps.is_empty() {
            emit_direct_owner_imports(
                self.program,
                self.module.id,
                self.path,
                file,
                planned_bindings,
                &migrated_extra_runtime_owner_deps,
            );
        }
        if !migrated_extra_runtime_owner_dep_aliases.is_empty() {
            emit_direct_owner_import_aliases(
                self.program,
                self.path,
                file,
                planned_bindings,
                &migrated_extra_runtime_owner_dep_aliases,
            );
        }
        emit_runtime_extra_alias_imports(
            self.path,
            &migrated_runtime_extra_runtime_dep_aliases,
            file,
            planned_bindings,
            self.used_runtime_helper_files,
            self.exported_runtime_helper_bindings,
            self.required_runtime_helper_bindings,
        );
        emit_runtime_extra_deps_imports(RuntimeExtraDepsImportArgs {
            program: self.program,
            module_id: self.module.id,
            path: self.path,
            deps_by_source: &migrated_extra_runtime_deps_by_source,
            file: &mut *file,
            planned_bindings: &mut *planned_bindings,
            used_runtime_helper_files: &mut *self.used_runtime_helper_files,
            exported_runtime_helper_bindings: &mut *self.exported_runtime_helper_bindings,
            required_runtime_helper_bindings: &mut *self.required_runtime_helper_bindings,
        });
        let retained_noop_deps = push_migrated_runtime_snippets_and_namespaces(
            self.program,
            &migrated_extra_snippets,
            &migrated_extra_namespace_exports,
            &migrated_extra_runtime_dep_aliases,
            &migrated_extra_noop_deps,
            file,
        );
        push_folded_noop_and_migrated_exports(FoldedNoopAndMigratedExportsArgs {
            folded,
            runtime_stub_exports: &runtime_stub_exports,
            direct_stub_exports: &direct_stub_exports,
            migrated_extra_noop_deps: &retained_noop_deps,
            migrated_local_bindings: &migrated_local_bindings,
            migrated_extra_namespace_bindings: &migrated_extra_namespace_bindings,
            file: &mut *file,
            planned_bindings: &mut *planned_bindings,
        });
        crate::finalize_planned_file(file);
        let pushed_file = std::mem::replace(file, PlannedFile::new(self.path));
        plan.push_file(pushed_file);
        true
    }
}

struct NormalRuntimePass<'a> {
    program: &'a EnrichedProgram,
    module: &'a ModuleInput,
    path: &'a str,
    lowered_runtime_sources: &'a BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
    source_module_wiring: &'a SourceModuleWiring,
    runtime_prelude_direct_imports:
        &'a BTreeMap<u32, BTreeMap<BindingName, RuntimePreludeDirectImport>>,
    runtime_singleton_inlines: &'a RuntimeSingletonInlinePlan,
    runtime_edge_direct_prelude_imports: &'a BTreeMap<u32, BTreeSet<BindingName>>,
    binding_owners: &'a BindingOwnerPlan,
    externalized_packages: &'a BTreeSet<ModuleId>,
    externalized_package_init_bindings: &'a BTreeSet<BindingName>,
    package_runtime_owner: Option<&'a crate::package_runtime::PackageRuntimeOwner>,
    has_runtime_edge_before_lazy_helpers: bool,
    migration: &'a OwnerMigrationState,
    used_runtime_helper_files: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    exported_runtime_helper_bindings: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    required_runtime_helper_bindings: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    used_runtime_helper_setters: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    used_lazy_module: &'a mut BTreeSet<u32>,
    used_lazy_value: &'a mut BTreeSet<u32>,
    exported_lazy_module: &'a mut BTreeSet<u32>,
    exported_lazy_value: &'a mut BTreeSet<u32>,
    used_package_runtime_helper_files:
        &'a mut BTreeMap<PackageRuntimeHelperKey, PackageRuntimeHelperUsage>,
}

struct NormalRuntimePassOutput<'a> {
    lowered_source: Option<&'a LoweredRuntimeModuleSource>,
    source_imports: BTreeSet<BindingName>,
    local_source_definitions: BTreeSet<BindingName>,
    namespace_member_rewrite: Option<RuntimeNamespaceMemberAccessRewrite>,
    localized_lazy_value_source: Option<String>,
    localized_noop_runtime_helpers: BTreeSet<BindingName>,
    node_builtin_require_rewrite: Option<NodeBuiltinRequireRewrite>,
    node_builtin_require_helpers: BTreeSet<BindingName>,
    written_runtime_helpers: BTreeSet<BindingName>,
}

impl<'a> NormalRuntimePass<'a> {
    fn run(
        self,
        file: &mut PlannedFile,
        planned_bindings: &mut BTreeSet<BindingName>,
        emitted_inline_runtime_helpers: &mut BTreeSet<(u32, BindingName)>,
    ) -> NormalRuntimePassOutput<'a> {
        let runtime_imports = self
            .program
            .model()
            .graph()
            .runtime_imports_for(self.module.id);
        let runtime_import_groups = group_runtime_imports(runtime_imports);
        let lowered_source = self.lowered_runtime_sources.get(&self.module.id);
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
        let source_imports = self.program.model().graph().ast_imports_for(self.module.id);
        let remaining_runtime_helpers = lowered_source
            .map(|source| source.remaining_helpers.clone())
            .unwrap_or_default();
        let written_runtime_helpers = lowered_source
            .map(|source| source.written_helpers.clone())
            .unwrap_or_default();
        let remaining_runtime_helpers = adjust_remaining_runtime_helpers(
            &remaining_runtime_helpers,
            &self.migration.migrated_extra_runtime_deps,
            &self.migration.migrated_local_bindings,
        );
        let written_runtime_helpers = adjust_written_runtime_helpers(
            &written_runtime_helpers,
            &self.migration.migrated_locally,
            &self.migration.migrated_extra_runtime_setter_deps,
        );
        let namespace_member_rewrite = compute_namespace_member_rewrite(
            self.program,
            lowered_source,
            &runtime_import_groups,
            &local_source_definitions,
            &source_imports,
            planned_bindings,
        );
        let node_builtin_require_helpers = compute_node_builtin_require_helpers(
            self.program,
            lowered_source,
            self.runtime_prelude_direct_imports,
        );
        let node_builtin_require_rewrite = compute_node_builtin_require_rewrite(
            lowered_source,
            namespace_member_rewrite.as_ref(),
            &node_builtin_require_helpers,
            &local_source_definitions,
            &source_imports,
            planned_bindings,
        );
        let source_runtime_refs = compute_source_runtime_refs(
            self.program,
            self.module.id,
            self.source_module_wiring,
            lowered_source,
            namespace_member_rewrite.as_ref(),
            node_builtin_require_rewrite.as_ref(),
        );
        let remaining_runtime_helpers =
            filter_remaining_helpers_by_write_rewrite(RemainingHelpersWriteRewriteArgs {
                program: self.program,
                module_id: self.module.id,
                source_module_wiring: self.source_module_wiring,
                lowered_source,
                namespace_member_rewrite: namespace_member_rewrite.as_ref(),
                node_builtin_require_rewrite: node_builtin_require_rewrite.as_ref(),
                written_runtime_helpers: &written_runtime_helpers,
                migrated_extra_runtime_deps: &self.migration.migrated_extra_runtime_deps,
                remaining_runtime_helpers,
            });
        let namespace_export_helpers_for_source =
            namespace_export_helpers_for_source(self.program, lowered_source);
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
        let localized_noop_runtime_helpers =
            compute_localized_noop_runtime_helpers(LocalizedNoopRuntimeHelperArgs {
                program: self.program,
                module_id: self.module.id,
                source_module_wiring: self.source_module_wiring,
                lowered_source,
                namespace_member_rewrite: namespace_member_rewrite.as_ref(),
                node_builtin_require_rewrite: node_builtin_require_rewrite.as_ref(),
                remaining_runtime_helpers: &remaining_runtime_helpers,
            });
        let mut localized_noop_runtime_helpers = localized_noop_runtime_helpers;
        localized_noop_runtime_helpers.extend(
            rewritable_externalized_package_init_shims_for_source(
                lowered_source,
                namespace_member_rewrite.as_ref(),
                self.externalized_packages,
                self.externalized_package_init_bindings,
            ),
        );
        let remaining_runtime_helpers: BTreeSet<BindingName> = remaining_runtime_helpers
            .difference(&localized_noop_runtime_helpers)
            .cloned()
            .collect();
        let mut runtime_import_partitions =
            build_runtime_import_partitions(RuntimeImportPartitionArgs {
                program: self.program,
                module_id: self.module.id,
                runtime_import_groups,
                binding_owners: self.binding_owners,
                namespace_member_rewrite: namespace_member_rewrite.as_ref(),
                source_runtime_refs: &source_runtime_refs,
                lowered_helpers: &lowered_helpers,
                written_runtime_helpers: &written_runtime_helpers,
                consumed_node_builtin_require_helpers: &consumed_node_builtin_require_helpers,
                localized_noop_runtime_helpers: &localized_noop_runtime_helpers,
                remaining_runtime_helpers: &remaining_runtime_helpers,
                planned_bindings,
                local_source_definitions: &local_source_definitions,
                local_source_writes: &local_source_writes,
            });
        let has_runtime_group_imports = runtime_import_partitions
            .iter()
            .any(|(_, partition)| !partition.runtime_bindings.is_empty());
        let mut lowered_import_partition = lowered_source
            .map(|lowered_source| {
                partition_runtime_owner_bindings(
                    self.binding_owners,
                    lowered_source.source_file_id,
                    self.module.id,
                    remaining_runtime_helpers.clone(),
                )
            })
            .unwrap_or_default();
        let mut lazy_helper_names = lowered_source
            .map(lazy_helper_import_names_for_source)
            .unwrap_or_default();
        let mut localized_lazy_value_source = try_localize_lazy_value(LazyValueLocalizationArgs {
            lowered_source,
            namespace_member_rewrite: namespace_member_rewrite.as_ref(),
            has_runtime_edge_before_lazy_helpers: self.has_runtime_edge_before_lazy_helpers,
            written_runtime_helpers: &written_runtime_helpers,
            has_runtime_group_imports,
            runtime_singleton_inlines: self.runtime_singleton_inlines,
            module_id: self.module.id,
            lowered_runtime_bindings: &lowered_import_partition.runtime_bindings,
        });
        if localized_lazy_value_source.is_some() {
            lazy_helper_names.retain(|name| *name != "lazyValue");
        }
        let runtime_sources_for_module = compute_runtime_sources_for_module(
            lowered_source,
            &lowered_import_partition,
            &runtime_import_partitions,
            self.runtime_singleton_inlines,
            self.module.id,
            &written_runtime_helpers,
            &lazy_helper_names,
        );
        route_prelude_imports_for_runtime_sources(
            lowered_source,
            &mut lowered_import_partition,
            &mut runtime_import_partitions,
            &runtime_sources_for_module,
            self.runtime_edge_direct_prelude_imports,
        );
        emit_migrated_extra_owner_imports(MigratedExtraOwnerImportArgs {
            program: self.program,
            module_id: self.module.id,
            module_path: self.path,
            file: &mut *file,
            planned_bindings: &mut *planned_bindings,
            migrated_extra_source_deps: &self.migration.migrated_extra_source_deps,
            migrated_extra_runtime_owner_deps: &self.migration.migrated_extra_runtime_owner_deps,
            migrated_extra_runtime_owner_dep_aliases: &self
                .migration
                .migrated_extra_runtime_owner_dep_aliases,
        });
        emit_migrated_runtime_extra_alias_imports(MigratedRuntimeExtraAliasImportArgs {
            module_path: self.path,
            file: &mut *file,
            planned_bindings: &mut *planned_bindings,
            used_runtime_helper_files: &mut *self.used_runtime_helper_files,
            exported_runtime_helper_bindings: &mut *self.exported_runtime_helper_bindings,
            required_runtime_helper_bindings: &mut *self.required_runtime_helper_bindings,
            migrated_runtime_extra_runtime_dep_aliases: &self
                .migration
                .migrated_runtime_extra_runtime_dep_aliases,
        });
        emit_migrated_extra_runtime_reexport_imports(MigratedExtraRuntimeReexportImportArgs {
            program: self.program,
            module_id: self.module.id,
            module_path: self.path,
            file: &mut *file,
            planned_bindings: &mut *planned_bindings,
            used_runtime_helper_files: &mut *self.used_runtime_helper_files,
            exported_runtime_helper_bindings: &mut *self.exported_runtime_helper_bindings,
            migrated_extra_runtime_reexport_deps: &self
                .migration
                .migrated_extra_runtime_reexport_deps,
        });
        if let Some(lowered_source) = lowered_source
            && (!remaining_runtime_helpers.is_empty()
                || !written_runtime_helpers.is_empty()
                || !lazy_helper_names.is_empty())
        {
            emit_direct_owner_imports(
                self.program,
                self.module.id,
                self.path,
                file,
                planned_bindings,
                &lowered_import_partition.direct_imports,
            );
            emit_direct_prelude_imports(
                file,
                planned_bindings,
                &lowered_import_partition.direct_prelude_imports,
            );
            let (inline_remaining_helpers, remaining_runtime_helpers) =
                partition_runtime_singleton_inline_bindings(
                    self.runtime_singleton_inlines,
                    self.module.id,
                    lowered_source.source_file_id,
                    &lowered_import_partition.runtime_bindings,
                );
            emit_runtime_singleton_inline_helpers(
                RuntimeSingletonInlineEmitContext {
                    program: self.program,
                    module_id: self.module.id,
                    module_path: self.path,
                },
                self.runtime_singleton_inlines,
                file,
                planned_bindings,
                emitted_inline_runtime_helpers,
                lowered_source.source_file_id,
                &inline_remaining_helpers,
            );
            let (
                package_remaining_helpers,
                remaining_runtime_helpers,
                package_written_helpers,
                written_runtime_helpers,
            ) = emit_lowered_package_runtime_imports(LoweredPackageRuntimeImportArgs {
                program: self.program,
                module_id: self.module.id,
                module_path: self.path,
                file: &mut *file,
                planned_bindings: &mut *planned_bindings,
                used_package_runtime_helper_files: &mut *self.used_package_runtime_helper_files,
                source_file_id: lowered_source.source_file_id,
                binding_owners: self.binding_owners,
                package_runtime_owner: self.package_runtime_owner,
                remaining_runtime_helpers: &remaining_runtime_helpers,
                written_runtime_helpers: &written_runtime_helpers,
            });
            if let Some(localized) =
                try_post_inline_localize_lazy_value(PostInlineLazyValueLocalizationArgs {
                    lowered_source,
                    namespace_member_rewrite: namespace_member_rewrite.as_ref(),
                    already_localized: localized_lazy_value_source.as_ref(),
                    has_runtime_edge_before_lazy_helpers: self.has_runtime_edge_before_lazy_helpers,
                    remaining_runtime_helpers: &remaining_runtime_helpers,
                    written_runtime_helpers: &written_runtime_helpers,
                    package_remaining_helpers: &package_remaining_helpers,
                    package_written_helpers: &package_written_helpers,
                    lazy_helper_names: &lazy_helper_names,
                    runtime_import_partitions: &runtime_import_partitions,
                    runtime_singleton_inlines: self.runtime_singleton_inlines,
                    binding_owners: self.binding_owners,
                    package_runtime_owner: self.package_runtime_owner,
                    module_id: self.module.id,
                })
            {
                localized_lazy_value_source = Some(localized);
                lazy_helper_names.retain(|name| *name != "lazyValue");
            }
            record_lowered_runtime_helper_usage(LoweredRuntimeHelperUsageArgs {
                lowered_source,
                remaining_runtime_helpers: &remaining_runtime_helpers,
                written_runtime_helpers: &written_runtime_helpers,
                lazy_helper_names: &lazy_helper_names,
                used_runtime_helper_files: &mut *self.used_runtime_helper_files,
                exported_runtime_helper_bindings: &mut *self.exported_runtime_helper_bindings,
                required_runtime_helper_bindings: &mut *self.required_runtime_helper_bindings,
                used_runtime_helper_setters: &mut *self.used_runtime_helper_setters,
                used_lazy_module: &mut *self.used_lazy_module,
                used_lazy_value: &mut *self.used_lazy_value,
                exported_lazy_module: &mut *self.exported_lazy_module,
                exported_lazy_value: &mut *self.exported_lazy_value,
            });
            emit_lowered_runtime_helper_import(LoweredRuntimeHelperImportArgs {
                program: self.program,
                module_id: self.module.id,
                module_path: self.path,
                file: &mut *file,
                planned_bindings: &mut *planned_bindings,
                source_file_id: lowered_source.source_file_id,
                remaining_runtime_helpers: &remaining_runtime_helpers,
                written_runtime_helpers: &written_runtime_helpers,
                lazy_helper_names: &lazy_helper_names,
            });
        }

        emit_runtime_import_partitions(RuntimeImportPartitionEmitArgs {
            program: self.program,
            module_id: self.module.id,
            module_path: self.path,
            file: &mut *file,
            planned_bindings: &mut *planned_bindings,
            used_runtime_helper_files: &mut *self.used_runtime_helper_files,
            exported_runtime_helper_bindings: &mut *self.exported_runtime_helper_bindings,
            required_runtime_helper_bindings: &mut *self.required_runtime_helper_bindings,
            used_package_runtime_helper_files: &mut *self.used_package_runtime_helper_files,
            emitted_inline_runtime_helpers: &mut *emitted_inline_runtime_helpers,
            runtime_import_partitions,
            runtime_singleton_inlines: self.runtime_singleton_inlines,
            binding_owners: self.binding_owners,
            package_runtime_owner: self.package_runtime_owner,
        });

        if let Some(rewrite) = &node_builtin_require_rewrite
            && !rewrite.imports.is_empty()
        {
            emit_node_builtin_default_imports(file, planned_bindings, &rewrite.imports);
        }

        NormalRuntimePassOutput {
            lowered_source,
            source_imports,
            local_source_definitions,
            namespace_member_rewrite,
            localized_lazy_value_source,
            localized_noop_runtime_helpers,
            node_builtin_require_rewrite,
            node_builtin_require_helpers,
            written_runtime_helpers,
        }
    }
}

fn rewritable_externalized_package_init_shims_for_source(
    lowered_source: Option<&LoweredRuntimeModuleSource>,
    namespace_member_rewrite: Option<&RuntimeNamespaceMemberAccessRewrite>,
    externalized_packages: &BTreeSet<ModuleId>,
    externalized_package_init_bindings: &BTreeSet<BindingName>,
) -> BTreeSet<BindingName> {
    let Some(lowered_source) = lowered_source else {
        return BTreeSet::new();
    };
    if externalized_packages.is_empty() || externalized_package_init_bindings.is_empty() {
        return BTreeSet::new();
    }
    let source = namespace_member_rewrite
        .map(|rewrite| rewrite.source.as_str())
        .unwrap_or(lowered_source.source.as_str());
    let call_identifiers = crate::call_identifiers_in_source(source);
    let mut shims = externalized_package_init_bindings
        .iter()
        .filter(|binding| call_identifiers.contains(binding.as_str()))
        .cloned()
        .collect::<BTreeSet<_>>();
    if shims.is_empty() {
        return BTreeSet::new();
    }
    let original = shims.clone();
    let _rewritten = crate::erase_rewritable_package_init_shim_calls(source, &mut shims);
    original.difference(&shims).cloned().collect()
}

struct NormalModuleBodyPass<'a> {
    program: &'a EnrichedProgram,
    module_id: ModuleId,
    lowered_source: Option<&'a LoweredRuntimeModuleSource>,
    runtime_var_migrations: &'a RuntimeVarMigrationPlan,
    source_imports: &'a BTreeSet<BindingName>,
    local_source_definitions: &'a BTreeSet<BindingName>,
    namespace_member_rewrite: Option<&'a RuntimeNamespaceMemberAccessRewrite>,
    localized_lazy_value_source: Option<&'a str>,
    localized_noop_runtime_helpers: &'a BTreeSet<BindingName>,
    node_builtin_require_rewrite: Option<&'a NodeBuiltinRequireRewrite>,
    node_builtin_require_helpers: &'a BTreeSet<BindingName>,
    written_runtime_helpers: &'a BTreeSet<BindingName>,
    migration: &'a OwnerMigrationState,
}

impl NormalModuleBodyPass<'_> {
    fn run(
        &self,
        file: &mut PlannedFile,
        planned_bindings: &mut BTreeSet<BindingName>,
    ) -> Result<(), PlanError> {
        let source_definitions = self
            .program
            .model()
            .graph()
            .ast_definitions_for(self.module_id);
        emit_module_definition_bindings(
            self.program,
            self.module_id,
            file,
            planned_bindings,
            self.lowered_source,
        );

        emit_source_import_bindings(
            self.program,
            self.module_id,
            file,
            planned_bindings,
            self.source_imports,
        );

        if self.lowered_source.is_none() && !self.migration.migrated_local_bindings.is_empty() {
            self.emit_migrated_declarations(file, planned_bindings);
        }

        if let Some(lowered_source) = self.lowered_source {
            self.emit_lowered_source_body(
                lowered_source,
                &source_definitions,
                file,
                planned_bindings,
            )?;
        }
        Ok(())
    }

    fn emit_migrated_declarations(
        &self,
        file: &mut PlannedFile,
        planned_bindings: &mut BTreeSet<BindingName>,
    ) {
        add_migrated_local_binding_declarations(
            file,
            planned_bindings,
            &self.migration.migrated_local_bindings,
            &self.migration.migrated_locally,
            &self.migration.migrated_extra_namespace_bindings,
        );
        emit_migrated_locally_var_declarations(
            file,
            &self.migration.migrated_locally,
            self.runtime_var_migrations,
        );
        let retained_noop_deps = emit_migrated_extra_chunks(
            self.program,
            file,
            &self.migration.migrated_extra_snippets,
            &self.migration.migrated_extra_namespace_exports,
            &self.migration.migrated_extra_runtime_dep_aliases,
            &self.migration.migrated_extra_noop_deps,
            &self.migration.migrated_extra_runtime_setter_deps_by_source,
        );
        for binding in &retained_noop_deps {
            file.push_source(noop_function_statement(binding));
        }
    }

    fn emit_lowered_source_body(
        &self,
        lowered_source: &LoweredRuntimeModuleSource,
        source_definitions: &BTreeSet<BindingName>,
        file: &mut PlannedFile,
        planned_bindings: &mut BTreeSet<BindingName>,
    ) -> Result<(), PlanError> {
        let source_file_path = lowered_source.source_file_path.as_str();
        let source = build_lowered_module_source(
            lowered_source,
            self.localized_lazy_value_source,
            self.namespace_member_rewrite,
            self.localized_noop_runtime_helpers,
            self.node_builtin_require_rewrite,
            self.node_builtin_require_helpers,
            self.written_runtime_helpers,
        );
        if contains_call_to_identifier(source.as_str(), "require")
            && !self
                .local_source_definitions
                .contains(&BindingName::new("require"))
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
            file,
            planned_bindings,
            &self.migration.migrated_local_bindings,
            &self.migration.migrated_locally,
            &self.migration.migrated_extra_namespace_bindings,
        );
        let implicit_globals = implicit_global_declarations_for_module(
            source.as_str(),
            source_definitions,
            self.source_imports,
            planned_bindings,
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
            file,
            &self.migration.migrated_locally,
            self.runtime_var_migrations,
        );
        let retained_noop_deps = emit_migrated_extra_chunks(
            self.program,
            file,
            &self.migration.migrated_extra_snippets,
            &self.migration.migrated_extra_namespace_exports,
            &self.migration.migrated_extra_runtime_dep_aliases,
            &self.migration.migrated_extra_noop_deps,
            &self.migration.migrated_extra_runtime_setter_deps_by_source,
        );
        for binding in &retained_noop_deps {
            file.push_source(noop_function_statement(binding));
        }
        let normalized = normalize_source_for_emit(
            self.module_id,
            source_file_path,
            source.as_str(),
            file.source_strategy(),
        )?;
        file.push_source(normalized);
        Ok(())
    }
}

/// Build the layer-2 export-name map once per run: minified binding -> real
/// esbuild export name, from every `RuntimeNamespaceExport` table
/// (`__export(NS,{realName:()=>binding})`). A binding exported under more than one
/// real name maps to `None` (ambiguous reused minified name → dropped for soundness).
pub(crate) fn build_namespace_export_name_map(
    program: &EnrichedProgram,
) -> BTreeMap<String, Option<String>> {
    let mut map: BTreeMap<String, Option<String>> = BTreeMap::new();
    for prelude in program.model().graph().runtime_preludes().values() {
        for namespace_export in &prelude.namespace_exports {
            for (real_name, binding) in &namespace_export.exports {
                if real_name.as_str() == binding.as_str() || !is_plain_js_identifier(real_name) {
                    continue;
                }
                map.entry(binding.as_str().to_string())
                    .and_modify(|existing| {
                        if existing.as_deref() != Some(real_name.as_str()) {
                            *existing = None;
                        }
                    })
                    .or_insert_with(|| Some(real_name.to_string()));
            }
        }
    }
    map
}

/// Layer-2 export-name recovery (post-emission pass over EVERY planned file).
/// For each file, parse its actual local bindings and rename any that the bundle's
/// `__export` tables map to a real export name. Covers functions wherever they were
/// emitted — regular modules, the entrypoint island, runtime-helper files — because
/// it reads the file's real bindings, not graph definitions/migrations (the bulk of
/// exported functions land in the prelude-derived island, not in module definitions).
/// Sound + safe: ambiguous reused names are dropped (`None`); the rename machinery's
/// collision guard skips clashes; esbuild exports by namespace PROPERTY name so the
/// local binding rename never affects importers.
pub(crate) fn apply_export_name_renames(
    plan: &mut EmitPlan,
    export_names: &BTreeMap<String, Option<String>>,
) {
    if export_names.is_empty() {
        return;
    }
    for file in &mut plan.files {
        let body = file.body.join("\n");
        for binding in local_bindings_in_source(&body) {
            let Some(Some(real_name)) = export_names.get(&binding) else {
                continue;
            };
            if real_name != &binding {
                file.add_readability_rename(PlannedRename::new(
                    BindingName::new(binding),
                    BindingName::new(real_name.clone()),
                ));
            }
        }
    }
}

/// A plain JS identifier of length >= 2 (single-char names are too low-entropy to
/// be a meaningful recovered export name): starts letter/`_`/`$`, rest word chars.
fn is_plain_js_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    name.len() >= 2 && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// Export NAMES the recovered body already emits — the alias target of a local
/// export (`export { F as f }` → `f`), a bare local export (`export { st }` →
/// `st`), and re-export names (`export { x } from "…"` → `x`). Used to decide
/// which graph-declared exports the body does NOT carry and must be emitted
/// explicitly.
pub(crate) fn body_emitted_export_names(file: &PlannedFile) -> BTreeSet<BindingName> {
    let mut names = BTreeSet::new();
    for statement in file
        .body
        .iter()
        .flat_map(|chunk| source_statements(chunk.as_str()))
    {
        let specifiers = local_named_export_specifiers(statement)
            .or_else(|| named_reexport_specifiers(statement));
        for specifier in specifiers.into_iter().flatten() {
            names.insert(BindingName::new(specifier.exported));
        }
    }
    names
}

fn emit_normal_module_exports(
    program: &EnrichedProgram,
    module_id: ModuleId,
    source_module_wiring: &SourceModuleWiring,
    runtime_var_migrations: &RuntimeVarMigrationPlan,
    migrated_local_bindings: &BTreeSet<BindingName>,
    file: &mut PlannedFile,
) {
    // A graph-declared export is only "source-backed" if the recovered body
    // actually carries an `export` statement for that name. esbuild exposes
    // some cross-module bindings purely through scope hoisting (a sibling
    // module reads the hoisted internal name directly), never through the
    // module's own `__export` map — so the def-use extractor records the export
    // while the recovered body has no statement for it. Marking those
    // source-backed silently drops the export and dangles every consumer
    // import. Emit an explicit `export { … }` for the names the body omits.
    let body_export_names = body_emitted_export_names(file);
    let mut unbacked_graph_exports = BTreeSet::<BindingName>::new();
    for export in program
        .model()
        .graph()
        .import_export()
        .exports_for(module_id)
    {
        if body_export_names.contains(&export) {
            file.add_export_with_source_backed(export, true);
        } else if is_js_keyword(export.as_str()) {
            // A graph export whose name is a reserved word (e.g. `default`, from a
            // CommonJS `exports.default = …`) cannot be emitted as a bare named
            // export — `export { default };` is a syntax error (only
            // `export default …`, `x as default`, or `… from '…'` are legal). The
            // recovered body keeps the original `exports.default = …` assignment,
            // so skip synthesizing an invalid ESM re-export here (which would make
            // the whole module unparseable and force the emitter to retain the raw,
            // un-lowered body).
            continue;
        } else {
            unbacked_graph_exports.insert(export);
        }
    }
    if !unbacked_graph_exports.is_empty() {
        file.push_source(named_export_statement(unbacked_graph_exports.iter()));
        for export in unbacked_graph_exports {
            file.add_export_with_source_backed(export, true);
        }
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
            .exports_for(module_id)
            .into_iter()
            .chain(
                source_module_wiring
                    .exports_by_module
                    .get(&module_id)
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
    let migration_source_exports = runtime_var_migrations.source_dep_exports_for_module(module_id);
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
        module_id,
        [source_module_wiring.exports_by_module.get(&module_id)],
    );
    if !extra_exports.is_empty() {
        let existing_exports = program
            .model()
            .graph()
            .import_export()
            .exports_for(module_id)
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
}
