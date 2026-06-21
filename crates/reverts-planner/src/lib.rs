mod binding_owner;
mod byte_lexer;
mod class_field_bindings;
mod cli_entrypoint;
mod compiler_preservation;
mod complete_referenced_imports;
mod compute_modules;
mod dead_export_prune;
mod destructure_writes;
mod eager_safe_analysis;
mod external_package_adapter_emit;
mod identifiers;
mod import_coalesce;
mod module_planning_context;
mod package_runtime;
mod package_runtime_accumulator;
mod plan;
mod plan_error;
mod plan_reachability;
mod planner_context;
mod planner_pipeline;
mod pure_reexport_bypass;
mod relative_paths;
mod runtime_externalized_scan;
mod runtime_globals;
mod runtime_helper_emission;
mod runtime_helper_source_closure;
mod runtime_helper_strip;
mod runtime_helper_usage;
mod runtime_helper_writes;
mod runtime_literal_compaction;
mod runtime_namespace_rewrite;
mod runtime_plan_preparation;
mod runtime_setter_migration_blocker;
mod runtime_singleton_inline;
mod runtime_source_read;
mod runtime_source_scan;
mod runtime_var_migration;
mod source_module_facts;
mod source_surgery;
mod top_level_definitions;

use runtime_source_read::{
    RuntimeBindingReadProfile, RuntimeSourceReadIndex, runtime_binding_read_profile_diagnostic,
    runtime_readers_for_binding, runtime_source_read_index,
};
mod decompose_namespace;
mod delazify;
mod delazify_init_chains;
mod external_adapters;
mod helper_declarations;
mod identifier_facts;
mod lazy_initializer_parse;
mod lazy_wrapper_inline;
mod local_bindings;
mod migratable_reader_snippet;
mod module_dependency_index;
mod named_specifiers;
mod node_builtin_require;
mod noop_runtime_helpers;
mod pure_expression;
mod runtime_orphan_prune;
mod runtime_proxy_inline;
mod statement_parsers;
mod statements;

#[cfg(test)]
mod tests;

use decompose_namespace::{collect_member_access_only, decompose_function_namespace_objects};
use delazify::delazify_pure_value_bindings;
use external_adapters::{
    ExternalPackageAdapterPlan, adapter_owned_runtime_bindings, external_package_adapter_analysis,
    populate_external_package_adapter_file,
};
use helper_declarations::{lower_commonjs_wrapper_helper, lower_lazy_initializer_helper};

use runtime_source_scan::{
    call_identifiers_in_source, runtime_import_identifiers_in_source, value_identifiers_in_source,
};

use runtime_externalized_scan::{
    erase_rewritable_package_init_shim_calls, retain_runtime_imports_referenced_in_source,
    runtime_module_owner_imports_for_source, runtime_namespace_exports_for_helpers,
    scan_runtime_externalized_bindings, unresolved_runtime_helper_references,
};
use runtime_globals::is_runtime_global_identifier;

use top_level_definitions::{
    implicit_global_declarations_for_module, implicit_global_writes_in_source,
    top_level_definitions_in_source, variable_declaration_binding_starts,
};

use runtime_helper_source_closure::{
    ClosedRuntimeHelperSource, close_runtime_helper_source, close_runtime_helper_source_excluding,
    runtime_entrypoint, runtime_entrypoint_root_bindings, runtime_entrypoint_side_effects,
    runtime_prelude_snippet_is_noop, sanitize_identifier_fragment,
};

use migratable_reader_snippet::{
    class_declaration_names_binding, function_declaration_names_binding,
    is_migratable_namespace_reader_snippet, is_migratable_private_runtime_function_dependency,
    is_migratable_reader_function_snippet, variable_declaration_names_function_like_binding,
};

use module_dependency_index::{
    module_dependency_modules_by_owner, module_dependency_path_exists,
    package_ownership_proven_module_ids, source_suppressed_package_dependency_closure,
};

use named_specifiers::{
    local_named_export_specifiers, named_import_specifiers, named_reexport_specifiers,
    source_statements,
};

use local_bindings::local_bindings_in_source;

use runtime_literal_compaction::{
    coalesce_runtime_lazy_initializer_call_runs, compact_pure_static_runtime_literals,
};

use lazy_initializer_parse::{
    private_runtime_lazy_initializer_replacement, pure_lazy_initializer_replacement,
    pure_runtime_value_bindings, try_parse_runtime_lazy_initializer_declaration,
};

#[cfg(test)]
use identifier_facts::try_identifier_read_facts_in_source;
use identifier_facts::{
    IdentifierReadUsage, compact_js_source, identifier_occurrence_is_value_reference,
    identifier_read_facts_in_source, identifier_read_rename_site_is_safe,
    identifier_read_rename_sites_are_safe, previous_token_is_keyword,
    rename_identifier_reads_in_source,
};
use lazy_wrapper_inline::{
    inline_remaining_lazy_module_wrappers, inline_remaining_lazy_value_wrappers,
    inline_remaining_lazy_value_wrappers_allowing_assignments, localize_lazy_value_source,
};
use node_builtin_require::{
    NodeBuiltinRequireRewrite, rewrite_node_builtin_require_calls,
    rewrite_node_builtin_require_calls_with_imports, runtime_create_require_helpers,
};
use noop_runtime_helpers::{
    compact_bare_void_zero_expression_statements, drop_bare_void_zero_top_level_statements,
    localizable_noop_runtime_helpers, noop_runtime_helpers_in_source,
    rewrite_noop_runtime_helper_calls, source_reads_bindings_only_as_erasable_noop_calls,
    strip_runtime_noop_declarations,
};
use pure_expression::{
    is_pure_initializer_expression, looks_like_arrow_function_expression, pure_class_expression,
};
use runtime_orphan_prune::prune_orphan_runtime_bindings;
use runtime_proxy_inline::inline_single_use_runtime_proxy_functions;

use binding_owner::{BindingOwner, BindingOwnerPlan, RuntimeOwnerImportPartition};
use package_runtime::{
    PackageRuntimeHelperKey, PackageRuntimeHelperUsage, PackageRuntimeImportEmitter,
    PackageRuntimeOwner, emit_package_runtime_helper_import, partition_package_runtime_bindings,
};
use runtime_singleton_inline::{
    RuntimeSingletonInlineEmitContext, RuntimeSingletonInlinePlan,
    emit_runtime_singleton_inline_helpers, module_exported_bindings,
    partition_runtime_singleton_inline_bindings,
};
use runtime_var_migration::{
    RuntimeOwnedSnippetMigration, RuntimeVarMigrationPlan, compute_runtime_var_migration_plan,
};

use source_module_facts::SourceModuleFacts;
pub(crate) use source_surgery::{
    apply_text_edits, contains_top_level_initializer_operator, expand_line_removal_edits,
    previous_non_ws, top_level_statement_slices, top_level_statement_spans,
};

#[cfg(test)]
use destructure_writes::array_destructuring_assignment_writes;
use destructure_writes::{
    rewrite_array_destructuring_helper_writes, rewrite_object_destructuring_helper_writes,
    split_top_level_properties,
};
use eager_safe_analysis::{
    EagerSafeAnalysis, compute_eager_safe_analysis, consumer_eagerified_imports,
    rewrite_eagerified_call_sites, should_compute_cross_module_eager_safe_analysis,
};

use runtime_helper_strip::migratable_runtime_var_initializer;
#[cfg(test)]
use runtime_helper_writes::inline_internal_setter_calls;
use runtime_helper_writes::rewrite_runtime_helper_writes;

use pure_reexport_bypass::PureReexportBypassPlan;
use runtime_namespace_rewrite::rewrite_runtime_namespace_member_accesses;

use byte_lexer::{
    find_matching_brace, looks_like_regex_literal, skip_non_code_at, skip_quoted,
    skip_regex_literal, skip_ws,
};
use identifiers::{declaration_keyword_at_start, keyword_at, parse_identifier};
#[cfg(test)]
use import_coalesce::coalesce_top_level_import_declarations;
use import_coalesce::{
    first_local_for_import, import_statement_local_bindings, parse_runtime_prelude_direct_import,
};

pub use plan::{
    EmitPlan, PlannedBinding, PlannedExport, PlannedFile, PlannedImport, PlannedParamRename,
    PlannedRename, PlannedRenameScope, PlannedTypeAnnotation, ValidatedEmitPlan,
    ValidatedPlannedFile,
};
pub use plan_error::PlanError;
use relative_paths::relative_import_specifier;

pub use compiler_preservation::{
    CompilerPreservationAction, CompilerPreservationDecision, SourceCompilerStrategy,
};
pub use runtime_setter_migration_blocker::{
    RuntimeSetterMigrationBindingKey, RuntimeSetterMigrationBindingStatus,
    RuntimeSetterMigrationBlockerReason, RuntimeSetterMigrationBlockerReport,
};

#[cfg(test)]
use statements::runtime_helper_setter_declarations;
use statements::{
    default_import_statement, default_named_import_alias_statement, named_export_statement,
    named_import_alias_statement, named_import_statement, named_reexport_statement,
    namespace_import_statement, node_require_prelude_statement, noop_function_statement,
    runtime_helper_import_statement, runtime_helper_setter_name, runtime_helpers_path,
    runtime_lazy_helpers_path, runtime_namespace_export_statement, variable_declaration_statement,
};

use std::collections::{BTreeMap, BTreeSet};

use planner_context::{AnalysisReadyPass, PlannerContext, PlannerPass};
use reverts_graph::{
    RevertsGraph, RuntimeNamespaceExport, RuntimePrelude, RuntimePreludeBindingKind,
    RuntimePreludeImport,
};
use reverts_input::{ModuleDependencyTarget, ModuleInput};
use reverts_ir::{BindingName, BindingShape, ModuleId, ModuleKind};
#[cfg(test)]
use reverts_js::{ParseGoal, collect_top_level_statement_facts};
use reverts_js::{
    format_source_pretty, is_ascii_identifier_continue as is_identifier_continue,
    is_ascii_identifier_start as is_identifier_start, is_js_keyword, skip_block_comment,
    skip_line_comment,
};
use reverts_model::EnrichedProgram;
use reverts_package::accepted_external_module_ids;

/// Applies the planner's explicit source-text finalization pass to a generated
/// file before it is appended to an [`EmitPlan`].
///
/// Keeping this separate from [`EmitPlan::push_file`] preserves the boundary
/// between plan data structures and readability/boilerplate rewrite policy.
pub fn finalize_planned_file(file: &mut PlannedFile) {
    import_coalesce::finalize_planned_file(file);
}

/// Build a `PlannedBinding` whose `shape` and `known_members` are derived
/// from the enriched program. Centralising this keeps every planner site
/// that emits a real user binding consistent — the shape comes from the
/// solver, and `known_members` are attached only for `NamespaceObject`
/// (paper #7 downstream). Sites that synthesise runtime helpers bypass
/// this and use `PlannedBinding::new` directly with their known shape.
pub(crate) fn plan_binding_from_program(
    program: &EnrichedProgram,
    module_id: ModuleId,
    original: BindingName,
    emitted: BindingName,
    source_backed: bool,
    shape_override: Option<BindingShape>,
) -> PlannedBinding {
    // When delazify or namespace decomposition has reshaped a binding's
    // emitted RHS, the IR-derived shape (computed against the pre-lowering
    // lazy thunk) no longer matches reality — pass `Unknown` so the audit
    // doesn't fire on a stale "Callable" classification.
    let shape =
        shape_override.unwrap_or_else(|| program.binding_shape(module_id, original.as_str()));
    let known_members = if shape == BindingShape::NamespaceObject {
        program.known_members(module_id, original.as_str())
    } else {
        BTreeSet::new()
    };
    PlannedBinding::new(original, emitted, shape, source_backed).with_known_members(known_members)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ImportExportPlanner;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannerAnalysis {
    external_package_adapters: BTreeMap<ModuleId, ExternalPackageAdapterPlan>,
    externalized_packages: BTreeSet<ModuleId>,
    externalized_package_init_bindings: BTreeSet<BindingName>,
    source_suppressed_packages: BTreeSet<ModuleId>,
    source_module_wiring: SourceModuleWiring,
    lowered_runtime_sources: BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
    runtime_lazy_folds: RuntimeLazyFoldPlan,
}

impl PlannerAnalysis {
    fn from_program(program: &EnrichedProgram) -> Self {
        let source_facts = SourceModuleFacts::from_program(program);
        let accepted_externalized_packages =
            accepted_external_module_ids(&program.model().input().package_attributions);
        let adapter_analysis = external_package_adapter_analysis(
            program,
            &accepted_externalized_packages,
            &source_facts,
        );
        let external_package_adapters = adapter_analysis.adapters;
        let adapter_required_packages = adapter_analysis.adapter_required_packages;
        let externalized_packages = accepted_externalized_packages
            .difference(&adapter_required_packages)
            .copied()
            .collect::<BTreeSet<_>>();
        let adapter_backed_packages = external_package_adapters
            .keys()
            .copied()
            .collect::<BTreeSet<_>>();
        let source_preserved_packages = adapter_required_packages
            .difference(&adapter_backed_packages)
            .copied()
            .collect::<BTreeSet<_>>();
        let base_source_suppressed_packages = accepted_externalized_packages
            .difference(&source_preserved_packages)
            .copied()
            .collect::<BTreeSet<_>>();
        let ownership_proven_packages = package_ownership_proven_module_ids(program);
        let source_suppressed_packages = source_suppressed_package_dependency_closure(
            program,
            &base_source_suppressed_packages,
            &source_preserved_packages,
            &ownership_proven_packages,
        );
        let closure_externalized_packages = source_suppressed_packages
            .difference(&accepted_externalized_packages)
            .copied()
            .collect::<BTreeSet<_>>();
        let externalized_packages = externalized_packages
            .union(&closure_externalized_packages)
            .copied()
            .collect::<BTreeSet<_>>();
        let source_module_wiring = source_module_wiring(
            program,
            &externalized_packages,
            &external_package_adapters,
            &source_facts,
        );
        let externalized_package_init_bindings = source_facts
            .definition_modules_all
            .iter()
            .filter_map(|(binding, module_id)| {
                module_id
                    .filter(|module_id| externalized_packages.contains(module_id))
                    .map(|_| binding.clone())
            })
            .collect::<BTreeSet<_>>();
        let eager_safe_analysis = if should_compute_cross_module_eager_safe_analysis(program) {
            compute_eager_safe_analysis(program, &source_module_wiring)
        } else {
            EagerSafeAnalysis::default()
        };
        let lowered_runtime_sources = lowered_runtime_sources(
            program,
            &source_module_wiring,
            &eager_safe_analysis,
            &source_suppressed_packages,
        );
        let runtime_lazy_folds = runtime_lazy_fold_plan(
            program,
            &source_module_wiring,
            &lowered_runtime_sources,
            &externalized_packages,
        );

        Self {
            external_package_adapters,
            externalized_packages,
            externalized_package_init_bindings,
            source_suppressed_packages,
            source_module_wiring,
            lowered_runtime_sources,
            runtime_lazy_folds,
        }
    }
}

/// For a folded module, split `folded.stub_exports` into:
/// - bindings whose owner is in another module (these get re-exported
///   from the owner directly), and
/// - bindings whose owner is still the runtime helpers file (re-exported
///   from the runtime helpers).
pub(crate) fn partition_folded_stub_exports(
    folded: &RuntimeLazyFoldModule,
    module_id: ModuleId,
    binding_owners: &BindingOwnerPlan,
) -> (
    BTreeSet<BindingName>,
    BTreeMap<ModuleId, BTreeSet<BindingName>>,
) {
    let mut runtime_stub_exports = BTreeSet::<BindingName>::new();
    let mut direct_stub_exports = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
    for binding in &folded.stub_exports {
        if let Some(owner_module) = binding_owners.module_owner(folded.source_file_id, binding) {
            if owner_module != module_id {
                direct_stub_exports
                    .entry(owner_module)
                    .or_default()
                    .insert(binding.clone());
            }
        } else {
            runtime_stub_exports.insert(binding.clone());
        }
    }
    (runtime_stub_exports, direct_stub_exports)
}

/// For a folded module, restrict `folded.required_bindings` to the
/// subset still owned by either the runtime helpers file or this
/// module itself (i.e. excluding bindings whose owners have moved to
/// another module — those are imported directly from the new owner).
pub(crate) fn folded_runtime_required_bindings(
    folded: &RuntimeLazyFoldModule,
    module_id: ModuleId,
    binding_owners: &BindingOwnerPlan,
) -> BTreeSet<BindingName> {
    folded
        .required_bindings
        .iter()
        .filter(|binding| {
            binding_owners
                .module_owner(folded.source_file_id, binding)
                .is_none_or(|owner| owner == module_id)
        })
        .cloned()
        .collect()
}

/// Emit `export { X, Y } from runtime` for the subset of folded-module
/// stub exports still owned by the runtime helpers file, and record
/// those bindings in the helper-file-bindings + exports indexes so the
/// runtime helper emission phase knows to surface them.
pub(crate) fn emit_folded_runtime_stub_reexports(
    source_file_id: u32,
    path: &str,
    runtime_stub_exports: &BTreeSet<BindingName>,
    file: &mut PlannedFile,
    used_runtime_helper_files: &mut BTreeMap<u32, BTreeSet<BindingName>>,
    exported_runtime_helper_bindings: &mut BTreeMap<u32, BTreeSet<BindingName>>,
) {
    if runtime_stub_exports.is_empty() {
        return;
    }
    used_runtime_helper_files
        .entry(source_file_id)
        .or_default()
        .extend(runtime_stub_exports.iter().cloned());
    exported_runtime_helper_bindings
        .entry(source_file_id)
        .or_default()
        .extend(runtime_stub_exports.iter().cloned());
    let specifier = relative_import_specifier(path, runtime_helpers_path(source_file_id).as_str());
    file.push_source(named_reexport_statement(
        runtime_stub_exports.iter(),
        specifier.as_str(),
    ));
}

/// Argument bundle for source-module import emission.
pub(crate) struct SourceModuleImportEmitArgs<'a> {
    pub(crate) program: &'a EnrichedProgram,
    pub(crate) module_id: ModuleId,
    pub(crate) path: &'a str,
    pub(crate) source_module_wiring: &'a SourceModuleWiring,
    pub(crate) pure_reexport_bypasses: &'a PureReexportBypassPlan,
    pub(crate) runtime_lazy_folds: &'a RuntimeLazyFoldPlan,
    pub(crate) omitted_folded_stub_modules: &'a BTreeSet<ModuleId>,
    pub(crate) binding_owners: &'a BindingOwnerPlan,
    pub(crate) file: &'a mut PlannedFile,
    pub(crate) planned_bindings: &'a mut BTreeSet<BindingName>,
    pub(crate) used_runtime_helper_files: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) exported_runtime_helper_bindings: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
}

/// Argument bundle for localized no-op runtime-helper analysis.
pub(crate) struct LocalizedNoopRuntimeHelperArgs<'a> {
    pub(crate) program: &'a EnrichedProgram,
    pub(crate) module_id: ModuleId,
    pub(crate) source_module_wiring: &'a SourceModuleWiring,
    pub(crate) lowered_source: Option<&'a LoweredRuntimeModuleSource>,
    pub(crate) namespace_member_rewrite:
        Option<&'a runtime_namespace_rewrite::RuntimeNamespaceMemberAccessRewrite>,
    pub(crate) node_builtin_require_rewrite: Option<&'a NodeBuiltinRequireRewrite>,
    pub(crate) remaining_runtime_helpers: &'a BTreeSet<BindingName>,
}

/// Argument bundle for runtime-import partition analysis.
pub(crate) struct RuntimeImportPartitionArgs<'a> {
    pub(crate) program: &'a EnrichedProgram,
    pub(crate) module_id: ModuleId,
    pub(crate) runtime_import_groups: BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) binding_owners: &'a BindingOwnerPlan,
    pub(crate) namespace_member_rewrite:
        Option<&'a runtime_namespace_rewrite::RuntimeNamespaceMemberAccessRewrite>,
    pub(crate) source_runtime_refs: &'a BTreeSet<BindingName>,
    pub(crate) lowered_helpers: &'a BTreeSet<BindingName>,
    pub(crate) written_runtime_helpers: &'a BTreeSet<BindingName>,
    pub(crate) consumed_node_builtin_require_helpers: &'a BTreeSet<BindingName>,
    pub(crate) localized_noop_runtime_helpers: &'a BTreeSet<BindingName>,
    pub(crate) remaining_runtime_helpers: &'a BTreeSet<BindingName>,
    pub(crate) planned_bindings: &'a BTreeSet<BindingName>,
    pub(crate) local_source_definitions: &'a BTreeSet<BindingName>,
    pub(crate) local_source_writes: &'a BTreeSet<BindingName>,
}

/// Argument bundle for first-pass lazyValue localization.
pub(crate) struct LazyValueLocalizationArgs<'a> {
    pub(crate) lowered_source: Option<&'a LoweredRuntimeModuleSource>,
    pub(crate) namespace_member_rewrite:
        Option<&'a runtime_namespace_rewrite::RuntimeNamespaceMemberAccessRewrite>,
    pub(crate) has_runtime_edge_before_lazy_helpers: bool,
    pub(crate) written_runtime_helpers: &'a BTreeSet<BindingName>,
    pub(crate) has_runtime_group_imports: bool,
    pub(crate) runtime_singleton_inlines: &'a RuntimeSingletonInlinePlan,
    pub(crate) module_id: ModuleId,
    pub(crate) lowered_runtime_bindings: &'a BTreeSet<BindingName>,
}

/// Argument bundle for package-runtime import emission from lowered sources.
pub(crate) struct LoweredPackageRuntimeImportArgs<'a> {
    pub(crate) program: &'a EnrichedProgram,
    pub(crate) module_id: ModuleId,
    pub(crate) module_path: &'a str,
    pub(crate) file: &'a mut PlannedFile,
    pub(crate) planned_bindings: &'a mut BTreeSet<BindingName>,
    pub(crate) used_package_runtime_helper_files:
        &'a mut BTreeMap<PackageRuntimeHelperKey, PackageRuntimeHelperUsage>,
    pub(crate) source_file_id: u32,
    pub(crate) binding_owners: &'a BindingOwnerPlan,
    pub(crate) package_runtime_owner: Option<&'a PackageRuntimeOwner>,
    pub(crate) remaining_runtime_helpers: &'a BTreeSet<BindingName>,
    pub(crate) written_runtime_helpers: &'a BTreeSet<BindingName>,
}

/// Argument bundle for second-pass lazyValue localization.
pub(crate) struct PostInlineLazyValueLocalizationArgs<'a> {
    pub(crate) lowered_source: &'a LoweredRuntimeModuleSource,
    pub(crate) namespace_member_rewrite:
        Option<&'a runtime_namespace_rewrite::RuntimeNamespaceMemberAccessRewrite>,
    pub(crate) already_localized: Option<&'a String>,
    pub(crate) has_runtime_edge_before_lazy_helpers: bool,
    pub(crate) remaining_runtime_helpers: &'a BTreeSet<BindingName>,
    pub(crate) written_runtime_helpers: &'a BTreeSet<BindingName>,
    pub(crate) package_remaining_helpers: &'a BTreeSet<BindingName>,
    pub(crate) package_written_helpers: &'a BTreeSet<BindingName>,
    pub(crate) lazy_helper_names: &'a [&'static str],
    pub(crate) runtime_import_partitions: &'a [(u32, RuntimeOwnerImportPartition)],
    pub(crate) runtime_singleton_inlines: &'a RuntimeSingletonInlinePlan,
    pub(crate) binding_owners: &'a BindingOwnerPlan,
    pub(crate) package_runtime_owner: Option<&'a PackageRuntimeOwner>,
    pub(crate) module_id: ModuleId,
}

/// Argument bundle for runtime-import partition emission.
pub(crate) struct RuntimeImportPartitionEmitArgs<'a> {
    pub(crate) program: &'a EnrichedProgram,
    pub(crate) module_id: ModuleId,
    pub(crate) module_path: &'a str,
    pub(crate) file: &'a mut PlannedFile,
    pub(crate) planned_bindings: &'a mut BTreeSet<BindingName>,
    pub(crate) used_runtime_helper_files: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) exported_runtime_helper_bindings: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) required_runtime_helper_bindings: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) used_package_runtime_helper_files:
        &'a mut BTreeMap<PackageRuntimeHelperKey, PackageRuntimeHelperUsage>,
    pub(crate) emitted_inline_runtime_helpers: &'a mut BTreeSet<(u32, BindingName)>,
    pub(crate) runtime_import_partitions: Vec<(u32, RuntimeOwnerImportPartition)>,
    pub(crate) runtime_singleton_inlines: &'a RuntimeSingletonInlinePlan,
    pub(crate) binding_owners: &'a BindingOwnerPlan,
    pub(crate) package_runtime_owner: Option<&'a PackageRuntimeOwner>,
}

/// Argument bundle for lowered runtime-helper import emission.
pub(crate) struct LoweredRuntimeHelperImportArgs<'a> {
    pub(crate) program: &'a EnrichedProgram,
    pub(crate) module_id: ModuleId,
    pub(crate) module_path: &'a str,
    pub(crate) file: &'a mut PlannedFile,
    pub(crate) planned_bindings: &'a mut BTreeSet<BindingName>,
    pub(crate) source_file_id: u32,
    pub(crate) remaining_runtime_helpers: &'a BTreeSet<BindingName>,
    pub(crate) written_runtime_helpers: &'a BTreeSet<BindingName>,
    pub(crate) lazy_helper_names: &'a [&'static str],
}

/// Argument bundle for lowered runtime-helper usage recording.
pub(crate) struct LoweredRuntimeHelperUsageArgs<'a> {
    pub(crate) lowered_source: &'a LoweredRuntimeModuleSource,
    pub(crate) remaining_runtime_helpers: &'a BTreeSet<BindingName>,
    pub(crate) written_runtime_helpers: &'a BTreeSet<BindingName>,
    pub(crate) lazy_helper_names: &'a [&'static str],
    pub(crate) used_runtime_helper_files: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) exported_runtime_helper_bindings: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) required_runtime_helper_bindings: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) used_runtime_helper_setters: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) used_lazy_module: &'a mut BTreeSet<u32>,
    pub(crate) used_lazy_value: &'a mut BTreeSet<u32>,
    pub(crate) exported_lazy_module: &'a mut BTreeSet<u32>,
    pub(crate) exported_lazy_value: &'a mut BTreeSet<u32>,
}

/// Argument bundle for migrated runtime-extra alias import emission.
pub(crate) struct MigratedRuntimeExtraAliasImportArgs<'a> {
    pub(crate) module_path: &'a str,
    pub(crate) file: &'a mut PlannedFile,
    pub(crate) planned_bindings: &'a mut BTreeSet<BindingName>,
    pub(crate) used_runtime_helper_files: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) exported_runtime_helper_bindings: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) required_runtime_helper_bindings: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) migrated_runtime_extra_runtime_dep_aliases:
        &'a BTreeMap<u32, BTreeMap<BindingName, BindingName>>,
}

/// Argument bundle for migrated runtime re-export import emission.
pub(crate) struct MigratedExtraRuntimeReexportImportArgs<'a> {
    pub(crate) program: &'a EnrichedProgram,
    pub(crate) module_id: ModuleId,
    pub(crate) module_path: &'a str,
    pub(crate) file: &'a mut PlannedFile,
    pub(crate) planned_bindings: &'a mut BTreeSet<BindingName>,
    pub(crate) used_runtime_helper_files: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) exported_runtime_helper_bindings: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) migrated_extra_runtime_reexport_deps: &'a BTreeMap<u32, BTreeSet<BindingName>>,
}

/// Argument bundle for migrated owner import emission.
pub(crate) struct MigratedExtraOwnerImportArgs<'a> {
    pub(crate) program: &'a EnrichedProgram,
    pub(crate) module_id: ModuleId,
    pub(crate) module_path: &'a str,
    pub(crate) file: &'a mut PlannedFile,
    pub(crate) planned_bindings: &'a mut BTreeSet<BindingName>,
    pub(crate) migrated_extra_source_deps: &'a BTreeMap<ModuleId, BTreeSet<BindingName>>,
    pub(crate) migrated_extra_runtime_owner_deps: &'a BTreeMap<ModuleId, BTreeSet<BindingName>>,
    pub(crate) migrated_extra_runtime_owner_dep_aliases:
        &'a BTreeMap<ModuleId, BTreeMap<BindingName, BindingName>>,
}

/// Argument bundle for write-rewrite remaining-helper filtering.
pub(crate) struct RemainingHelpersWriteRewriteArgs<'a> {
    pub(crate) program: &'a EnrichedProgram,
    pub(crate) module_id: ModuleId,
    pub(crate) source_module_wiring: &'a SourceModuleWiring,
    pub(crate) lowered_source: Option<&'a LoweredRuntimeModuleSource>,
    pub(crate) namespace_member_rewrite:
        Option<&'a runtime_namespace_rewrite::RuntimeNamespaceMemberAccessRewrite>,
    pub(crate) node_builtin_require_rewrite: Option<&'a NodeBuiltinRequireRewrite>,
    pub(crate) written_runtime_helpers: &'a BTreeSet<BindingName>,
    pub(crate) migrated_extra_runtime_deps: &'a BTreeSet<BindingName>,
    pub(crate) remaining_runtime_helpers: BTreeSet<BindingName>,
}

/// Argument bundle for folded-stub migrated export emission.
pub(crate) struct FoldedNoopAndMigratedExportsArgs<'a> {
    pub(crate) folded: &'a RuntimeLazyFoldModule,
    pub(crate) runtime_stub_exports: &'a BTreeSet<BindingName>,
    pub(crate) direct_stub_exports: &'a BTreeMap<ModuleId, BTreeSet<BindingName>>,
    pub(crate) migrated_extra_noop_deps: &'a BTreeSet<BindingName>,
    pub(crate) migrated_local_bindings: &'a BTreeSet<BindingName>,
    pub(crate) migrated_extra_namespace_bindings: &'a BTreeSet<BindingName>,
    pub(crate) file: &'a mut PlannedFile,
    pub(crate) planned_bindings: &'a mut BTreeSet<BindingName>,
}

/// Argument bundle for runtime-extra dependency import emission.
pub(crate) struct RuntimeExtraDepsImportArgs<'a> {
    pub(crate) program: &'a EnrichedProgram,
    pub(crate) module_id: ModuleId,
    pub(crate) path: &'a str,
    pub(crate) deps_by_source: &'a BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) file: &'a mut PlannedFile,
    pub(crate) planned_bindings: &'a mut BTreeSet<BindingName>,
    pub(crate) used_runtime_helper_files: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) exported_runtime_helper_bindings: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
    pub(crate) required_runtime_helper_bindings: &'a mut BTreeMap<u32, BTreeSet<BindingName>>,
}

/// Emit the consumer-side `import { … } from './target.ts'` /
/// `import { … } from runtime` lines for every binding this module
/// reads from another source module (per
/// `source_module_wiring.imports_by_module`). The path varies by
/// target kind:
///
/// - if a pure re-export barrel routes the target, the `redirects`
///   table sends each binding to its real owner;
/// - if the target is a folded module whose stub got omitted, the
///   binding's `BindingOwner::Module` re-points to the new owner
///   directly, while runtime-routed bindings fall back to the
///   runtime helper file;
/// - everything else gets the obvious `import { … } from '<target>'`.
///
/// Returns `true` if at least one binding routed through a folded
/// module's runtime helper file — the caller uses that to decide
/// whether the lazy helper imports must precede the rest.
pub(crate) fn emit_source_module_imports(args: SourceModuleImportEmitArgs<'_>) -> bool {
    let SourceModuleImportEmitArgs {
        program,
        module_id,
        path,
        source_module_wiring,
        pure_reexport_bypasses,
        runtime_lazy_folds,
        omitted_folded_stub_modules,
        binding_owners,
        file,
        planned_bindings,
        used_runtime_helper_files,
        exported_runtime_helper_bindings,
    } = args;
    let mut has_runtime_edge_before_lazy_helpers = false;
    let Some(module_imports) = source_module_wiring.imports_by_module.get(&module_id) else {
        return has_runtime_edge_before_lazy_helpers;
    };
    for (target_module_id, bindings) in module_imports {
        let mut bindings_by_target = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
        if let Some(redirects) = pure_reexport_bypasses.redirects.get(target_module_id) {
            for binding in bindings {
                let effective_target = redirects.get(binding).copied().unwrap_or(*target_module_id);
                bindings_by_target
                    .entry(effective_target)
                    .or_default()
                    .insert(binding.clone());
            }
        } else {
            bindings_by_target.insert(*target_module_id, bindings.clone());
        }
        for (effective_target_module_id, effective_bindings) in bindings_by_target {
            if effective_target_module_id == module_id {
                continue;
            }
            if let Some(folded) = runtime_lazy_folds.modules.get(&effective_target_module_id)
                && omitted_folded_stub_modules.contains(&effective_target_module_id)
            {
                let mut runtime_bindings = BTreeSet::<BindingName>::new();
                let mut direct_bindings = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
                for binding in effective_bindings {
                    if let Some(owner_module) =
                        binding_owners.module_owner(folded.source_file_id, &binding)
                        && owner_module != module_id
                    {
                        direct_bindings
                            .entry(owner_module)
                            .or_default()
                            .insert(binding.clone());
                    } else {
                        runtime_bindings.insert(binding.clone());
                    }
                }
                emit_direct_owner_imports(
                    program,
                    module_id,
                    path,
                    file,
                    planned_bindings,
                    &direct_bindings,
                );
                if runtime_bindings.is_empty() {
                    continue;
                }
                has_runtime_edge_before_lazy_helpers = true;
                used_runtime_helper_files
                    .entry(folded.source_file_id)
                    .or_default()
                    .extend(runtime_bindings.iter().cloned());
                exported_runtime_helper_bindings
                    .entry(folded.source_file_id)
                    .or_default()
                    .extend(runtime_bindings.iter().cloned());
                let specifier = relative_import_specifier(
                    path,
                    runtime_helpers_path(folded.source_file_id).as_str(),
                );
                file.push_source(named_import_statement(
                    runtime_bindings.iter(),
                    specifier.as_str(),
                ));
                for binding in runtime_bindings {
                    planned_bindings.insert(binding.clone());
                    file.add_binding(plan_binding_from_program(
                        program,
                        module_id,
                        binding.clone(),
                        binding,
                        true,
                        None,
                    ));
                }
                continue;
            }
            let Some(target_path) = module_output_path(program, effective_target_module_id) else {
                continue;
            };
            let specifier = relative_import_specifier(path, target_path.as_str());
            file.push_source(named_import_statement(
                effective_bindings.iter(),
                specifier.as_str(),
            ));
            for binding in effective_bindings {
                planned_bindings.insert(binding.clone());
                file.add_binding(plan_binding_from_program(
                    program,
                    module_id,
                    binding.clone(),
                    binding,
                    true,
                    None,
                ));
            }
        }
    }
    has_runtime_edge_before_lazy_helpers
}

/// Compute the set of runtime no-op helpers that can be localised
/// (declared inline in the consuming module) because the prelude
/// shows them safe to copy and the rewritten source still references
/// them.
pub(crate) fn compute_localized_noop_runtime_helpers(
    args: LocalizedNoopRuntimeHelperArgs<'_>,
) -> BTreeSet<BindingName> {
    let LocalizedNoopRuntimeHelperArgs {
        program,
        module_id,
        source_module_wiring,
        lowered_source,
        namespace_member_rewrite,
        node_builtin_require_rewrite,
        remaining_runtime_helpers,
    } = args;
    let Some(source) = lowered_source else {
        return BTreeSet::new();
    };
    let Some(prelude) = program
        .model()
        .graph()
        .runtime_prelude(source.source_file_id)
    else {
        return BTreeSet::new();
    };
    let source_text = node_builtin_require_rewrite
        .map(|rewrite| rewrite.source.as_str())
        .unwrap_or_else(|| {
            namespace_member_rewrite
                .map(|rewrite| rewrite.source.as_str())
                .unwrap_or(source.source.as_str())
        });
    let exported_bindings = module_exported_bindings(
        program,
        module_id,
        source_module_wiring.exports_by_module.get(&module_id),
        source_text,
    );
    localizable_noop_runtime_helpers(
        prelude,
        source_text,
        remaining_runtime_helpers,
        &exported_bindings,
        source.uses_lazy_value,
    )
}

/// Strip helpers already accounted for by namespace-member or
/// node-builtin-`require` rewrites from the remaining-helpers set.
/// The namespace rewrite drops getter-bound helpers when their
/// namespace is collapsed in this source; the require rewrite consumes
/// helper bindings whose only use site has been folded into an
/// equivalent CommonJS `require(...)` form. Returns the filtered set
/// plus the consumed-by-require helper set (still needed by a later
/// partitioning filter on a different binding stream).
pub(crate) fn filter_remaining_helpers_namespace_and_require(
    remaining_runtime_helpers: BTreeSet<BindingName>,
    lowered_source: Option<&LoweredRuntimeModuleSource>,
    namespace_member_rewrite: Option<
        &runtime_namespace_rewrite::RuntimeNamespaceMemberAccessRewrite,
    >,
    node_builtin_require_rewrite: Option<&NodeBuiltinRequireRewrite>,
) -> (BTreeSet<BindingName>, BTreeSet<BindingName>) {
    let dropped_runtime_namespaces_for_source = lowered_source
        .and_then(|source| {
            namespace_member_rewrite.and_then(|rewrite| {
                rewrite
                    .dropped_namespaces_by_source
                    .get(&source.source_file_id)
            })
        })
        .cloned()
        .unwrap_or_default();
    let consumed_node_builtin_require_helpers = node_builtin_require_rewrite
        .map(|rewrite| rewrite.consumed_helpers.clone())
        .unwrap_or_default();
    let filtered = remaining_runtime_helpers
        .into_iter()
        .filter(|binding| !dropped_runtime_namespaces_for_source.contains(binding))
        .filter(|binding| !consumed_node_builtin_require_helpers.contains(binding))
        .collect();
    (filtered, consumed_node_builtin_require_helpers)
}

/// Build the per-source-file runtime-import partitions for a module
/// from the raw runtime-import groups: each group's binding stream
/// has dropped/already-emitted helpers filtered out, namespace-member
/// imports folded in, and the result split into runtime-helper vs.
/// direct-import buckets via `partition_runtime_owner_bindings`.
/// Empty partitions are dropped.
pub(crate) fn build_runtime_import_partitions(
    args: RuntimeImportPartitionArgs<'_>,
) -> Vec<(u32, RuntimeOwnerImportPartition)> {
    let RuntimeImportPartitionArgs {
        program,
        module_id,
        runtime_import_groups,
        binding_owners,
        namespace_member_rewrite,
        source_runtime_refs,
        lowered_helpers,
        written_runtime_helpers,
        consumed_node_builtin_require_helpers,
        localized_noop_runtime_helpers,
        remaining_runtime_helpers,
        planned_bindings,
        local_source_definitions,
        local_source_writes,
    } = args;
    let mut runtime_import_partitions = Vec::<(u32, RuntimeOwnerImportPartition)>::new();
    for (source_file_id, bindings) in runtime_import_groups {
        let dropped_runtime_namespaces = namespace_member_rewrite
            .and_then(|rewrite| rewrite.dropped_namespaces_by_source.get(&source_file_id))
            .cloned()
            .unwrap_or_default();
        let namespace_member_imports = namespace_member_rewrite
            .and_then(|rewrite| rewrite.imports_by_source.get(&source_file_id))
            .cloned()
            .unwrap_or_default();
        let namespace_export_helpers = program
            .model()
            .graph()
            .runtime_prelude(source_file_id)
            .map(|prelude| {
                prelude
                    .namespace_exports
                    .iter()
                    .map(|export| export.helper.clone())
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        let bindings = bindings
            .into_iter()
            .filter(|binding| !dropped_runtime_namespaces.contains(binding))
            .chain(namespace_member_imports.into_iter())
            // Namespace helper calls such as `__export(ns, {...})`
            // are planner-lowered to `Object.defineProperties(...)`
            // when the namespace setup is emitted. If graph recovery
            // attributed that prelude-only call to a source module,
            // do not keep a synthetic runtime import/export edge for
            // the helper unless the module body itself still names it.
            .filter(|binding| {
                !namespace_export_helpers.contains(binding) || source_runtime_refs.contains(binding)
            })
            .filter(|binding| !lowered_helpers.contains(binding))
            // Runtime imports recorded by the graph can include the
            // LHS of cross-module writes. After we rewrite those
            // writes through `__reverts_set_X(...)`, a write-only `X`
            // must not stay in the value-import/public-export
            // surface. Bindings that are both written and read in the
            // lowered source are handled by `remaining_runtime_helpers`
            // above, so this only removes setter-only leftovers.
            .filter(|binding| !written_runtime_helpers.contains(binding))
            .filter(|binding| !consumed_node_builtin_require_helpers.contains(binding))
            .filter(|binding| !localized_noop_runtime_helpers.contains(binding))
            .filter(|binding| !remaining_runtime_helpers.contains(binding))
            .filter(|binding| !planned_bindings.contains(binding))
            .filter(|binding| !local_source_definitions.contains(binding))
            .filter(|binding| !local_source_writes.contains(binding))
            .collect::<BTreeSet<_>>();
        if bindings.is_empty() {
            continue;
        }
        let import_partition =
            partition_runtime_owner_bindings(binding_owners, source_file_id, module_id, bindings);
        if !import_partition.runtime_bindings.is_empty()
            || !import_partition.direct_imports.is_empty()
        {
            runtime_import_partitions.push((source_file_id, import_partition));
        }
    }
    runtime_import_partitions
}

/// When a lowered source only uses `lazyValue` (not `lazyModule`) and
/// has no remaining runtime-helper imports, no written runtime
/// helpers, and no other runtime group imports, try to inline a tiny
/// local `lazyValue` shim instead of importing it. Returns the
/// localized rewritten source on success; `None` if any precondition
/// fails or the rewrite isn't applicable.
pub(crate) fn try_localize_lazy_value(args: LazyValueLocalizationArgs<'_>) -> Option<String> {
    let LazyValueLocalizationArgs {
        lowered_source,
        namespace_member_rewrite,
        has_runtime_edge_before_lazy_helpers,
        written_runtime_helpers,
        has_runtime_group_imports,
        runtime_singleton_inlines,
        module_id,
        lowered_runtime_bindings,
    } = args;
    let lowered_source = lowered_source?;
    if !lowered_source.uses_lazy_value
        || lowered_source.uses_lazy_module
        || has_runtime_edge_before_lazy_helpers
        || !written_runtime_helpers.is_empty()
        || has_runtime_group_imports
    {
        return None;
    }
    let source_for_lazy = namespace_member_rewrite
        .map(|rewrite| rewrite.source.as_str())
        .unwrap_or(lowered_source.source.as_str());
    let (inlineable_lazy_runtime_deps, _runtime_deps_after_inline) =
        partition_runtime_singleton_inline_bindings(
            runtime_singleton_inlines,
            module_id,
            lowered_source.source_file_id,
            lowered_runtime_bindings,
        );
    if !inlineable_lazy_runtime_deps.is_empty()
        || !owner_runtime_imports_are_lazy_safe(source_for_lazy, lowered_runtime_bindings)
    {
        return None;
    }
    localize_lazy_value_source(source_for_lazy)
}

/// Collect the source files whose runtime-helper companion file this
/// module will still need to import — either because not every helper
/// could be inlined as a singleton snippet, or because written/lazy
/// helpers always go through the runtime file. Used downstream to
/// route prelude imports through the runtime companion.
pub(crate) fn compute_runtime_sources_for_module(
    lowered_source: Option<&LoweredRuntimeModuleSource>,
    lowered_import_partition: &RuntimeOwnerImportPartition,
    runtime_import_partitions: &[(u32, RuntimeOwnerImportPartition)],
    runtime_singleton_inlines: &RuntimeSingletonInlinePlan,
    module_id: ModuleId,
    written_runtime_helpers: &BTreeSet<BindingName>,
    lazy_helper_names: &[&'static str],
) -> BTreeSet<u32> {
    let mut runtime_sources_for_module = BTreeSet::<u32>::new();
    if let Some(lowered_source) = lowered_source
        && (!partition_runtime_singleton_inline_bindings(
            runtime_singleton_inlines,
            module_id,
            lowered_source.source_file_id,
            &lowered_import_partition.runtime_bindings,
        )
        .1
        .is_empty()
            || !written_runtime_helpers.is_empty()
            || !lazy_helper_names.is_empty())
    {
        runtime_sources_for_module.insert(lowered_source.source_file_id);
    }
    for (source_file_id, partition) in runtime_import_partitions {
        if !partition_runtime_singleton_inline_bindings(
            runtime_singleton_inlines,
            module_id,
            *source_file_id,
            &partition.runtime_bindings,
        )
        .1
        .is_empty()
        {
            runtime_sources_for_module.insert(*source_file_id);
        }
    }
    runtime_sources_for_module
}

/// Apply the planner-owned source rewrites to the lowered source for
/// this module — namespace-member access folding, lazyValue
/// localisation, noop runtime-helper call removal (plus dropping the
/// bare `void 0;` statements they often leave behind), node-builtin
/// `require(...)` reshape, and runtime-helper writes routed through
/// `__reverts_set_X(...)`. The result is the source body the emitter
/// will normalise and write to disk.
pub(crate) fn build_lowered_module_source(
    lowered_source: &LoweredRuntimeModuleSource,
    localized_lazy_value_source: Option<&str>,
    namespace_member_rewrite: Option<
        &runtime_namespace_rewrite::RuntimeNamespaceMemberAccessRewrite,
    >,
    localized_noop_runtime_helpers: &BTreeSet<BindingName>,
    node_builtin_require_rewrite: Option<&NodeBuiltinRequireRewrite>,
    node_builtin_require_helpers: &BTreeSet<BindingName>,
    written_runtime_helpers: &BTreeSet<BindingName>,
) -> String {
    let mut source = localized_lazy_value_source
        .map(str::to_owned)
        .unwrap_or_else(|| {
            namespace_member_rewrite
                .map(|rewrite| rewrite.source.clone())
                .unwrap_or_else(|| lowered_source.source.clone())
        });
    if !localized_noop_runtime_helpers.is_empty() {
        source = rewrite_noop_runtime_helper_calls(source.as_str(), localized_noop_runtime_helpers);
        source = drop_bare_void_zero_top_level_statements(source.as_str());
        source = compact_bare_void_zero_expression_statements(source.as_str());
    }
    if let Some(rewrite) = node_builtin_require_rewrite
        && !rewrite.imports.is_empty()
    {
        source = rewrite_node_builtin_require_calls_with_imports(
            source.as_str(),
            node_builtin_require_helpers,
            &rewrite.imports,
        );
    }
    if !written_runtime_helpers.is_empty() {
        source = rewrite_runtime_helper_writes(source.as_str(), written_runtime_helpers);
    }
    source
}

/// Emit `var` declarations for bindings migrated locally into this
/// module: one bundled `var X, Y, Z;` for the uninitialized
/// migrations and a separate `var X = init;` for each that carries an
/// initializer. Initialized bindings are emitted as individual
/// statements (not bundled) so the initializers stay readable.
pub(crate) fn emit_migrated_locally_var_declarations(
    file: &mut PlannedFile,
    migrated_locally: &BTreeSet<BindingName>,
    runtime_var_migrations: &RuntimeVarMigrationPlan,
) {
    if migrated_locally.is_empty() {
        return;
    }
    let bare: BTreeSet<&BindingName> = migrated_locally
        .iter()
        .filter(|binding| {
            runtime_var_migrations
                .migrations_by_binding
                .get(*binding)
                .and_then(|m| m.initializer.as_deref())
                .is_none()
        })
        .collect();
    if !bare.is_empty() {
        file.push_source(variable_declaration_statement(bare.into_iter()));
    }
    for binding in migrated_locally {
        let Some(migration) = runtime_var_migrations.migrations_by_binding.get(binding) else {
            continue;
        };
        let Some(initializer) = migration.initializer.as_deref() else {
            continue;
        };
        file.push_source(format!(
            "var {name} = {initializer};",
            name = binding.as_str()
        ));
    }
}

/// Register each migrated-local binding as planned and declare it on
/// the file with the appropriate shape: bindings that were migrated
/// to this module (either as locals or as namespace members) get
/// `BindingShape::Unknown` because the lowered form may reshape them;
/// other migrated bindings stay `Callable`. Called from both the
/// source-free and source-backed paths.
pub(crate) fn add_migrated_local_binding_declarations(
    file: &mut PlannedFile,
    planned_bindings: &mut BTreeSet<BindingName>,
    migrated_local_bindings: &BTreeSet<BindingName>,
    migrated_locally: &BTreeSet<BindingName>,
    migrated_extra_namespace_bindings: &BTreeSet<BindingName>,
) {
    for binding in migrated_local_bindings {
        planned_bindings.insert(binding.clone());
        let binding_shape = if migrated_locally.contains(binding)
            || migrated_extra_namespace_bindings.contains(binding)
        {
            BindingShape::Unknown
        } else {
            BindingShape::Callable
        };
        file.add_binding(PlannedBinding::new(
            binding.clone(),
            binding.clone(),
            binding_shape,
            true,
        ));
    }
}

/// Emit the migrated-extra runtime chunks (snippet bodies and
/// namespace exports) into this module's source, ordered by their
/// original byte position in the runtime prelude. Identifier reads
/// inside each chunk are renamed through `migrated_runtime_dep_aliases`
/// and setter-writes are routed through `__reverts_set_X` helpers.
/// Called from both the source-free and source-backed module-emit
/// paths since the chunk-emit shape is identical.
pub(crate) fn emit_migrated_extra_chunks(
    program: &EnrichedProgram,
    file: &mut PlannedFile,
    migrated_extra_snippets: &BTreeSet<(u32, BindingName)>,
    migrated_extra_namespace_exports: &BTreeSet<(u32, BindingName)>,
    migrated_extra_runtime_dep_aliases: &BTreeMap<u32, BTreeMap<BindingName, BindingName>>,
    migrated_extra_noop_deps: &BTreeSet<BindingName>,
    migrated_extra_runtime_setter_deps_by_source: &BTreeMap<u32, BTreeSet<BindingName>>,
) -> BTreeSet<BindingName> {
    if migrated_extra_snippets.is_empty() && migrated_extra_namespace_exports.is_empty() {
        return BTreeSet::new();
    }
    let mut migrated_chunks = Vec::<(u32, u8, String)>::new();
    let mut retained_noop_deps = BTreeSet::<BindingName>::new();
    let migrated_runtime_dep_aliases = migrated_extra_runtime_dep_aliases
        .values()
        .flat_map(|aliases| aliases.iter())
        .map(|(original, alias)| (original.clone(), alias.clone()))
        .collect::<BTreeMap<_, _>>();
    for (source_file_id, binding) in migrated_extra_snippets {
        let Some(prelude) = program.model().graph().runtime_prelude(*source_file_id) else {
            continue;
        };
        let Some(snippet) = prelude.snippets.get(binding) else {
            continue;
        };
        let mut source = snippet.source.clone();
        if let Some(setter_deps) = migrated_extra_runtime_setter_deps_by_source.get(source_file_id)
            && !setter_deps.is_empty()
        {
            source = rewrite_runtime_helper_writes(source.as_str(), setter_deps);
        }
        source = rewrite_migrated_extra_noop_calls(
            source.as_str(),
            migrated_extra_noop_deps,
            &mut retained_noop_deps,
        );
        migrated_chunks.push((
            snippet.byte_start,
            0,
            rename_identifier_reads_in_source(source.as_str(), &migrated_runtime_dep_aliases),
        ));
    }
    for (source_file_id, namespace) in migrated_extra_namespace_exports {
        let Some(prelude) = program.model().graph().runtime_prelude(*source_file_id) else {
            continue;
        };
        let Some(namespace_export) = prelude
            .namespace_exports
            .iter()
            .find(|export| export.namespace == *namespace)
        else {
            continue;
        };
        let mut source = runtime_namespace_export_statement(namespace_export);
        source = rewrite_migrated_extra_noop_calls(
            source.as_str(),
            migrated_extra_noop_deps,
            &mut retained_noop_deps,
        );
        migrated_chunks.push((
            namespace_export.byte_start,
            1,
            rename_identifier_reads_in_source(source.as_str(), &migrated_runtime_dep_aliases),
        ));
    }
    migrated_chunks.sort_by_key(|(byte_start, kind, _source)| (*byte_start, *kind));
    for (_, _, source) in migrated_chunks {
        file.push_source(source);
    }
    retained_noop_deps
}

fn rewrite_migrated_extra_noop_calls(
    source: &str,
    migrated_extra_noop_deps: &BTreeSet<BindingName>,
    retained_noop_deps: &mut BTreeSet<BindingName>,
) -> String {
    if migrated_extra_noop_deps.is_empty() {
        return source.to_string();
    }
    let rewritten = rewrite_noop_runtime_helper_calls(source, migrated_extra_noop_deps);
    let rewritten = compact_bare_void_zero_expression_statements(rewritten.as_str());
    let remaining = identifiers_in_source(rewritten.as_str());
    retained_noop_deps.extend(
        migrated_extra_noop_deps
            .iter()
            .filter(|binding| remaining.contains(binding.as_str()))
            .cloned(),
    );
    rewritten
}

/// Emit `PlannedBinding`s for every graph-known definition in this
/// module, recording a readability rename when the emitted semantic
/// name differs from the original AST identifier and an `Unknown`
/// shape override for bindings that were reshaped during runtime
/// lowering.
pub(crate) fn emit_module_definition_bindings(
    program: &EnrichedProgram,
    module_id: ModuleId,
    file: &mut PlannedFile,
    planned_bindings: &mut BTreeSet<BindingName>,
    lowered_source: Option<&LoweredRuntimeModuleSource>,
) {
    let source_definitions = program.model().graph().ast_definitions_for(module_id);
    let module_has_source = program
        .model()
        .input()
        .module_source_slice(module_id)
        .is_some();
    let reshaped_bindings = lowered_source
        .map(|src| src.reshaped_bindings.clone())
        .unwrap_or_default();
    for original in program.model().graph().definitions_for(module_id) {
        let source_backed = source_definitions.contains(&original);
        let emitted = program
            .semantic_names()
            .binding_name(module_id, original.as_str())
            .cloned()
            .unwrap_or_else(|| original.clone());
        if emitted != original {
            file.add_readability_rename(PlannedRename::new(original.clone(), emitted.clone()));
        }
        if module_has_source && !source_backed {
            continue;
        }
        let shape_override = if reshaped_bindings.contains(&original) {
            Some(BindingShape::Unknown)
        } else {
            None
        };
        planned_bindings.insert(original.clone());
        file.add_binding(plan_binding_from_program(
            program,
            module_id,
            original,
            emitted,
            source_backed,
            shape_override,
        ));
    }
}

/// Emit `PlannedBinding`s for the module's source-imported names that
/// aren't already planned (graph imports the planner kept as-is),
/// each with a readability rename when the emitted semantic name
/// differs from the original.
pub(crate) fn emit_source_import_bindings(
    program: &EnrichedProgram,
    module_id: ModuleId,
    file: &mut PlannedFile,
    planned_bindings: &mut BTreeSet<BindingName>,
    source_imports: &BTreeSet<BindingName>,
) {
    for original in source_imports {
        if planned_bindings.contains(original) {
            continue;
        }
        let emitted = program
            .semantic_names()
            .binding_name(module_id, original.as_str())
            .cloned()
            .unwrap_or_else(|| original.clone());
        if emitted != *original {
            file.add_readability_rename(PlannedRename::new(original.clone(), emitted.clone()));
        }
        planned_bindings.insert(original.clone());
        file.add_binding(plan_binding_from_program(
            program,
            module_id,
            original.clone(),
            emitted,
            true,
            None,
        ));
    }
}

/// Peel package-runtime-owned helpers off the remaining-read and
/// written runtime-helper sets for this module's lowered source and
/// emit the corresponding package-runtime helper-file import (if any
/// remain after the split). Returns `(package_remaining, remaining,
/// package_written, written)` — the post-split sets needed by the
/// follow-up lazyValue localization and helper-usage recording.
pub(crate) fn emit_lowered_package_runtime_imports(
    args: LoweredPackageRuntimeImportArgs<'_>,
) -> (
    BTreeSet<BindingName>,
    BTreeSet<BindingName>,
    BTreeSet<BindingName>,
    BTreeSet<BindingName>,
) {
    let LoweredPackageRuntimeImportArgs {
        program,
        module_id,
        module_path,
        file,
        planned_bindings,
        used_package_runtime_helper_files,
        source_file_id,
        binding_owners,
        package_runtime_owner,
        remaining_runtime_helpers,
        written_runtime_helpers,
    } = args;
    let (package_remaining_helpers, remaining_runtime_helpers) = partition_package_runtime_bindings(
        binding_owners,
        package_runtime_owner,
        source_file_id,
        remaining_runtime_helpers,
    );
    let (package_written_helpers, written_runtime_helpers) = partition_package_runtime_bindings(
        binding_owners,
        package_runtime_owner,
        source_file_id,
        written_runtime_helpers,
    );
    if !package_remaining_helpers.is_empty() || !package_written_helpers.is_empty() {
        let mut package_import = PackageRuntimeImportEmitter {
            program,
            used_package_runtime_helper_files,
            file,
            planned_bindings,
            module_id,
            module_path,
            owner: package_runtime_owner.expect("package runtime bindings must have an owner"),
            source_file_id,
        };
        emit_package_runtime_helper_import(
            &mut package_import,
            &package_remaining_helpers,
            &package_written_helpers,
        );
    }
    (
        package_remaining_helpers,
        remaining_runtime_helpers,
        package_written_helpers,
        written_runtime_helpers,
    )
}

/// Second-chance lazyValue localization, run after singleton-inline
/// and package-runtime partitioning. A module can fail the earlier
/// gate because it still referenced a singleton runtime helper; once
/// that helper has been inlined and any package-runtime bindings
/// peeled off, if the shared `lazyValue` import is the only remaining
/// runtime edge, we localize it too and avoid materializing the
/// runtime-helper file just for the memoizer.
pub(crate) fn try_post_inline_localize_lazy_value(
    args: PostInlineLazyValueLocalizationArgs<'_>,
) -> Option<String> {
    let PostInlineLazyValueLocalizationArgs {
        lowered_source,
        namespace_member_rewrite,
        already_localized,
        has_runtime_edge_before_lazy_helpers,
        remaining_runtime_helpers,
        written_runtime_helpers,
        package_remaining_helpers,
        package_written_helpers,
        lazy_helper_names,
        runtime_import_partitions,
        runtime_singleton_inlines,
        binding_owners,
        package_runtime_owner,
        module_id,
    } = args;
    if already_localized.is_some()
        || !lowered_source.uses_lazy_value
        || lowered_source.uses_lazy_module
        || has_runtime_edge_before_lazy_helpers
        || !remaining_runtime_helpers.is_empty()
        || !written_runtime_helpers.is_empty()
        || !package_remaining_helpers.is_empty()
        || !package_written_helpers.is_empty()
        || !lazy_helper_names.contains(&"lazyValue")
    {
        return None;
    }
    let has_runtime_group_edges_after_inline =
        runtime_import_partitions
            .iter()
            .any(|(source_file_id, partition)| {
                let (_inline_bindings, runtime_bindings) =
                    partition_runtime_singleton_inline_bindings(
                        runtime_singleton_inlines,
                        module_id,
                        *source_file_id,
                        &partition.runtime_bindings,
                    );
                let (_package_bindings, runtime_bindings) = partition_package_runtime_bindings(
                    binding_owners,
                    package_runtime_owner,
                    *source_file_id,
                    &runtime_bindings,
                );
                !runtime_bindings.is_empty()
            });
    if has_runtime_group_edges_after_inline {
        return None;
    }
    let source_for_lazy = namespace_member_rewrite
        .map(|rewrite| rewrite.source.as_str())
        .unwrap_or(lowered_source.source.as_str());
    localize_lazy_value_source(source_for_lazy)
}

/// Emit the per-source-file runtime-import partitions for this
/// module: direct owner imports, direct prelude imports, singleton-
/// inline snippets, package-runtime helper imports for any package-
/// owned bindings, and finally the standard runtime-helper import for
/// the remaining bindings — updating usage/export accumulators along
/// the way.
pub(crate) fn emit_runtime_import_partitions(args: RuntimeImportPartitionEmitArgs<'_>) {
    let RuntimeImportPartitionEmitArgs {
        program,
        module_id,
        module_path,
        file,
        planned_bindings,
        used_runtime_helper_files,
        exported_runtime_helper_bindings,
        required_runtime_helper_bindings,
        used_package_runtime_helper_files,
        emitted_inline_runtime_helpers,
        runtime_import_partitions,
        runtime_singleton_inlines,
        binding_owners,
        package_runtime_owner,
    } = args;
    for (source_file_id, import_partition) in runtime_import_partitions {
        emit_direct_owner_imports(
            program,
            module_id,
            module_path,
            file,
            planned_bindings,
            &import_partition.direct_imports,
        );
        emit_direct_prelude_imports(
            file,
            planned_bindings,
            &import_partition.direct_prelude_imports,
        );
        let (inline_bindings, runtime_bindings) = partition_runtime_singleton_inline_bindings(
            runtime_singleton_inlines,
            module_id,
            source_file_id,
            &import_partition.runtime_bindings,
        );
        emit_runtime_singleton_inline_helpers(
            RuntimeSingletonInlineEmitContext {
                program,
                module_id,
                module_path,
            },
            runtime_singleton_inlines,
            file,
            planned_bindings,
            emitted_inline_runtime_helpers,
            source_file_id,
            &inline_bindings,
        );
        let (package_bindings, bindings) = partition_package_runtime_bindings(
            binding_owners,
            package_runtime_owner,
            source_file_id,
            &runtime_bindings,
        );
        if !package_bindings.is_empty() {
            let mut package_import = PackageRuntimeImportEmitter {
                program,
                used_package_runtime_helper_files,
                file,
                planned_bindings,
                module_id,
                module_path,
                owner: package_runtime_owner.expect("package runtime bindings must have an owner"),
                source_file_id,
            };
            emit_package_runtime_helper_import(
                &mut package_import,
                &package_bindings,
                &BTreeSet::new(),
            );
        }
        if bindings.is_empty() {
            continue;
        }
        used_runtime_helper_files
            .entry(source_file_id)
            .or_default()
            .extend(bindings.iter().cloned());
        exported_runtime_helper_bindings
            .entry(source_file_id)
            .or_default()
            .extend(bindings.iter().cloned());
        required_runtime_helper_bindings
            .entry(source_file_id)
            .or_default()
            .extend(bindings.iter().cloned());
        let specifier =
            relative_import_specifier(module_path, runtime_helpers_path(source_file_id).as_str());
        file.push_source(runtime_helper_import_statement(
            &bindings,
            &BTreeSet::new(),
            &[],
            specifier.as_str(),
        ));
        for binding in bindings {
            if planned_bindings.contains(&binding) {
                continue;
            }
            planned_bindings.insert(binding.clone());
            file.add_binding(plan_binding_from_program(
                program,
                module_id,
                binding.clone(),
                binding,
                true,
                None,
            ));
        }
    }
}

/// Emit the runtime-helper companion imports for this module's lowered
/// source. Source-specific helpers/setters still come from
/// `source-<N>-helpers.ts`, while the pure `lazyValue` / `lazyModule`
/// memoizers come from the shared `runtime/lazy.ts` file so lazy-only
/// consumers do not depend on a large per-source helper island. Skipped
/// entirely when every category is empty.
pub(crate) fn emit_lowered_runtime_helper_import(args: LoweredRuntimeHelperImportArgs<'_>) {
    let LoweredRuntimeHelperImportArgs {
        program,
        module_id,
        module_path,
        file,
        planned_bindings,
        source_file_id,
        remaining_runtime_helpers,
        written_runtime_helpers,
        lazy_helper_names,
    } = args;
    if remaining_runtime_helpers.is_empty()
        && written_runtime_helpers.is_empty()
        && lazy_helper_names.is_empty()
    {
        return;
    }
    if !remaining_runtime_helpers.is_empty() || !written_runtime_helpers.is_empty() {
        let specifier =
            relative_import_specifier(module_path, runtime_helpers_path(source_file_id).as_str());
        file.push_source(runtime_helper_import_statement(
            remaining_runtime_helpers,
            written_runtime_helpers,
            &[],
            specifier.as_str(),
        ));
    }
    if !lazy_helper_names.is_empty() {
        let specifier = relative_import_specifier(module_path, runtime_lazy_helpers_path());
        file.push_source(runtime_helper_import_statement(
            &BTreeSet::new(),
            &BTreeSet::new(),
            lazy_helper_names,
            specifier.as_str(),
        ));
    }
    for binding in remaining_runtime_helpers {
        if planned_bindings.contains(binding) {
            continue;
        }
        planned_bindings.insert(binding.clone());
        file.add_binding(plan_binding_from_program(
            program,
            module_id,
            binding.clone(),
            binding.clone(),
            true,
            None,
        ));
    }
}

/// Record this module's runtime-helper usage against the per-source
/// accumulators used by downstream emission and audits: which helper
/// files are read, which bindings are re-exported, which writers/
/// setters target this source, and which lazy helpers are imported or
/// re-exported by lazy form.
pub(crate) fn record_lowered_runtime_helper_usage(args: LoweredRuntimeHelperUsageArgs<'_>) {
    let LoweredRuntimeHelperUsageArgs {
        lowered_source,
        remaining_runtime_helpers,
        written_runtime_helpers,
        lazy_helper_names,
        used_runtime_helper_files,
        exported_runtime_helper_bindings,
        required_runtime_helper_bindings,
        used_runtime_helper_setters,
        used_lazy_module,
        used_lazy_value,
        exported_lazy_module,
        exported_lazy_value,
    } = args;
    let source_file_id = lowered_source.source_file_id;
    if !remaining_runtime_helpers.is_empty() || !written_runtime_helpers.is_empty() {
        used_runtime_helper_files.entry(source_file_id).or_default();
    }
    if !remaining_runtime_helpers.is_empty() {
        used_runtime_helper_files
            .entry(source_file_id)
            .or_default()
            .extend(remaining_runtime_helpers.iter().cloned());
        exported_runtime_helper_bindings
            .entry(source_file_id)
            .or_default()
            .extend(remaining_runtime_helpers.iter().cloned());
        required_runtime_helper_bindings
            .entry(source_file_id)
            .or_default()
            .extend(remaining_runtime_helpers.iter().cloned());
    }
    if !written_runtime_helpers.is_empty() {
        used_runtime_helper_setters
            .entry(source_file_id)
            .or_default()
            .extend(written_runtime_helpers.iter().cloned());
    }
    if lowered_source.uses_lazy_module {
        used_lazy_module.insert(source_file_id);
    }
    if lazy_helper_names.contains(&"lazyValue") {
        used_lazy_value.insert(source_file_id);
    }
    if lazy_helper_names.contains(&"lazyModule") {
        exported_lazy_module.insert(source_file_id);
    }
    if lazy_helper_names.contains(&"lazyValue") {
        exported_lazy_value.insert(source_file_id);
    }
}

/// Emit alias-renamed re-imports of runtime helpers that were moved
/// out to this module from another owner. Each `(source_file_id ->
/// {original -> alias})` produces one `import { original as alias, ...
/// } from "runtime-helpers/..."` statement and updates the helper-
/// tracking maps so audits know this module reads from that source.
pub(crate) fn emit_migrated_runtime_extra_alias_imports(
    args: MigratedRuntimeExtraAliasImportArgs<'_>,
) {
    let MigratedRuntimeExtraAliasImportArgs {
        module_path,
        file,
        planned_bindings,
        used_runtime_helper_files,
        exported_runtime_helper_bindings,
        required_runtime_helper_bindings,
        migrated_runtime_extra_runtime_dep_aliases,
    } = args;
    for (source_file_id, aliases) in migrated_runtime_extra_runtime_dep_aliases {
        if aliases.is_empty() {
            continue;
        }
        let original_bindings = aliases.keys().cloned().collect::<BTreeSet<_>>();
        used_runtime_helper_files
            .entry(*source_file_id)
            .or_default()
            .extend(original_bindings.iter().cloned());
        exported_runtime_helper_bindings
            .entry(*source_file_id)
            .or_default()
            .extend(original_bindings.iter().cloned());
        required_runtime_helper_bindings
            .entry(*source_file_id)
            .or_default()
            .extend(original_bindings.iter().cloned());
        let specifier =
            relative_import_specifier(module_path, runtime_helpers_path(*source_file_id).as_str());
        file.push_source(named_import_alias_statement(
            aliases
                .iter()
                .map(|(original, alias)| (original.as_str(), alias)),
            specifier.as_str(),
        ));
        for alias in aliases.values() {
            if planned_bindings.contains(alias) {
                continue;
            }
            planned_bindings.insert(alias.clone());
            file.add_binding(PlannedBinding::new(
                alias.clone(),
                alias.clone(),
                BindingShape::Unknown,
                true,
            ));
        }
    }
}

/// Emit plain re-export imports of runtime helpers that were
/// migrated from another owner's runtime file into this module so it
/// can re-export them. Each `(source_file_id -> {bindings})` emits
/// one helper-import statement and records the surface for audit.
pub(crate) fn emit_migrated_extra_runtime_reexport_imports(
    args: MigratedExtraRuntimeReexportImportArgs<'_>,
) {
    let MigratedExtraRuntimeReexportImportArgs {
        program,
        module_id,
        module_path,
        file,
        planned_bindings,
        used_runtime_helper_files,
        exported_runtime_helper_bindings,
        migrated_extra_runtime_reexport_deps,
    } = args;
    for (source_file_id, bindings) in migrated_extra_runtime_reexport_deps {
        used_runtime_helper_files
            .entry(*source_file_id)
            .or_default()
            .extend(bindings.iter().cloned());
        exported_runtime_helper_bindings
            .entry(*source_file_id)
            .or_default()
            .extend(bindings.iter().cloned());
        let specifier =
            relative_import_specifier(module_path, runtime_helpers_path(*source_file_id).as_str());
        file.push_source(runtime_helper_import_statement(
            bindings,
            &BTreeSet::new(),
            &[],
            specifier.as_str(),
        ));
        for binding in bindings {
            if planned_bindings.contains(binding) {
                continue;
            }
            planned_bindings.insert(binding.clone());
            file.add_binding(plan_binding_from_program(
                program,
                module_id,
                binding.clone(),
                binding.clone(),
                true,
                None,
            ));
        }
    }
}

/// Emit the migrated-extra owner/runtime-owner import statements that
/// have already been moved out of their original module: a plain
/// direct owner re-import for source/runtime-owner deps and an
/// aliased import for the alias-renamed bindings.
pub(crate) fn emit_migrated_extra_owner_imports(args: MigratedExtraOwnerImportArgs<'_>) {
    let MigratedExtraOwnerImportArgs {
        program,
        module_id,
        module_path,
        file,
        planned_bindings,
        migrated_extra_source_deps,
        migrated_extra_runtime_owner_deps,
        migrated_extra_runtime_owner_dep_aliases,
    } = args;
    if !migrated_extra_source_deps.is_empty() {
        emit_direct_owner_imports(
            program,
            module_id,
            module_path,
            file,
            planned_bindings,
            migrated_extra_source_deps,
        );
    }
    if !migrated_extra_runtime_owner_deps.is_empty() {
        emit_direct_owner_imports(
            program,
            module_id,
            module_path,
            file,
            planned_bindings,
            migrated_extra_runtime_owner_deps,
        );
    }
    if !migrated_extra_runtime_owner_dep_aliases.is_empty() {
        emit_direct_owner_import_aliases(
            program,
            module_path,
            file,
            planned_bindings,
            migrated_extra_runtime_owner_dep_aliases,
        );
    }
}

/// For every source whose helper file this module still imports, mark
/// its prelude-direct imports as runtime-routed (so the planner emits
/// them via the runtime companion instead of a direct prelude path),
/// except for bindings whose direct route was explicitly allowed by
/// `runtime_edge_direct_prelude_imports`.
pub(crate) fn route_prelude_imports_for_runtime_sources(
    lowered_source: Option<&LoweredRuntimeModuleSource>,
    lowered_import_partition: &mut RuntimeOwnerImportPartition,
    runtime_import_partitions: &mut [(u32, RuntimeOwnerImportPartition)],
    runtime_sources_for_module: &BTreeSet<u32>,
    runtime_edge_direct_prelude_imports: &BTreeMap<u32, BTreeSet<BindingName>>,
) {
    if let Some(lowered_source) = lowered_source
        && runtime_sources_for_module.contains(&lowered_source.source_file_id)
    {
        lowered_import_partition.route_prelude_imports_through_runtime_except(
            runtime_edge_direct_prelude_imports.get(&lowered_source.source_file_id),
        );
    }
    for (source_file_id, partition) in runtime_import_partitions {
        if runtime_sources_for_module.contains(source_file_id) {
            partition.route_prelude_imports_through_runtime_except(
                runtime_edge_direct_prelude_imports.get(source_file_id),
            );
        }
    }
}

/// Collect the helper bindings that drive each
/// `Object.defineProperties` namespace export. Used to gate which
/// runtime helpers can be dropped from the helper file: a helper that
/// only exists to back a namespace getter can be erased when nothing
/// in the rewritten source still references it.
pub(crate) fn namespace_export_helpers_for_source(
    program: &EnrichedProgram,
    lowered_source: Option<&LoweredRuntimeModuleSource>,
) -> BTreeSet<BindingName> {
    lowered_source
        .and_then(|source| {
            program
                .model()
                .graph()
                .runtime_prelude(source.source_file_id)
        })
        .map(|prelude| {
            prelude
                .namespace_exports
                .iter()
                .map(|export| export.helper.clone())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default()
}

/// Drop any namespace-export helper that the source no longer
/// references after the rewrites.
pub(crate) fn filter_unreferenced_namespace_helpers(
    remaining: BTreeSet<BindingName>,
    namespace_export_helpers: &BTreeSet<BindingName>,
    source_runtime_refs: &BTreeSet<BindingName>,
) -> BTreeSet<BindingName> {
    remaining
        .into_iter()
        .filter(|binding| {
            !namespace_export_helpers.contains(binding) || source_runtime_refs.contains(binding)
        })
        .collect()
}

/// Re-filter `remaining_runtime_helpers` after applying the
/// runtime-helper write rewrite. The write rewrite (`X = value` →
/// `__reverts_set_X(value)`) can introduce new identifier references
/// and erase others; this pass keeps only helpers that are still
/// referenced or still pulled in via migration.
pub(crate) fn filter_remaining_helpers_by_write_rewrite(
    args: RemainingHelpersWriteRewriteArgs<'_>,
) -> BTreeSet<BindingName> {
    let RemainingHelpersWriteRewriteArgs {
        program,
        module_id,
        source_module_wiring,
        lowered_source,
        namespace_member_rewrite,
        node_builtin_require_rewrite,
        written_runtime_helpers,
        migrated_extra_runtime_deps,
        remaining_runtime_helpers,
    } = args;
    let Some(lowered_source) = lowered_source else {
        return remaining_runtime_helpers;
    };
    if written_runtime_helpers.is_empty() {
        return remaining_runtime_helpers;
    }
    let source_text = node_builtin_require_rewrite
        .map(|rewrite| rewrite.source.as_str())
        .unwrap_or_else(|| {
            namespace_member_rewrite
                .map(|rewrite| rewrite.source.as_str())
                .unwrap_or(lowered_source.source.as_str())
        });
    let rewritten = rewrite_runtime_helper_writes(source_text, written_runtime_helpers);
    let mut refs_after_write_rewrite = runtime_import_identifiers_in_source(&rewritten)
        .into_iter()
        .map(BindingName::new)
        .collect::<BTreeSet<_>>();
    refs_after_write_rewrite.extend(
        program
            .model()
            .graph()
            .import_export()
            .exports_for(module_id),
    );
    if let Some(exports) = source_module_wiring.exports_by_module.get(&module_id) {
        refs_after_write_rewrite.extend(exports.iter().cloned());
    }
    if let Some((_stripped, exports)) = strip_top_level_named_exports(&rewritten) {
        refs_after_write_rewrite.extend(exports);
    }
    remaining_runtime_helpers
        .into_iter()
        .filter(|binding| {
            migrated_extra_runtime_deps.contains(binding)
                || refs_after_write_rewrite.contains(binding)
        })
        .collect()
}

/// Compute the union of runtime identifier references contained in
/// the (already-rewritten) source plus every binding the module
/// exports — both via explicit `export {…}` and via the source-module
/// wiring's import set. Top-level named exports get stripped and
/// folded back in so the binding's name survives even when the
/// `export` statement is later removed.
pub(crate) fn compute_source_runtime_refs(
    program: &EnrichedProgram,
    module_id: ModuleId,
    source_module_wiring: &SourceModuleWiring,
    lowered_source: Option<&LoweredRuntimeModuleSource>,
    namespace_member_rewrite: Option<
        &runtime_namespace_rewrite::RuntimeNamespaceMemberAccessRewrite,
    >,
    node_builtin_require_rewrite: Option<&NodeBuiltinRequireRewrite>,
) -> BTreeSet<BindingName> {
    let Some(source) = lowered_source else {
        return BTreeSet::new();
    };
    let source_text = node_builtin_require_rewrite
        .map(|rewrite| rewrite.source.as_str())
        .unwrap_or_else(|| {
            namespace_member_rewrite
                .map(|rewrite| rewrite.source.as_str())
                .unwrap_or(source.source.as_str())
        });
    let mut refs = runtime_import_identifiers_in_source(source_text)
        .into_iter()
        .map(BindingName::new)
        .collect::<BTreeSet<_>>();
    refs.extend(
        program
            .model()
            .graph()
            .import_export()
            .exports_for(module_id),
    );
    if let Some(exports) = source_module_wiring.exports_by_module.get(&module_id) {
        refs.extend(exports.iter().cloned());
    }
    if let Some((_stripped, exports)) = strip_top_level_named_exports(source_text) {
        refs.extend(exports);
    }
    refs
}

/// Compute the namespace-member-access rewrite for the lowered source,
/// reserving the union of source-definitions / source-imports /
/// already-planned bindings to avoid clobbering shadowable names.
pub(crate) fn compute_namespace_member_rewrite(
    program: &EnrichedProgram,
    lowered_source: Option<&LoweredRuntimeModuleSource>,
    runtime_import_groups: &BTreeMap<u32, BTreeSet<BindingName>>,
    local_source_definitions: &BTreeSet<BindingName>,
    source_imports: &BTreeSet<BindingName>,
    planned_bindings: &BTreeSet<BindingName>,
) -> Option<runtime_namespace_rewrite::RuntimeNamespaceMemberAccessRewrite> {
    lowered_source.and_then(|source| {
        let mut reserved_bindings = local_source_definitions.clone();
        reserved_bindings.extend(source_imports.iter().cloned());
        reserved_bindings.extend(planned_bindings.iter().cloned());
        rewrite_runtime_namespace_member_accesses(
            source.source.as_str(),
            runtime_import_groups,
            program.model().graph(),
            &reserved_bindings,
        )
    })
}

/// Compute the per-source-file `runtime_create_require_helpers` map
/// that the node-builtin-require rewriter needs.
pub(crate) fn compute_node_builtin_require_helpers(
    program: &EnrichedProgram,
    lowered_source: Option<&LoweredRuntimeModuleSource>,
    runtime_prelude_direct_imports: &BTreeMap<
        u32,
        BTreeMap<BindingName, RuntimePreludeDirectImport>,
    >,
) -> BTreeSet<BindingName> {
    lowered_source
        .and_then(|source| {
            let prelude = program
                .model()
                .graph()
                .runtime_prelude(source.source_file_id)?;
            Some(runtime_create_require_helpers(
                prelude,
                runtime_prelude_direct_imports.get(&source.source_file_id),
            ))
        })
        .unwrap_or_default()
}

/// Apply the node-builtin-require rewrite over the source that
/// optionally already had its namespace members rewritten. The reserved
/// binding set is recomputed so the new top-level definitions
/// introduced by the namespace rewrite are visible.
pub(crate) fn compute_node_builtin_require_rewrite(
    lowered_source: Option<&LoweredRuntimeModuleSource>,
    namespace_member_rewrite: Option<
        &runtime_namespace_rewrite::RuntimeNamespaceMemberAccessRewrite,
    >,
    node_builtin_require_helpers: &BTreeSet<BindingName>,
    local_source_definitions: &BTreeSet<BindingName>,
    source_imports: &BTreeSet<BindingName>,
    planned_bindings: &BTreeSet<BindingName>,
) -> Option<NodeBuiltinRequireRewrite> {
    lowered_source.map(|source| {
        let source_text = namespace_member_rewrite
            .map(|rewrite| rewrite.source.as_str())
            .unwrap_or(source.source.as_str());
        let mut reserved_bindings = local_source_definitions.clone();
        reserved_bindings.extend(source_imports.iter().cloned());
        reserved_bindings.extend(planned_bindings.iter().cloned());
        reserved_bindings.extend(top_level_definitions_in_source(source_text));
        rewrite_node_builtin_require_calls(
            source_text,
            node_builtin_require_helpers,
            &reserved_bindings,
        )
    })
}

/// Adjust the lowered-source `remaining_runtime_helpers` to reflect
/// Phase 10b migrations: bindings the migration pulled in as extra
/// runtime deps need to remain in the helper file, while bindings now
/// owned locally are removed from the runtime side.
pub(crate) fn adjust_remaining_runtime_helpers(
    remaining: &BTreeSet<BindingName>,
    migrated_extra_runtime_deps: &BTreeSet<BindingName>,
    migrated_local_bindings: &BTreeSet<BindingName>,
) -> BTreeSet<BindingName> {
    remaining
        .union(migrated_extra_runtime_deps)
        .cloned()
        .collect::<BTreeSet<_>>()
        .difference(migrated_local_bindings)
        .cloned()
        .collect()
}

/// Adjust the lowered-source `written_runtime_helpers` to reflect
/// Phase 10b migrations: bindings now owned locally no longer write
/// through the runtime setter, while moved reader snippets contribute
/// new setter writes.
pub(crate) fn adjust_written_runtime_helpers(
    written: &BTreeSet<BindingName>,
    migrated_locally: &BTreeSet<BindingName>,
    migrated_extra_runtime_setter_deps: &BTreeSet<BindingName>,
) -> BTreeSet<BindingName> {
    written
        .difference(migrated_locally)
        .cloned()
        .chain(migrated_extra_runtime_setter_deps.iter().cloned())
        .collect()
}

/// Per-owner snapshot of the runtime-var migration plan.
///
/// Computing all of these per-owner views at the top of the per-module
/// loop avoids re-walking the migration plan multiple times for the
/// same module. The fields mirror the `*_for_owner` accessors on
/// `RuntimeVarMigrationPlan`. Phase 10b commentary: `migrated_locally`
/// are the primary runtime vars now declared in this module — their
/// `X = value` writes stay as direct assignments instead of getting
/// rewritten to setter calls.
pub(crate) struct OwnerMigrationState {
    migrated_locally: BTreeSet<BindingName>,
    migrated_extra_snippets: BTreeSet<(u32, BindingName)>,
    migrated_extra_namespace_exports: BTreeSet<(u32, BindingName)>,
    migrated_extra_namespace_bindings: BTreeSet<BindingName>,
    migrated_extra_runtime_deps: BTreeSet<BindingName>,
    migrated_extra_runtime_setter_deps: BTreeSet<BindingName>,
    migrated_extra_runtime_setter_deps_by_source: BTreeMap<u32, BTreeSet<BindingName>>,
    migrated_extra_runtime_owner_deps: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    migrated_extra_runtime_owner_dep_aliases:
        BTreeMap<ModuleId, BTreeMap<BindingName, BindingName>>,
    migrated_extra_runtime_dep_aliases: BTreeMap<u32, BTreeMap<BindingName, BindingName>>,
    migrated_runtime_extra_runtime_dep_aliases: BTreeMap<u32, BTreeMap<BindingName, BindingName>>,
    migrated_extra_source_deps: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    migrated_extra_runtime_reexport_deps: BTreeMap<u32, BTreeSet<BindingName>>,
    migrated_extra_noop_deps: BTreeSet<BindingName>,
    migrated_local_bindings: BTreeSet<BindingName>,
}

impl OwnerMigrationState {
    fn from_plan(plan: &RuntimeVarMigrationPlan, module_id: ModuleId) -> Self {
        let migrated_locally = plan
            .migrations_by_owner
            .get(&module_id)
            .cloned()
            .unwrap_or_default();
        let migrated_extra_snippets = plan.extra_snippets_for_owner(module_id);
        let migrated_extra_namespace_exports = plan.extra_namespace_exports_for_owner(module_id);
        let migrated_extra_namespace_bindings = migrated_extra_namespace_exports
            .iter()
            .map(|(_, binding)| binding.clone())
            .collect::<BTreeSet<_>>();
        Self {
            migrated_locally,
            migrated_extra_snippets,
            migrated_extra_namespace_exports,
            migrated_extra_namespace_bindings,
            migrated_extra_runtime_deps: plan.extra_runtime_deps_for_owner(module_id),
            migrated_extra_runtime_setter_deps: plan.extra_runtime_setter_deps_for_owner(module_id),
            migrated_extra_runtime_setter_deps_by_source: plan
                .extra_runtime_setter_deps_by_source_for_owner(module_id),
            migrated_extra_runtime_owner_deps: plan
                .migrated_extra_runtime_deps_for_owner(module_id),
            migrated_extra_runtime_owner_dep_aliases: plan
                .migrated_aliased_extra_runtime_deps_for_owner(module_id),
            migrated_extra_runtime_dep_aliases: plan.extra_runtime_dep_aliases_for_owner(module_id),
            migrated_runtime_extra_runtime_dep_aliases: plan
                .runtime_extra_runtime_dep_aliases_for_owner(module_id),
            migrated_extra_source_deps: plan.extra_source_deps_for_owner(module_id),
            migrated_extra_runtime_reexport_deps: plan
                .extra_runtime_reexport_source_deps_for_owner(module_id),
            migrated_extra_noop_deps: plan.extra_noop_deps_for_owner(module_id),
            migrated_local_bindings: plan.local_bindings_for_owner(module_id),
        }
    }
}

/// Push every `PlannedImport` for module's package-graph imports
/// (the decisions surfaced by `program.package_imports_for`).
pub(crate) fn push_package_imports(
    program: &EnrichedProgram,
    module_id: ModuleId,
    file: &mut PlannedFile,
) {
    for decision in program.package_imports_for(module_id) {
        file.add_import(PlannedImport {
            namespace: decision.namespace_binding.clone(),
            resolution: decision.resolution.clone(),
            source_backed: decision.source_backed,
        });
    }
}

/// Final emission step for a folded-module stub file: emit no-op
/// shims for any package-runtime stubs the migrated cluster pulled
/// in, emit `export { … };` for migrated local bindings that weren't
/// already part of the stub-export surface, register `PlannedBinding`
/// entries for both the original folded stub exports and the migrated
/// local bindings. Namespace-component bindings get `Unknown` shape
/// because their shape was rewritten by namespace decomposition;
/// other migrated bindings are callable.
pub(crate) fn push_folded_noop_and_migrated_exports(args: FoldedNoopAndMigratedExportsArgs<'_>) {
    let FoldedNoopAndMigratedExportsArgs {
        folded,
        runtime_stub_exports,
        direct_stub_exports,
        migrated_extra_noop_deps,
        migrated_local_bindings,
        migrated_extra_namespace_bindings,
        file,
        planned_bindings,
    } = args;
    for binding in migrated_extra_noop_deps {
        file.push_source(noop_function_statement(binding));
    }
    let already_exported = runtime_stub_exports
        .iter()
        .cloned()
        .chain(direct_stub_exports.values().flatten().cloned())
        .collect::<BTreeSet<_>>();
    let migrated_exports = migrated_local_bindings
        .difference(&already_exported)
        .cloned()
        .collect::<BTreeSet<_>>();
    if !migrated_exports.is_empty() {
        file.push_source(named_export_statement(migrated_exports.iter()));
        for binding in &migrated_exports {
            file.add_export_with_source_backed(binding.clone(), true);
        }
    }
    for export in &folded.stub_exports {
        file.add_binding(PlannedBinding::new(
            export.clone(),
            export.clone(),
            BindingShape::Unknown,
            true,
        ));
        file.add_export_with_source_backed(export.clone(), true);
    }
    for binding in migrated_local_bindings {
        if planned_bindings.insert(binding.clone()) {
            let binding_shape = if migrated_extra_namespace_bindings.contains(binding) {
                BindingShape::Unknown
            } else {
                BindingShape::Callable
            };
            file.add_binding(PlannedBinding::new(
                binding.clone(),
                binding.clone(),
                binding_shape,
                true,
            ));
        }
    }
}

/// Push the recovered source of every migrated runtime snippet and
/// every migrated namespace-export Object.defineProperties statement
/// into `file`, in their original source order. The flat alias table
/// from `migrated_extra_runtime_dep_aliases` is used to rename any
/// references to bindings whose original name collided in this owner.
pub(crate) fn push_migrated_runtime_snippets_and_namespaces(
    program: &EnrichedProgram,
    migrated_extra_snippets: &BTreeSet<(u32, BindingName)>,
    migrated_extra_namespace_exports: &BTreeSet<(u32, BindingName)>,
    migrated_extra_runtime_dep_aliases: &BTreeMap<u32, BTreeMap<BindingName, BindingName>>,
    migrated_extra_noop_deps: &BTreeSet<BindingName>,
    file: &mut PlannedFile,
) -> BTreeSet<BindingName> {
    if migrated_extra_snippets.is_empty() && migrated_extra_namespace_exports.is_empty() {
        return BTreeSet::new();
    }
    let migrated_extra_runtime_dep_aliases = migrated_extra_runtime_dep_aliases
        .values()
        .flat_map(|aliases| aliases.iter())
        .map(|(original, alias)| (original.clone(), alias.clone()))
        .collect::<BTreeMap<_, _>>();
    let migrated_extra_runtime_dep_aliases = &migrated_extra_runtime_dep_aliases;
    let mut migrated_chunks = Vec::<(u32, u8, String)>::new();
    let mut retained_noop_deps = BTreeSet::<BindingName>::new();
    for (source_file_id, binding) in migrated_extra_snippets {
        let Some(prelude) = program.model().graph().runtime_prelude(*source_file_id) else {
            continue;
        };
        let Some(snippet) = prelude.snippets.get(binding) else {
            continue;
        };
        let source = rewrite_migrated_extra_noop_calls(
            snippet.source.as_str(),
            migrated_extra_noop_deps,
            &mut retained_noop_deps,
        );
        migrated_chunks.push((
            snippet.byte_start,
            0,
            rename_identifier_reads_in_source(source.as_str(), migrated_extra_runtime_dep_aliases),
        ));
    }
    for (source_file_id, namespace) in migrated_extra_namespace_exports {
        let Some(prelude) = program.model().graph().runtime_prelude(*source_file_id) else {
            continue;
        };
        let Some(namespace_export) = prelude
            .namespace_exports
            .iter()
            .find(|export| export.namespace == *namespace)
        else {
            continue;
        };
        let source = rewrite_migrated_extra_noop_calls(
            runtime_namespace_export_statement(namespace_export).as_str(),
            migrated_extra_noop_deps,
            &mut retained_noop_deps,
        );
        migrated_chunks.push((
            namespace_export.byte_start,
            1,
            rename_identifier_reads_in_source(source.as_str(), migrated_extra_runtime_dep_aliases),
        ));
    }
    migrated_chunks.sort_by_key(|(byte_start, kind, _source)| (*byte_start, *kind));
    for (_, _, source) in migrated_chunks {
        file.push_source(source);
    }
    retained_noop_deps
}

/// Emit `import { X, Y } from runtime` for each (source_file_id, bindings)
/// pair the migration plan recorded as still needed by this owner module
/// after its lazy fold. Each binding is registered in the helper-file /
/// exported / required indexes and turned into a `PlannedBinding`
/// derived from the program's shape/known-members data.
pub(crate) fn emit_runtime_extra_deps_imports(args: RuntimeExtraDepsImportArgs<'_>) {
    let RuntimeExtraDepsImportArgs {
        program,
        module_id,
        path,
        deps_by_source,
        file,
        planned_bindings,
        used_runtime_helper_files,
        exported_runtime_helper_bindings,
        required_runtime_helper_bindings,
    } = args;
    for (source_file_id, bindings) in deps_by_source {
        if bindings.is_empty() {
            continue;
        }
        used_runtime_helper_files
            .entry(*source_file_id)
            .or_default()
            .extend(bindings.iter().cloned());
        exported_runtime_helper_bindings
            .entry(*source_file_id)
            .or_default()
            .extend(bindings.iter().cloned());
        required_runtime_helper_bindings
            .entry(*source_file_id)
            .or_default()
            .extend(bindings.iter().cloned());
        let specifier =
            relative_import_specifier(path, runtime_helpers_path(*source_file_id).as_str());
        file.push_source(runtime_helper_import_statement(
            bindings,
            &BTreeSet::new(),
            &[],
            specifier.as_str(),
        ));
        for binding in bindings {
            if planned_bindings.contains(binding) {
                continue;
            }
            planned_bindings.insert(binding.clone());
            file.add_binding(plan_binding_from_program(
                program,
                module_id,
                binding.clone(),
                binding.clone(),
                true,
                None,
            ));
        }
    }
}

/// For each per-source-file alias group, emit a
/// `import { original as alias } from runtime` line for the folded
/// module + register every original/alias binding in the helper-file,
/// exported, and required indexes so the runtime helper file actually
/// surfaces them.
pub(crate) fn emit_runtime_extra_alias_imports(
    path: &str,
    aliases_by_source: &BTreeMap<u32, BTreeMap<BindingName, BindingName>>,
    file: &mut PlannedFile,
    planned_bindings: &mut BTreeSet<BindingName>,
    used_runtime_helper_files: &mut BTreeMap<u32, BTreeSet<BindingName>>,
    exported_runtime_helper_bindings: &mut BTreeMap<u32, BTreeSet<BindingName>>,
    required_runtime_helper_bindings: &mut BTreeMap<u32, BTreeSet<BindingName>>,
) {
    for (source_file_id, aliases) in aliases_by_source {
        if aliases.is_empty() {
            continue;
        }
        let original_bindings = aliases.keys().cloned().collect::<BTreeSet<_>>();
        used_runtime_helper_files
            .entry(*source_file_id)
            .or_default()
            .extend(original_bindings.iter().cloned());
        exported_runtime_helper_bindings
            .entry(*source_file_id)
            .or_default()
            .extend(original_bindings.iter().cloned());
        required_runtime_helper_bindings
            .entry(*source_file_id)
            .or_default()
            .extend(original_bindings.iter().cloned());
        let specifier =
            relative_import_specifier(path, runtime_helpers_path(*source_file_id).as_str());
        file.push_source(named_import_alias_statement(
            aliases
                .iter()
                .map(|(original, alias)| (original.as_str(), alias)),
            specifier.as_str(),
        ));
        for alias in aliases.values() {
            if planned_bindings.insert(alias.clone()) {
                file.add_binding(PlannedBinding::new(
                    alias.clone(),
                    alias.clone(),
                    BindingShape::Unknown,
                    true,
                ));
            }
        }
    }
}

/// Emit `export { X } from './other-owner.ts'` for each binding whose
/// owner moved to a different module. Owners without a resolvable output
/// path are silently skipped — they'll surface as audit findings later.
pub(crate) fn emit_folded_direct_stub_reexports(
    program: &EnrichedProgram,
    path: &str,
    direct_stub_exports: &BTreeMap<ModuleId, BTreeSet<BindingName>>,
    file: &mut PlannedFile,
) {
    for (owner_module, bindings) in direct_stub_exports {
        let Some(owner_path) = module_output_path(program, *owner_module) else {
            continue;
        };
        let specifier = relative_import_specifier(path, owner_path.as_str());
        file.push_source(named_reexport_statement(
            bindings.iter(),
            specifier.as_str(),
        ));
    }
}

/// When a lazy binding is folded into its helper module, the consumer no
/// longer carries the `lazyValue(...)`/`lazyModule(...)` call — but the
/// helper file does. Detect that case here so the helper file declares
/// and exports `lazyValue`/`lazyModule` even though no consumer needs to
/// import them.
pub(crate) fn detect_folded_lazy_helper_use(
    runtime_lazy_folds: &RuntimeLazyFoldPlan,
    used_lazy_module: &mut BTreeSet<u32>,
    used_lazy_value: &mut BTreeSet<u32>,
) {
    for (source_file_id, chunks) in &runtime_lazy_folds.chunks_by_source_file {
        for chunk in chunks {
            if chunk.source.contains("lazyModule(") {
                used_lazy_module.insert(*source_file_id);
            }
            if chunk.source.contains("lazyValue(") {
                used_lazy_value.insert(*source_file_id);
            }
        }
    }
}

impl ImportExportPlanner {
    #[must_use]
    pub fn runtime_setter_migration_blocker_report(
        self,
        program: &EnrichedProgram,
    ) -> RuntimeSetterMigrationBlockerReport {
        let context = PlannerContext::new(program);
        let analysis = context.analysis();
        runtime_setter_migration_blocker_report(
            context.program(),
            &analysis.source_module_wiring,
            &analysis.lowered_runtime_sources,
            &analysis.runtime_lazy_folds,
            &analysis.source_suppressed_packages,
        )
    }

    pub fn plan_enriched_program(self, program: &EnrichedProgram) -> Result<EmitPlan, PlanError> {
        let context = PlannerContext::new(program);
        AnalysisReadyPass.run(&context)?;
        planner_pipeline::run_planner_pipeline(&context)
    }

    #[must_use]
    pub fn plan_module_file(
        self,
        graph: &RevertsGraph,
        module_id: ModuleId,
        path: impl Into<String>,
    ) -> PlannedFile {
        let mut file = PlannedFile::new(path);
        for binding in graph.definitions_for(module_id) {
            file.add_export(binding);
        }
        file
    }
}

pub(crate) fn normalize_source_for_emit(
    _module_id: ModuleId,
    path: &str,
    source: &str,
    source_strategy: SourceCompilerStrategy,
) -> Result<String, PlanError> {
    // OXC's prettifier refuses some legitimate-but-edge bundle constructs
    // (e.g. `const X;` without initializer, certain JSX comma patterns).
    // Per ADR 0002 we don't repair the input: when prettification fails we
    // emit the raw source unchanged. Downstream `audit_emitted_project_parse`
    // will then surface an `UnparseableOutput` warning so the consumer
    // sees the affected module without stranding the whole project.
    Ok(format_source_pretty(
        source,
        source_strategy.path_hint(path),
        source_strategy.parse_goal(),
    )
    .unwrap_or_else(|_| source.to_string()))
}

pub(crate) fn group_runtime_imports(
    imports: Vec<RuntimePreludeImport>,
) -> BTreeMap<u32, BTreeSet<BindingName>> {
    let mut grouped = BTreeMap::<u32, BTreeSet<BindingName>>::new();
    for import in imports {
        grouped
            .entry(import.source_file_id)
            .or_default()
            .insert(import.binding);
    }
    grouped
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct SourceModuleWiring {
    pub(crate) imports_by_module: BTreeMap<ModuleId, BTreeMap<ModuleId, BTreeSet<BindingName>>>,
    pub(crate) exports_by_module: BTreeMap<ModuleId, BTreeSet<BindingName>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoweredRuntimeModuleSource {
    pub(crate) source_file_id: u32,
    pub(crate) source_file_path: String,
    pub(crate) byte_start: u32,
    pub(crate) source: String,
    pub(crate) lowered_helpers: BTreeSet<BindingName>,
    pub(crate) remaining_helpers: BTreeSet<BindingName>,
    pub(crate) local_definitions: BTreeSet<BindingName>,
    pub(crate) local_writes: BTreeSet<BindingName>,
    pub(crate) written_helpers: BTreeSet<BindingName>,
    pub(crate) uses_lazy_module: bool,
    pub(crate) uses_lazy_value: bool,
    /// Bindings whose shape was rewritten by delazify / namespace decomposition.
    /// The planner must override the IR-derived shape (which assumed the
    /// pre-lowering lazy thunk) to keep the audit consistent with what was
    /// actually emitted.
    pub(crate) reshaped_bindings: BTreeSet<BindingName>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeLazyFoldPlan {
    pub(crate) modules: BTreeMap<ModuleId, RuntimeLazyFoldModule>,
    pub(crate) chunks_by_source_file: BTreeMap<u32, Vec<RuntimeFoldedSourceChunk>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimePreludeDirectImport {
    pub(crate) source: String,
    pub(crate) snippet_source: String,
    pub(crate) snippet_byte_start: u32,
    pub(crate) kind: RuntimePreludeDirectImportKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimePreludeDirectImportKind {
    Default,
    Namespace,
    Named { imported: String },
}

pub(crate) fn partition_runtime_owner_bindings(
    binding_owners: &BindingOwnerPlan,
    source_file_id: u32,
    current_module: ModuleId,
    bindings: BTreeSet<BindingName>,
) -> RuntimeOwnerImportPartition {
    let mut partition = RuntimeOwnerImportPartition::default();
    for binding in bindings {
        match binding_owners.owner_for(source_file_id, &binding) {
            BindingOwner::Module(owner) if owner != current_module => {
                partition
                    .direct_imports
                    .entry(owner)
                    .or_default()
                    .insert(binding);
            }
            BindingOwner::Module(_owner_is_current_module) => {}
            BindingOwner::PreludeImport(import) => {
                partition.direct_prelude_imports.insert(binding, import);
            }
            BindingOwner::Runtime | BindingOwner::PackageRuntime(_) => {
                partition.runtime_bindings.insert(binding);
            }
        }
    }
    partition
}

pub(crate) fn emit_direct_owner_imports(
    program: &EnrichedProgram,
    module_id: ModuleId,
    module_path: &str,
    file: &mut PlannedFile,
    planned_bindings: &mut BTreeSet<BindingName>,
    direct_imports: &BTreeMap<ModuleId, BTreeSet<BindingName>>,
) {
    for (owner_module, bindings) in direct_imports {
        let Some(owner_path) = module_output_path(program, *owner_module) else {
            continue;
        };
        let bindings = bindings
            .iter()
            .filter(|binding| !planned_bindings.contains(*binding))
            .cloned()
            .collect::<BTreeSet<_>>();
        if bindings.is_empty() {
            continue;
        }
        let specifier = relative_import_specifier(module_path, owner_path.as_str());
        file.push_source(named_import_statement(bindings.iter(), specifier.as_str()));
        for binding in bindings {
            planned_bindings.insert(binding.clone());
            file.add_binding(plan_binding_from_program(
                program,
                module_id,
                binding.clone(),
                binding,
                true,
                None,
            ));
        }
    }
}

pub(crate) fn emit_direct_owner_import_aliases(
    program: &EnrichedProgram,
    module_path: &str,
    file: &mut PlannedFile,
    planned_bindings: &mut BTreeSet<BindingName>,
    direct_imports: &BTreeMap<ModuleId, BTreeMap<BindingName, BindingName>>,
) {
    for (owner_module, aliases) in direct_imports {
        let Some(owner_path) = module_output_path(program, *owner_module) else {
            continue;
        };
        let aliases = aliases
            .iter()
            .filter(|(_imported, local)| !planned_bindings.contains(*local))
            .map(|(imported, local)| (imported.clone(), local.clone()))
            .collect::<BTreeMap<_, _>>();
        if aliases.is_empty() {
            continue;
        }
        let specifier = relative_import_specifier(module_path, owner_path.as_str());
        file.push_source(named_import_alias_statement(
            aliases
                .iter()
                .map(|(imported, local)| (imported.as_str(), local)),
            specifier.as_str(),
        ));
        for alias in aliases.values() {
            planned_bindings.insert(alias.clone());
            file.add_binding(PlannedBinding::new(
                alias.clone(),
                alias.clone(),
                BindingShape::Unknown,
                true,
            ));
        }
    }
}

pub(crate) fn emit_node_builtin_default_imports(
    file: &mut PlannedFile,
    planned_bindings: &mut BTreeSet<BindingName>,
    imports: &BTreeMap<BindingName, String>,
) {
    for (binding, specifier) in imports {
        if planned_bindings.contains(binding) {
            continue;
        }
        file.push_source(default_import_statement(binding, specifier.as_str()));
        planned_bindings.insert(binding.clone());
        file.add_binding(PlannedBinding::new(
            binding.clone(),
            binding.clone(),
            BindingShape::Unknown,
            true,
        ));
    }
}

pub(crate) fn emit_direct_prelude_imports(
    file: &mut PlannedFile,
    planned_bindings: &mut BTreeSet<BindingName>,
    direct_imports: &BTreeMap<BindingName, RuntimePreludeDirectImport>,
) {
    if direct_imports.is_empty() {
        return;
    }

    let mut default_by_source = BTreeMap::<String, Vec<BindingName>>::new();
    let mut named_by_source = BTreeMap::<String, BTreeSet<(String, BindingName)>>::new();
    for (binding, import) in direct_imports {
        if planned_bindings.contains(binding) {
            continue;
        }
        match &import.kind {
            RuntimePreludeDirectImportKind::Default => {
                default_by_source
                    .entry(import.source.clone())
                    .or_default()
                    .push(binding.clone());
            }
            RuntimePreludeDirectImportKind::Namespace => {
                file.push_source(namespace_import_statement(binding, import.source.as_str()));
            }
            RuntimePreludeDirectImportKind::Named { imported } => {
                named_by_source
                    .entry(import.source.clone())
                    .or_default()
                    .insert((imported.clone(), binding.clone()));
            }
        }
        planned_bindings.insert(binding.clone());
        file.add_binding(PlannedBinding::new(
            binding.clone(),
            binding.clone(),
            BindingShape::Unknown,
            true,
        ));
    }

    for (source, specifiers) in named_by_source {
        let defaults = default_by_source.remove(&source).unwrap_or_default();
        if let [default_binding] = defaults.as_slice() {
            file.push_source(default_named_import_alias_statement(
                default_binding,
                specifiers
                    .iter()
                    .map(|(imported, local)| (imported.as_str(), local)),
                source.as_str(),
            ));
        } else {
            for default_binding in defaults {
                file.push_source(default_import_statement(&default_binding, source.as_str()));
            }
            file.push_source(named_import_alias_statement(
                specifiers
                    .iter()
                    .map(|(imported, local)| (imported.as_str(), local)),
                source.as_str(),
            ));
        }
    }
    for (source, defaults) in default_by_source {
        for default_binding in defaults {
            file.push_source(default_import_statement(&default_binding, source.as_str()));
        }
    }
}

pub(crate) fn runtime_prelude_direct_imports(
    program: &EnrichedProgram,
) -> BTreeMap<u32, BTreeMap<BindingName, RuntimePreludeDirectImport>> {
    program
        .model()
        .graph()
        .runtime_preludes()
        .iter()
        .filter_map(|(source_file_id, prelude)| {
            let imports = runtime_prelude_direct_imports_for_prelude(prelude);
            (!imports.is_empty()).then_some((*source_file_id, imports))
        })
        .collect()
}

pub(crate) fn runtime_prelude_direct_imports_for_prelude(
    prelude: &RuntimePrelude,
) -> BTreeMap<BindingName, RuntimePreludeDirectImport> {
    let mut imports = BTreeMap::<BindingName, RuntimePreludeDirectImport>::new();
    for (binding, snippet) in &prelude.snippets {
        let Some(mut import) =
            parse_runtime_prelude_direct_import(snippet.source.as_str(), binding)
        else {
            continue;
        };
        import.snippet_source = snippet.source.clone();
        import.snippet_byte_start = snippet.byte_start;
        imports.entry(binding.clone()).or_insert(import);
    }
    imports
}

pub(crate) fn runtime_edge_direct_prelude_imports(
    program: &EnrichedProgram,
    lowered_runtime_sources: &BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
    runtime_lazy_folds: &RuntimeLazyFoldPlan,
    direct_imports: &BTreeMap<u32, BTreeMap<BindingName, RuntimePreludeDirectImport>>,
) -> BTreeMap<u32, BTreeSet<BindingName>> {
    let consumers = runtime_prelude_direct_import_consumers(program, lowered_runtime_sources);
    let mut allowed = BTreeMap::<u32, BTreeSet<BindingName>>::new();

    for (source_file_id, imports) in direct_imports {
        let Some(prelude) = program.model().graph().runtime_prelude(*source_file_id) else {
            continue;
        };
        let mut imports_by_snippet = BTreeMap::<u32, BTreeSet<BindingName>>::new();
        let mut snippets_by_import_source = BTreeMap::<String, BTreeSet<u32>>::new();
        for (binding, import) in imports {
            imports_by_snippet
                .entry(import.snippet_byte_start)
                .or_default()
                .insert(binding.clone());
            snippets_by_import_source
                .entry(import.source.clone())
                .or_default()
                .insert(import.snippet_byte_start);
        }

        for bindings in imports_by_snippet.values() {
            let Some(first_binding) = bindings.first() else {
                continue;
            };
            let Some(first_import) = imports.get(first_binding) else {
                continue;
            };
            if snippets_by_import_source
                .get(&first_import.source)
                .is_some_and(|snippets| snippets.len() > 1)
            {
                continue;
            }
            if bindings.iter().any(|binding| {
                imports.get(binding).is_some_and(|import| {
                    matches!(import.kind, RuntimePreludeDirectImportKind::Namespace)
                })
            }) {
                continue;
            }
            let Some(statement_bindings) =
                import_statement_local_bindings(first_import.snippet_source.as_str())
            else {
                continue;
            };
            if statement_bindings != *bindings {
                continue;
            }
            let has_runtime_reader = bindings.iter().any(|binding| {
                runtime_prelude_import_has_runtime_reader(
                    prelude,
                    runtime_lazy_folds,
                    *source_file_id,
                    binding,
                    first_import.snippet_byte_start,
                )
            });
            if !runtime_prelude_direct_import_group_is_profitable(
                *source_file_id,
                bindings,
                imports,
                &consumers,
                !has_runtime_reader,
            ) {
                continue;
            }
            allowed
                .entry(*source_file_id)
                .or_default()
                .extend(bindings.iter().cloned());
        }
    }

    allowed
}

pub(crate) fn runtime_prelude_direct_import_consumers(
    program: &EnrichedProgram,
    lowered_runtime_sources: &BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
) -> BTreeMap<(u32, BindingName), BTreeSet<ModuleId>> {
    let mut consumers = BTreeMap::<(u32, BindingName), BTreeSet<ModuleId>>::new();
    for module in program.model().modules() {
        for import in program.model().graph().runtime_imports_for(module.id) {
            consumers
                .entry((import.source_file_id, import.binding))
                .or_default()
                .insert(module.id);
        }
        if let Some(source) = lowered_runtime_sources.get(&module.id) {
            for binding in &source.remaining_helpers {
                consumers
                    .entry((source.source_file_id, binding.clone()))
                    .or_default()
                    .insert(module.id);
            }
        }
    }
    consumers
}

pub(crate) fn runtime_prelude_import_has_runtime_reader(
    prelude: &RuntimePrelude,
    runtime_lazy_folds: &RuntimeLazyFoldPlan,
    source_file_id: u32,
    binding: &BindingName,
    import_snippet_byte_start: u32,
) -> bool {
    for snippet in prelude.snippets.values() {
        if snippet.byte_start == import_snippet_byte_start {
            continue;
        }
        if identifiers_in_source(snippet.source.as_str())
            .into_iter()
            .any(|identifier| identifier == binding.as_str())
        {
            return true;
        }
    }
    for namespace_export in &prelude.namespace_exports {
        if namespace_export.namespace == *binding
            || namespace_export
                .exports
                .values()
                .any(|export| export == binding)
        {
            return true;
        }
    }
    if let Some(entrypoint) = &prelude.entrypoint {
        if entrypoint.callee == *binding {
            return true;
        }
        for side_effect in runtime_entrypoint_side_effects(prelude, entrypoint) {
            if identifiers_in_source(side_effect.source.as_str())
                .into_iter()
                .any(|identifier| identifier == binding.as_str())
            {
                return true;
            }
        }
    }
    runtime_lazy_folds
        .chunks_by_source_file
        .get(&source_file_id)
        .is_some_and(|chunks| {
            chunks.iter().any(|chunk| {
                identifiers_in_source(chunk.source.as_str())
                    .into_iter()
                    .any(|identifier| identifier == binding.as_str())
            })
        })
}

pub(crate) fn runtime_prelude_direct_import_group_is_profitable(
    source_file_id: u32,
    bindings: &BTreeSet<BindingName>,
    imports: &BTreeMap<BindingName, RuntimePreludeDirectImport>,
    consumers: &BTreeMap<(u32, BindingName), BTreeSet<ModuleId>>,
    saves_runtime_snippet: bool,
) -> bool {
    let Some(first_binding) = bindings.first() else {
        return false;
    };
    let Some(first_import) = imports.get(first_binding) else {
        return false;
    };
    let mut saved = if saves_runtime_snippet {
        first_import.snippet_source.len()
    } else {
        0
    };
    let mut added = BTreeMap::<ModuleId, BTreeMap<String, BTreeSet<(String, BindingName)>>>::new();
    let mut default_or_namespace_imports = Vec::<(ModuleId, &RuntimePreludeDirectImport)>::new();
    for binding in bindings {
        saved += binding.as_str().len() + 2; // runtime export list
        let Some(import) = imports.get(binding) else {
            return false;
        };
        let binding_consumers = consumers
            .get(&(source_file_id, binding.clone()))
            .cloned()
            .unwrap_or_default();
        if binding_consumers.is_empty() {
            return false;
        }
        for module_id in binding_consumers {
            saved += binding.as_str().len() + 2; // runtime import list in the consumer
            match &import.kind {
                RuntimePreludeDirectImportKind::Named { imported } => {
                    added
                        .entry(module_id)
                        .or_default()
                        .entry(import.source.clone())
                        .or_default()
                        .insert((imported.clone(), binding.clone()));
                }
                RuntimePreludeDirectImportKind::Default
                | RuntimePreludeDirectImportKind::Namespace => {
                    default_or_namespace_imports.push((module_id, import));
                }
            }
        }
    }

    let mut added_bytes = 0usize;
    for (_module_id, imports_by_source) in added {
        for (source, specifiers) in imports_by_source {
            added_bytes += named_import_alias_statement(
                specifiers
                    .iter()
                    .map(|(imported, local)| (imported.as_str(), local)),
                source.as_str(),
            )
            .len();
        }
    }
    for (_module_id, import) in default_or_namespace_imports {
        added_bytes += match import.kind {
            RuntimePreludeDirectImportKind::Default => default_import_statement(
                first_local_for_import(bindings, imports, import),
                &import.source,
            )
            .len(),
            RuntimePreludeDirectImportKind::Namespace => namespace_import_statement(
                first_local_for_import(bindings, imports, import),
                &import.source,
            )
            .len(),
            RuntimePreludeDirectImportKind::Named { .. } => 0,
        };
    }

    saved > added_bytes
}

pub(crate) fn add_global_owned_runtime_snippet_migrations(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
    lowered_runtime_sources: &BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
    runtime_lazy_folds: &RuntimeLazyFoldPlan,
    externalized_packages: &BTreeSet<ModuleId>,
    plan: &mut RuntimeVarMigrationPlan,
) {
    let folded_modules = runtime_lazy_folds
        .modules
        .keys()
        .copied()
        .collect::<BTreeSet<_>>();
    let source_definition_modules_by_source =
        runtime_owner_definition_modules_by_source(program, externalized_packages);
    let module_dependencies_by_owner = module_dependency_modules_by_owner(program);
    let owner_available_bindings = runtime_reader_owner_available_bindings(
        program,
        source_module_wiring,
        lowered_runtime_sources,
        lowered_runtime_sources.keys().copied(),
    );
    let owner_local_definitions = lowered_runtime_sources
        .iter()
        .map(|(module_id, source)| (*module_id, source.local_definitions.clone()))
        .collect::<BTreeMap<_, _>>();
    let modules_by_id = program
        .model()
        .modules()
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();

    for (source_file_id, prelude) in program.model().graph().runtime_preludes() {
        let folded_chunks = runtime_lazy_folds
            .chunks_by_source_file
            .get(source_file_id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let read_index = runtime_source_read_index(prelude, folded_chunks);
        let source_definition_modules = source_definition_modules_by_source
            .get(source_file_id)
            .cloned()
            .unwrap_or_default();
        let namespace_target_owners =
            runtime_namespace_target_owners(&read_index, &source_definition_modules);
        let mut eligible_movable_bindings = BTreeSet::<BindingName>::new();
        let mut candidate_owners = BTreeMap::<BindingName, ModuleId>::new();
        for (binding, snippet) in &prelude.snippets {
            if plan.migrated_owner(*source_file_id, binding).is_some()
                || !matches!(
                    prelude.binding_kind(binding),
                    Some(RuntimePreludeBindingKind::SourceBacked)
                )
                || !global_owner_movable_runtime_snippet(
                    &read_index,
                    binding,
                    snippet.source.as_str(),
                )
                || read_index.entrypoint_callee.as_ref() == Some(binding)
            {
                continue;
            }
            eligible_movable_bindings.insert(binding.clone());
            let definition_owner = source_definition_modules
                .get(binding)
                .and_then(|module_id| *module_id);
            let namespace_owner = read_index
                .namespace_exports_by_namespace
                .get(binding)
                .and_then(|namespace_export| {
                    namespace_export_owner(namespace_export, &source_definition_modules)
                });
            let namespace_target_owner = namespace_target_owners
                .get(binding)
                .and_then(|owner| *owner);
            let span_owner = runtime_snippet_source_span_owner(
                program.model().modules(),
                prelude.source_file_id,
                snippet.byte_start,
                snippet.source.len(),
                externalized_packages,
            );
            let owner_module = global_runtime_snippet_owner(
                definition_owner,
                namespace_owner.or(namespace_target_owner),
                span_owner,
            );
            let Some(owner_module) = owner_module else {
                continue;
            };
            if runtime_owner_candidate_can_emit(
                binding,
                owner_module,
                &modules_by_id,
                externalized_packages,
                &owner_local_definitions,
                &owner_available_bindings,
                &folded_modules,
            ) {
                candidate_owners.insert(binding.clone(), owner_module);
            }
        }
        for (binding, owner_module) in
            runtime_adjacent_snippet_owners(prelude, &eligible_movable_bindings, &candidate_owners)
        {
            if candidate_owners.contains_key(&binding) {
                continue;
            }
            if runtime_owner_candidate_can_emit(
                &binding,
                owner_module,
                &modules_by_id,
                externalized_packages,
                &owner_local_definitions,
                &owner_available_bindings,
                &folded_modules,
            ) {
                candidate_owners.insert(binding, owner_module);
            }
        }
        propagate_runtime_reader_owned_snippet_candidates(
            prelude,
            &read_index,
            &eligible_movable_bindings,
            &mut candidate_owners,
            &owner_available_bindings,
            &owner_local_definitions,
            &folded_modules,
        );
        let owner_runtime_state = runtime_reader_owner_runtime_state(
            lowered_runtime_sources,
            candidate_owners.values().copied(),
        );

        for (binding, migration) in closed_global_owned_runtime_snippets(
            prelude,
            &read_index,
            &candidate_owners,
            &owner_available_bindings,
            &module_dependencies_by_owner,
            &owner_runtime_state,
        ) {
            plan.insert_owned_snippet(binding, migration);
        }
    }
}

pub(crate) fn runtime_owner_candidate_can_emit(
    binding: &BindingName,
    owner_module: ModuleId,
    modules_by_id: &BTreeMap<ModuleId, &ModuleInput>,
    externalized_packages: &BTreeSet<ModuleId>,
    owner_local_definitions: &BTreeMap<ModuleId, BTreeSet<BindingName>>,
    owner_available_bindings: &BTreeMap<ModuleId, BTreeSet<BindingName>>,
    folded_modules: &BTreeSet<ModuleId>,
) -> bool {
    if modules_by_id.get(&owner_module).is_some_and(|module| {
        module.kind == ModuleKind::Package && externalized_packages.contains(&owner_module)
    }) {
        return false;
    }
    if owner_local_definitions
        .get(&owner_module)
        .is_some_and(|definitions| definitions.contains(binding))
    {
        // The owner source already emits this exact top-level definition.
        // Re-emitting the runtime snippet there would create duplicate
        // declarations, including for folded lazy modules whose source body is
        // still materialized.
        return false;
    }
    if !folded_modules.contains(&owner_module)
        && owner_available_bindings
            .get(&owner_module)
            .is_some_and(|available| available.contains(binding))
    {
        // The owner source already emits this name. Re-emitting the runtime
        // snippet there would create a duplicate declaration; keep the runtime
        // copy until a source-span-aware replacement pass can delete the
        // original definition.
        return false;
    }
    true
}

pub(crate) fn runtime_adjacent_snippet_owners(
    prelude: &RuntimePrelude,
    eligible_movable_bindings: &BTreeSet<BindingName>,
    known_owners: &BTreeMap<BindingName, ModuleId>,
) -> BTreeMap<BindingName, ModuleId> {
    let ordered = prelude
        .snippets
        .iter()
        .filter(|(binding, _snippet)| eligible_movable_bindings.contains(*binding))
        .map(|(binding, snippet)| (snippet.byte_start, binding.clone()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut owners = BTreeMap::<BindingName, ModuleId>::new();
    for (index, (_byte_start, binding)) in ordered.iter().enumerate() {
        if known_owners.contains_key(binding) {
            continue;
        }
        let previous_owner = ordered[..index]
            .iter()
            .rev()
            .find_map(|(_, previous)| known_owners.get(previous).copied());
        let next_owner = ordered[index + 1..]
            .iter()
            .find_map(|(_, next)| known_owners.get(next).copied());
        if let (Some(previous_owner), Some(next_owner)) = (previous_owner, next_owner)
            && previous_owner == next_owner
        {
            owners.insert(binding.clone(), previous_owner);
        }
    }
    owners
}

pub(crate) fn global_runtime_snippet_owner(
    definition_owner: Option<ModuleId>,
    namespace_owner: Option<ModuleId>,
    span_owner: Option<ModuleId>,
) -> Option<ModuleId> {
    // Source spans are useful when the runtime binding has no recovered symbol,
    // but they are lossy for bundled lazy stubs: a large function/class can be
    // hoisted into runtime while the owning module span only covers the tiny
    // lazy wrapper that initializes its package/module. A unique source-local
    // symbol owner is therefore stronger than a conflicting span overlap.
    definition_owner.or(span_owner).or(namespace_owner)
}

pub(crate) fn namespace_export_owner(
    namespace_export: &RuntimeNamespaceExport,
    source_definition_modules: &BTreeMap<BindingName, Option<ModuleId>>,
) -> Option<ModuleId> {
    let mut owners = namespace_export
        .exports
        .values()
        .filter_map(|binding| {
            source_definition_modules
                .get(binding)
                .and_then(|owner| *owner)
        })
        .collect::<BTreeSet<_>>();
    let owner = owners.pop_first()?;
    owners.is_empty().then_some(owner)
}

pub(crate) fn runtime_namespace_target_owners(
    read_index: &RuntimeSourceReadIndex,
    source_definition_modules: &BTreeMap<BindingName, Option<ModuleId>>,
) -> BTreeMap<BindingName, Option<ModuleId>> {
    let mut owners = BTreeMap::<BindingName, Option<ModuleId>>::new();
    for (namespace, namespace_export) in &read_index.namespace_exports_by_namespace {
        let Some(owner_module) = source_definition_modules
            .get(namespace)
            .and_then(|owner| *owner)
        else {
            continue;
        };
        for target in namespace_export.exports.values() {
            owners
                .entry(target.clone())
                .and_modify(|existing| {
                    if *existing != Some(owner_module) {
                        *existing = None;
                    }
                })
                .or_insert(Some(owner_module));
        }
    }
    owners
}

pub(crate) fn runtime_snippet_source_span_owner<'a>(
    modules: impl IntoIterator<Item = &'a ModuleInput>,
    source_file_id: u32,
    byte_start: u32,
    source_len: usize,
    externalized_packages: &BTreeSet<ModuleId>,
) -> Option<ModuleId> {
    let source_len = u32::try_from(source_len).unwrap_or(u32::MAX);
    let mut byte_end = byte_start.saturating_add(source_len);
    if byte_end == byte_start {
        byte_end = byte_start.saturating_add(1);
    }

    let candidate_spans = modules
        .into_iter()
        .filter(|module| module.source_file_id == Some(source_file_id))
        .filter(|module| {
            !(module.kind == ModuleKind::Package && externalized_packages.contains(&module.id))
        })
        .filter_map(|module| {
            let span = module.source_span?;
            (span.byte_start < byte_end && byte_start < span.byte_end).then_some((module.id, span))
        })
        .collect::<Vec<_>>();

    // Prefer a true containment owner over a generic overlap. Bundler module
    // spans can overlap when a wrapper/source unit was split again later; the
    // smallest unique containing span is the most specific owner and avoids
    // conservatively dropping recoverable runtime classes/functions as
    // ambiguous. Fall back to the historical unique-overlap gate when no
    // containing span exists.
    let containing = candidate_spans
        .iter()
        .filter(|(_, span)| span.byte_start <= byte_start && byte_end <= span.byte_end)
        .map(|(module_id, span)| {
            (
                span.byte_end.saturating_sub(span.byte_start),
                span.byte_start,
                span.byte_end,
                *module_id,
            )
        })
        .collect::<BTreeSet<_>>();
    if let Some((best_len, _best_start, _best_end, owner)) = containing.iter().next().copied() {
        let equally_specific = containing
            .iter()
            .filter(|(len, _start, _end, _)| *len == best_len)
            .count();
        if equally_specific == 1 {
            return Some(owner);
        }
        return None;
    }

    let mut owners = candidate_spans
        .into_iter()
        .map(|(module_id, _)| module_id)
        .collect::<BTreeSet<_>>();
    let owner = owners.pop_first()?;
    owners.is_empty().then_some(owner)
}

pub(crate) fn global_owner_movable_runtime_snippet(
    read_index: &RuntimeSourceReadIndex,
    binding: &BindingName,
    source: &str,
) -> bool {
    if read_index
        .namespace_exports_by_namespace
        .contains_key(binding)
    {
        return is_migratable_namespace_reader_snippet(binding, source);
    }
    if runtime_prelude_snippet_is_noop(binding.as_str(), source) {
        return false;
    }
    is_migratable_reader_function_snippet(binding, source)
}

pub(crate) fn propagate_runtime_reader_owned_snippet_candidates(
    prelude: &RuntimePrelude,
    read_index: &RuntimeSourceReadIndex,
    eligible_movable_bindings: &BTreeSet<BindingName>,
    candidate_owners: &mut BTreeMap<BindingName, ModuleId>,
    owner_available_bindings: &BTreeMap<ModuleId, BTreeSet<BindingName>>,
    owner_local_definitions: &BTreeMap<ModuleId, BTreeSet<BindingName>>,
    folded_modules: &BTreeSet<ModuleId>,
) {
    loop {
        let mut additions = Vec::<(BindingName, ModuleId)>::new();
        for binding in eligible_movable_bindings {
            if candidate_owners.contains_key(binding) {
                continue;
            }
            let readers = runtime_readers_for_binding(read_index, binding);
            if readers.is_empty() {
                continue;
            }
            let mut reader_owners = BTreeSet::<ModuleId>::new();
            let mut all_readers_owned = true;
            for reader in &readers {
                let Some(reader_owner) = candidate_owners.get(reader).copied() else {
                    all_readers_owned = false;
                    break;
                };
                reader_owners.insert(reader_owner);
            }
            if !all_readers_owned || reader_owners.len() != 1 {
                continue;
            }
            let owner_module = *reader_owners
                .iter()
                .next()
                .expect("single reader owner should exist");
            if owner_local_definitions
                .get(&owner_module)
                .is_some_and(|definitions| definitions.contains(binding))
            {
                continue;
            }
            if !folded_modules.contains(&owner_module)
                && owner_available_bindings
                    .get(&owner_module)
                    .is_some_and(|available| available.contains(binding))
            {
                continue;
            }
            if prelude.snippets.get(binding).is_none_or(|snippet| {
                !global_owner_movable_runtime_snippet(read_index, binding, snippet.source.as_str())
            }) {
                continue;
            }
            additions.push((binding.clone(), owner_module));
        }
        if additions.is_empty() {
            break;
        }
        for (binding, owner_module) in additions {
            candidate_owners.insert(binding, owner_module);
        }
    }
}

pub(crate) fn closed_global_owned_runtime_snippets(
    prelude: &RuntimePrelude,
    read_index: &RuntimeSourceReadIndex,
    candidate_owners: &BTreeMap<BindingName, ModuleId>,
    owner_available_bindings: &BTreeMap<ModuleId, BTreeSet<BindingName>>,
    module_dependencies_by_owner: &BTreeMap<ModuleId, BTreeSet<ModuleId>>,
    owner_runtime_state: &BTreeMap<ModuleId, RuntimeReaderOwnerRuntimeState>,
) -> BTreeMap<BindingName, RuntimeOwnedSnippetMigration> {
    let mut selected = candidate_owners.clone();
    // Reachability through selected owner-to-owner runtime dependencies is
    // monotonic as candidates are removed: deleting candidates can only remove
    // paths, never create new ones. Use the initial graph as an over-approximate
    // cycle proof for the pruning loop so large cyclic regions are discarded in
    // batches instead of re-solving transitive closure after every small change.
    // This is conservative: a candidate may stay in runtime after a later
    // removal would have broken its cycle, but emitted code never gains an
    // unsafe owner import.
    let selected_owner_paths = selected_owner_dependency_reachability(&selected, read_index);
    loop {
        let mut removed = BTreeSet::<BindingName>::new();
        for (binding, owner_module) in &selected {
            let Some(snippet) = prelude.snippets.get(binding) else {
                removed.insert(binding.clone());
                continue;
            };
            if read_index.entrypoint_callee.as_ref() == Some(binding)
                || read_index.namespace_export_helpers.contains(binding)
            {
                removed.insert(binding.clone());
                continue;
            }

            let runtime_reads = read_index
                .free_bindings_by_snippet
                .get(binding)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|dep| prelude.defines(dep))
                .collect::<BTreeSet<_>>();
            let blocking_cross_owner_deps = runtime_reads
                .iter()
                .filter(|dep| {
                    let Some(dep_owner) = selected.get(*dep) else {
                        return false;
                    };
                    dep_owner != owner_module
                        && (module_dependency_path_exists(
                            module_dependencies_by_owner,
                            *dep_owner,
                            *owner_module,
                        ) || selected_owner_paths
                            .get(dep_owner)
                            .is_some_and(|reachable| reachable.contains(owner_module)))
                })
                .cloned()
                .collect::<BTreeSet<_>>();
            if !blocking_cross_owner_deps.is_empty() {
                let binding_lines = snippet.source.lines().count().max(1);
                for dep in blocking_cross_owner_deps {
                    let dep_lines = prelude
                        .snippets
                        .get(&dep)
                        .map(|dep_snippet| dep_snippet.source.lines().count().max(1))
                        .unwrap_or(usize::MAX);
                    if dep_lines < binding_lines {
                        // Break owner<->owner cycles by pinning the smaller
                        // dependency in runtime. The larger recovered owner
                        // snippet can then import it as a stable runtime dep
                        // instead of discarding the whole recovered component.
                        removed.insert(dep);
                    } else {
                        removed.insert(binding.clone());
                    }
                }
                continue;
            }

            let runtime_writes = read_index
                .runtime_writes_by_snippet
                .get(binding)
                .cloned()
                .unwrap_or_default();
            let blocking_write = runtime_writes.iter().find(|dep| {
                selected
                    .get(dep)
                    .is_none_or(|dep_owner| dep_owner != owner_module)
            });
            if blocking_write.is_some() {
                removed.insert(binding.clone());
                continue;
            }

            if read_index.non_snippet_runtime_reads.contains(binding)
                && !global_owned_folded_runtime_read_can_import_owner(
                    GlobalOwnedRuntimeEdgeContext {
                        prelude,
                        read_index,
                        selected: &selected,
                        owner_runtime_state,
                    },
                    binding,
                    *owner_module,
                    &runtime_reads,
                )
            {
                removed.insert(binding.clone());
                continue;
            }

            let runtime_readers = runtime_readers_for_binding(read_index, binding);
            let retained_runtime_readers = runtime_readers
                .iter()
                .filter(|reader| !selected.contains_key(*reader))
                .cloned()
                .collect::<BTreeSet<_>>();
            if !retained_runtime_readers.is_empty()
                && !global_owned_retained_runtime_readers_can_import_owner(
                    GlobalOwnedRuntimeEdgeContext {
                        prelude,
                        read_index,
                        selected: &selected,
                        owner_runtime_state,
                    },
                    binding,
                    *owner_module,
                    &retained_runtime_readers,
                    &runtime_reads,
                )
            {
                removed.insert(binding.clone());
                continue;
            }

            let runtime_writers = read_index
                .snippet_writers_by_binding
                .get(binding)
                .cloned()
                .unwrap_or_default();
            let blocking_writer = runtime_writers.iter().find(|writer| {
                selected
                    .get(writer)
                    .is_none_or(|writer_owner| writer_owner != owner_module)
            });
            if blocking_writer.is_some() {
                removed.insert(binding.clone());
                continue;
            }

            if let Some(namespace_export) = read_index.namespace_exports_by_namespace.get(binding)
                && namespace_export.exports.values().any(|target| {
                    let Some(target_owner) = selected.get(target) else {
                        return false;
                    };
                    target_owner != owner_module
                        && (module_dependency_path_exists(
                            module_dependencies_by_owner,
                            *target_owner,
                            *owner_module,
                        ) || selected_owner_paths
                            .get(target_owner)
                            .is_some_and(|reachable| reachable.contains(owner_module)))
                })
            {
                removed.insert(binding.clone());
            }
        }
        if removed.is_empty() {
            break;
        }
        for binding in removed {
            selected.remove(&binding);
        }
    }

    let final_selected = selected.clone();
    final_selected
        .into_iter()
        .filter_map(|(binding, owner_module)| {
            let _snippet = prelude.snippets.get(&binding)?;
            let mut extra_runtime_deps = BTreeSet::<BindingName>::new();
            let mut extra_noop_deps = BTreeSet::<BindingName>::new();
            for dep in read_index
                .free_bindings_by_snippet
                .get(&binding)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|dep| prelude.defines(dep))
                .filter(|dep| {
                    selected
                        .get(dep)
                        .is_none_or(|dep_owner| *dep_owner != owner_module)
                })
            {
                if prelude.snippets.get(&dep).is_some_and(|dep_snippet| {
                    runtime_prelude_snippet_is_noop(dep.as_str(), dep_snippet.source.as_str())
                }) {
                    extra_noop_deps.insert(dep);
                } else {
                    extra_runtime_deps.insert(dep);
                }
            }
            let runtime_writes = read_index
                .runtime_writes_by_snippet
                .get(&binding)
                .cloned()
                .unwrap_or_default();
            if runtime_writes.iter().any(|dep| {
                selected
                    .get(dep)
                    .is_none_or(|dep_owner| *dep_owner != owner_module)
            }) {
                return None;
            }
            let moves_namespace_export = read_index
                .namespace_exports_by_namespace
                .contains_key(&binding);
            if let Some(namespace_export) = read_index.namespace_exports_by_namespace.get(&binding)
            {
                for target in namespace_export.exports.values() {
                    if selected
                        .get(target)
                        .is_some_and(|target_owner| *target_owner == owner_module)
                    {
                        continue;
                    }
                    if prelude.defines(target) {
                        if prelude.snippets.get(target).is_some_and(|target_snippet| {
                            runtime_prelude_snippet_is_noop(
                                target.as_str(),
                                target_snippet.source.as_str(),
                            )
                        }) {
                            extra_noop_deps.insert(target.clone());
                        } else {
                            extra_runtime_deps.insert(target.clone());
                        }
                    }
                }
            }
            extra_runtime_deps.remove(&binding);
            extra_noop_deps.remove(&binding);
            let owner_available = owner_available_bindings
                .get(&owner_module)
                .cloned()
                .unwrap_or_default();
            let extra_runtime_dep_aliases = global_owned_runtime_dep_aliases_for_owner_conflicts(
                &binding,
                &extra_runtime_deps,
                &owner_available,
            );
            Some((
                binding,
                RuntimeOwnedSnippetMigration {
                    owner_module,
                    source_file_id: prelude.source_file_id,
                    extra_runtime_deps,
                    extra_runtime_dep_aliases,
                    extra_noop_deps,
                    moves_namespace_export,
                },
            ))
        })
        .collect()
}

#[derive(Clone, Copy)]
pub(crate) struct GlobalOwnedRuntimeEdgeContext<'a> {
    prelude: &'a RuntimePrelude,
    read_index: &'a RuntimeSourceReadIndex,
    selected: &'a BTreeMap<BindingName, ModuleId>,
    owner_runtime_state: &'a BTreeMap<ModuleId, RuntimeReaderOwnerRuntimeState>,
}

pub(crate) fn global_owned_retained_runtime_readers_can_import_owner(
    ctx: GlobalOwnedRuntimeEdgeContext<'_>,
    binding: &BindingName,
    owner_module: ModuleId,
    retained_runtime_readers: &BTreeSet<BindingName>,
    runtime_reads: &BTreeSet<BindingName>,
) -> bool {
    if retained_runtime_readers.is_empty() {
        return true;
    }
    // A retained namespace export object is still emitted in the runtime
    // helper. After this binding moves, runtime_module_owner_imports_for_source
    // can import the moved target back from its rebuilt owner and keep the
    // namespace barrel live. Other retained runtime readers would keep calling
    // arbitrary runtime code across a new runtime -> owner edge, so they stay
    // blocked until a fuller SCC move can take them as well.
    if retained_runtime_readers
        .iter()
        .any(|reader| !global_owned_retained_runtime_reader_is_import_safe(ctx, reader))
    {
        return false;
    }

    global_owned_runtime_edge_to_owner_is_safe(ctx, binding, owner_module, runtime_reads)
}

pub(crate) fn global_owned_retained_runtime_reader_is_import_safe(
    ctx: GlobalOwnedRuntimeEdgeContext<'_>,
    reader: &BindingName,
) -> bool {
    if ctx
        .read_index
        .namespace_exports_by_namespace
        .contains_key(reader)
    {
        return true;
    }
    if retained_runtime_reader_has_unsafe_runtime_caller(ctx.read_index, reader, ctx.selected) {
        return false;
    }
    global_owned_moved_snippet_is_cycle_safe(ctx.prelude, reader)
}

pub(crate) fn retained_runtime_reader_has_unsafe_runtime_caller(
    read_index: &RuntimeSourceReadIndex,
    reader: &BindingName,
    selected: &BTreeMap<BindingName, ModuleId>,
) -> bool {
    let mut visited = BTreeSet::<BindingName>::new();
    let mut stack = vec![reader.clone()];
    while let Some(binding) = stack.pop() {
        if !visited.insert(binding.clone()) {
            continue;
        }
        if read_index.entrypoint_callee.as_ref() == Some(&binding)
            || read_index
                .entrypoint_non_snippet_runtime_reads
                .contains(&binding)
            || read_index.folded_unsafe_runtime_reads.contains(&binding)
        {
            return true;
        }
        for caller in runtime_readers_for_binding(read_index, &binding) {
            if selected.contains_key(&caller) {
                continue;
            }
            stack.push(caller);
        }
    }
    false
}

pub(crate) fn global_owned_folded_runtime_read_can_import_owner(
    ctx: GlobalOwnedRuntimeEdgeContext<'_>,
    binding: &BindingName,
    owner_module: ModuleId,
    runtime_reads: &BTreeSet<BindingName>,
) -> bool {
    if ctx
        .read_index
        .entrypoint_non_snippet_runtime_reads
        .contains(binding)
        || !ctx
            .read_index
            .folded_non_snippet_runtime_reads
            .contains(binding)
    {
        return false;
    }
    // Folded chunks stay in the runtime helper. They may import a recovered
    // owner binding only when the read is not an eager top-level call: lazy
    // initializer reads and non-call references are already deferred/benign,
    // while eager calls would execute through a new runtime -> owner edge.
    if ctx.read_index.folded_unsafe_runtime_reads.contains(binding)
        || (!ctx
            .read_index
            .folded_lazy_safe_runtime_reads
            .contains(binding)
            && !ctx
                .read_index
                .folded_non_call_runtime_reads
                .contains(binding))
    {
        return false;
    }

    global_owned_runtime_edge_to_owner_is_safe(ctx, binding, owner_module, runtime_reads)
}

pub(crate) fn global_owned_runtime_edge_to_owner_is_safe(
    ctx: GlobalOwnedRuntimeEdgeContext<'_>,
    binding: &BindingName,
    owner_module: ModuleId,
    runtime_reads: &BTreeSet<BindingName>,
) -> bool {
    let Some(owner_state) = ctx.owner_runtime_state.get(&owner_module) else {
        return false;
    };
    if owner_state.uses_lazy_module
        || !owner_runtime_writes_are_lazy_safe(
            owner_state.source.as_str(),
            &owner_state.written_helpers,
        )
    {
        return false;
    }

    let locally_owned_runtime = ctx
        .selected
        .iter()
        .filter_map(|(selected_binding, selected_owner)| {
            (*selected_owner == owner_module).then_some(selected_binding.clone())
        })
        .collect::<BTreeSet<_>>();
    let moved_runtime_deps = runtime_reads
        .iter()
        .filter(|dep| {
            ctx.selected
                .get(*dep)
                .is_none_or(|dep_owner| *dep_owner != owner_module)
        })
        .filter(|dep| {
            !ctx.prelude.snippets.get(*dep).is_some_and(|dep_snippet| {
                runtime_prelude_snippet_is_noop(dep.as_str(), dep_snippet.source.as_str())
            })
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    let owner_runtime_deps = owner_state
        .remaining_helpers
        .iter()
        .filter(|helper| !locally_owned_runtime.contains(*helper))
        .cloned()
        .chain(moved_runtime_deps)
        .collect::<BTreeSet<_>>();
    owner_runtime_imports_are_lazy_safe(owner_state.source.as_str(), &owner_runtime_deps)
        && global_owned_moved_snippet_is_cycle_safe(ctx.prelude, binding)
}

pub(crate) fn global_owned_moved_snippet_is_cycle_safe(
    prelude: &RuntimePrelude,
    binding: &BindingName,
) -> bool {
    let Some(snippet) = prelude.snippets.get(binding) else {
        return false;
    };
    let source = snippet.source.trim();
    if function_declaration_names_binding(source, binding) {
        return true;
    }
    if class_declaration_names_binding(source, binding) {
        return true;
    }
    if is_migratable_namespace_reader_snippet(binding, source) {
        return true;
    }
    variable_declaration_names_function_like_binding(source, binding)
}

pub(crate) fn global_owned_runtime_dep_aliases_for_owner_conflicts(
    binding: &BindingName,
    extra_runtime_deps: &BTreeSet<BindingName>,
    owner_available_bindings: &BTreeSet<BindingName>,
) -> BTreeMap<BindingName, BindingName> {
    let mut aliases = BTreeMap::<BindingName, BindingName>::new();
    let mut reserved = owner_available_bindings.clone();
    reserved.insert(binding.clone());
    reserved.extend(extra_runtime_deps.iter().cloned());
    for dep in extra_runtime_deps
        .iter()
        .filter(|dep| owner_available_bindings.contains(*dep))
    {
        let alias = unique_runtime_dep_alias(dep, &mut reserved);
        aliases.insert(dep.clone(), alias);
    }
    aliases
}

pub(crate) fn selected_owner_dependency_reachability(
    selected: &BTreeMap<BindingName, ModuleId>,
    read_index: &RuntimeSourceReadIndex,
) -> BTreeMap<ModuleId, BTreeSet<ModuleId>> {
    let mut direct = BTreeMap::<ModuleId, BTreeSet<ModuleId>>::new();
    for (binding, owner) in selected {
        for dep in selected_runtime_dependencies_for_binding(read_index, binding) {
            let Some(dep_owner) = selected.get(&dep) else {
                continue;
            };
            if dep_owner != owner {
                direct.entry(*owner).or_default().insert(*dep_owner);
            }
        }
    }
    let owners = direct
        .keys()
        .copied()
        .chain(direct.values().flatten().copied())
        .collect::<BTreeSet<_>>();
    let mut reachability = BTreeMap::<ModuleId, BTreeSet<ModuleId>>::new();
    for owner in owners {
        let mut reachable = BTreeSet::<ModuleId>::new();
        let mut stack = direct
            .get(&owner)
            .into_iter()
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        while let Some(next) = stack.pop() {
            if !reachable.insert(next) {
                continue;
            }
            if let Some(next_owners) = direct.get(&next) {
                stack.extend(next_owners.iter().copied());
            }
        }
        if !reachable.is_empty() {
            reachability.insert(owner, reachable);
        }
    }
    reachability
}

pub(crate) fn selected_runtime_dependencies_for_binding(
    read_index: &RuntimeSourceReadIndex,
    binding: &BindingName,
) -> BTreeSet<BindingName> {
    let mut deps = read_index
        .free_bindings_by_snippet
        .get(binding)
        .cloned()
        .unwrap_or_default();
    if let Some(namespace_export) = read_index.namespace_exports_by_namespace.get(binding) {
        deps.extend(namespace_export.exports.values().cloned());
    }
    deps
}

fn add_runtime_setter_blocker_reason(
    report: &mut RuntimeSetterMigrationBlockerReport,
    source_id: u32,
    binding: BindingName,
    reason: RuntimeSetterMigrationBlockerReason,
    sub_reason: Option<&'static str>,
    is_setter_dependency: bool,
) {
    let (reason, sub_reason) = if is_setter_dependency {
        (
            RuntimeSetterMigrationBlockerReason::RuntimeReaderWriteSetterDependency,
            Some(sub_reason.unwrap_or_else(|| reason.as_str())),
        )
    } else {
        (reason, sub_reason)
    };
    if let Some(sub_reason) = sub_reason {
        report.add_reason_with_sub(source_id, binding, reason, Some(sub_reason));
    } else {
        report.add_reason(source_id, binding, reason);
    }
}

pub(crate) fn runtime_setter_migration_blocker_report(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
    lowered_runtime_sources: &BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
    runtime_lazy_folds: &RuntimeLazyFoldPlan,
    externalized_packages: &BTreeSet<ModuleId>,
) -> RuntimeSetterMigrationBlockerReport {
    let actual_migrations = compute_runtime_var_migration_plan(
        program,
        source_module_wiring,
        lowered_runtime_sources,
        runtime_lazy_folds,
        externalized_packages,
    );
    let folded_modules: BTreeSet<ModuleId> = runtime_lazy_folds.modules.keys().copied().collect();
    let source_definition_modules =
        unique_source_definition_modules(program, externalized_packages);
    let all_source_definition_modules = unique_source_definition_modules(program, &BTreeSet::new());
    let module_dependencies_by_owner = module_dependency_modules_by_owner(program);
    let runtime_source_consumers =
        runtime_prelude_direct_import_consumers(program, lowered_runtime_sources);
    let modules_by_id = program
        .model()
        .modules()
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let mut eligible_writers = BTreeMap::<(u32, BindingName), BTreeSet<ModuleId>>::new();
    let mut excluded_folded = BTreeMap::<(u32, BindingName), BTreeSet<ModuleId>>::new();
    let mut excluded_externalized = BTreeMap::<(u32, BindingName), BTreeSet<ModuleId>>::new();
    let runtime_reader_write_setter_deps = actual_migrations
        .migrations_by_binding
        .values()
        .flat_map(|migration| {
            migration
                .extra_runtime_setter_deps
                .iter()
                .cloned()
                .map(|binding| (migration.source_file_id, binding))
        })
        .collect::<BTreeSet<_>>();

    for (module_id, source) in lowered_runtime_sources {
        for binding in &source.written_helpers {
            let key = (source.source_file_id, binding.clone());
            if folded_modules.contains(module_id) {
                excluded_folded.entry(key).or_default().insert(*module_id);
                continue;
            }
            if modules_by_id.get(module_id).is_some_and(|module| {
                module.kind == ModuleKind::Package && externalized_packages.contains(module_id)
            }) {
                excluded_externalized
                    .entry(key)
                    .or_default()
                    .insert(*module_id);
                continue;
            }
            eligible_writers.entry(key).or_default().insert(*module_id);
        }
    }

    let all_keys = eligible_writers
        .keys()
        .chain(excluded_folded.keys())
        .chain(excluded_externalized.keys())
        .chain(runtime_reader_write_setter_deps.iter())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut report = RuntimeSetterMigrationBlockerReport {
        total_bindings: all_keys.len(),
        ..Default::default()
    };

    let mut by_source = BTreeMap::<u32, Vec<(BindingName, ModuleId)>>::new();
    for (source_id, binding) in &all_keys {
        if actual_migrations
            .migrations_by_binding
            .get(binding)
            .is_some_and(|migration| migration.source_file_id == *source_id)
        {
            report.add_accepted(*source_id, binding.clone());
            continue;
        }
        let is_setter_dependency =
            runtime_reader_write_setter_deps.contains(&(*source_id, binding.clone()));
        let eligible = eligible_writers
            .get(&(*source_id, binding.clone()))
            .cloned()
            .unwrap_or_default();
        if eligible.is_empty() {
            let reason = if excluded_folded.contains_key(&(*source_id, binding.clone())) {
                RuntimeSetterMigrationBlockerReason::FoldedWriterOnly
            } else if excluded_externalized.contains_key(&(*source_id, binding.clone())) {
                RuntimeSetterMigrationBlockerReason::ExternalizedPackageWriterOnly
            } else {
                RuntimeSetterMigrationBlockerReason::NoEligibleWriter
            };
            add_runtime_setter_blocker_reason(
                &mut report,
                *source_id,
                binding.clone(),
                reason,
                None,
                is_setter_dependency,
            );
            continue;
        }
        if eligible.len() > 1 {
            add_runtime_setter_blocker_reason(
                &mut report,
                *source_id,
                binding.clone(),
                RuntimeSetterMigrationBlockerReason::MultipleEligibleWriters,
                None,
                is_setter_dependency,
            );
            continue;
        }
        let owner_module = *eligible
            .iter()
            .next()
            .expect("single eligible writer must be present");
        by_source
            .entry(*source_id)
            .or_default()
            .push((binding.clone(), owner_module));
    }

    for (source_id, candidates) in by_source {
        let Some(prelude) = program.model().graph().runtime_prelude(source_id) else {
            for (binding, _owner_module) in candidates {
                let is_setter_dependency =
                    runtime_reader_write_setter_deps.contains(&(source_id, binding.clone()));
                add_runtime_setter_blocker_reason(
                    &mut report,
                    source_id,
                    binding,
                    RuntimeSetterMigrationBlockerReason::MissingRuntimePrelude,
                    None,
                    is_setter_dependency,
                );
            }
            continue;
        };
        let mut initialized_candidates = Vec::<(BindingName, ModuleId)>::new();
        for (binding, owner_module) in candidates {
            if migratable_runtime_var_initializer(prelude, &binding).is_none() {
                let is_setter_dependency =
                    runtime_reader_write_setter_deps.contains(&(source_id, binding.clone()));
                add_runtime_setter_blocker_reason(
                    &mut report,
                    source_id,
                    binding,
                    RuntimeSetterMigrationBlockerReason::InitializerNotMigratable,
                    None,
                    is_setter_dependency,
                );
                continue;
            }
            initialized_candidates.push((binding, owner_module));
        }
        let movable_bindings = initialized_candidates
            .iter()
            .map(|(binding, _)| binding.clone())
            .collect::<BTreeSet<_>>();
        let candidate_owners = initialized_candidates
            .iter()
            .map(|(binding, owner_module)| (binding.clone(), *owner_module))
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
        let mut reported_primary_bindings = BTreeSet::<BindingName>::new();
        for (binding, owner_module) in initialized_candidates {
            if reported_primary_bindings.contains(&binding) {
                continue;
            }
            let is_setter_dependency =
                runtime_reader_write_setter_deps.contains(&(source_id, binding.clone()));
            match runtime_binding_read_profile_diagnostic(&read_index, &binding) {
                Ok(RuntimeBindingReadProfile::NoReads) => {
                    add_runtime_setter_blocker_reason(
                        &mut report,
                        source_id,
                        binding.clone(),
                        RuntimeSetterMigrationBlockerReason::NoDiagnosticStatus,
                        None,
                        is_setter_dependency,
                    );
                    reported_primary_bindings.insert(binding);
                }
                Ok(RuntimeBindingReadProfile::SnippetReaders(readers)) => {
                    let cluster_result = migratable_runtime_reader_cluster_result(
                        &reader_cluster_context,
                        owner_module,
                        &binding,
                        readers,
                    );
                    match cluster_result {
                        Ok(_) => add_runtime_setter_blocker_reason(
                            &mut report,
                            source_id,
                            binding,
                            RuntimeSetterMigrationBlockerReason::ReaderClusterOverlapsMigratedBinding,
                            None,
                            is_setter_dependency,
                        ),
                        Err(blocker) => add_runtime_setter_blocker_reason(
                            &mut report,
                            source_id,
                            binding,
                            blocker.into(),
                            blocker.sub_reason(),
                            is_setter_dependency,
                        ),
                    }
                }
                Ok(RuntimeBindingReadProfile::Rejected) => {
                    unreachable!("diagnostic read profile returns Err for rejected bindings");
                }
                Err(reason) => {
                    if reason == RuntimeSetterMigrationBlockerReason::RuntimeNonSnippetRead
                        && runtime_reader_folded_non_snippet_use_can_move(
                            &reader_cluster_context,
                            &binding,
                        )
                    {
                        let readers = runtime_readers_for_binding(&read_index, &binding);
                        if !readers.is_empty() {
                            let cluster_result = migratable_runtime_reader_cluster_result(
                                &reader_cluster_context,
                                owner_module,
                                &binding,
                                readers,
                            );
                            match cluster_result {
                                Ok(_) => add_runtime_setter_blocker_reason(
                                    &mut report,
                                    source_id,
                                    binding,
                                    RuntimeSetterMigrationBlockerReason::ReaderClusterOverlapsMigratedBinding,
                                    None,
                                    is_setter_dependency,
                                ),
                                Err(blocker) => add_runtime_setter_blocker_reason(
                                    &mut report,
                                    source_id,
                                    binding,
                                    blocker.into(),
                                    blocker.sub_reason(),
                                    is_setter_dependency,
                                ),
                            }
                            continue;
                        }
                    }
                    add_runtime_setter_blocker_reason(
                        &mut report,
                        source_id,
                        binding,
                        reason,
                        None,
                        is_setter_dependency,
                    )
                }
            }
        }
    }

    debug_assert_eq!(
        report.total_bindings,
        report.accepted_bindings + report.blocked_bindings
    );
    report
}

pub(crate) struct RuntimeReaderClusterContext<'a> {
    pub(crate) source_file_id: u32,
    pub(crate) owner_available_bindings: &'a BTreeMap<ModuleId, BTreeSet<BindingName>>,
    pub(crate) source_consumers_by_runtime_binding:
        &'a BTreeMap<(u32, BindingName), BTreeSet<ModuleId>>,
    pub(crate) source_definition_modules: &'a BTreeMap<BindingName, Option<ModuleId>>,
    pub(crate) all_source_definition_modules: &'a BTreeMap<BindingName, Option<ModuleId>>,
    pub(crate) externalized_packages: &'a BTreeSet<ModuleId>,
    pub(crate) module_dependencies_by_owner: &'a BTreeMap<ModuleId, BTreeSet<ModuleId>>,
    pub(crate) folded_modules: &'a BTreeSet<ModuleId>,
    pub(crate) folded_runtime_definitions: &'a BTreeSet<BindingName>,
    pub(crate) owner_runtime_state: &'a BTreeMap<ModuleId, RuntimeReaderOwnerRuntimeState>,
    /// Source line count for each candidate owner module. Lets the cluster
    /// size cap scale with the receiving module so large source modules can
    /// absorb proportionally larger reader clusters; small modules still
    /// see the fixed floor.
    pub(crate) owner_source_lines: &'a BTreeMap<ModuleId, usize>,
    pub(crate) prelude: &'a RuntimePrelude,
    pub(crate) read_index: &'a RuntimeSourceReadIndex,
    pub(crate) movable_bindings: &'a BTreeSet<BindingName>,
    pub(crate) candidate_owners: &'a BTreeMap<BindingName, ModuleId>,
}

/// Per-owner cluster line cap: keep the historical fixed floor as a lower
/// bound, but allow growth proportional to the owner module's source so
/// large modules can absorb proportionally larger runtime clusters
/// without producing 99%-runtime-disguised-as-source output for small
/// modules.
pub(crate) fn runtime_reader_cluster_cap_for_owner(
    ctx: &RuntimeReaderClusterContext<'_>,
    owner_module: ModuleId,
) -> usize {
    let owner_lines = ctx
        .owner_source_lines
        .get(&owner_module)
        .copied()
        .unwrap_or(0);
    MAX_RUNTIME_READER_MIGRATION_CLUSTER_LINES.max(owner_lines.saturating_mul(3))
}

/// Tally source-file line counts for the given owner modules. Modules
/// produced by a bundle splitter are typically only a handful of lines
/// each — the meaningful "absorption capacity" is the source file the
/// module slice was carved from, not the per-module slice. Two owners
/// drawn from the same source file share its capacity here.
pub(crate) fn owner_module_source_lines(
    program: &EnrichedProgram,
    owners: impl IntoIterator<Item = ModuleId>,
) -> BTreeMap<ModuleId, usize> {
    let input = program.model().input();
    let mut file_lines = BTreeMap::<u32, usize>::new();
    for source_file in &input.source_files {
        let lines = source_file
            .source
            .as_deref()
            .map(|s| s.lines().count())
            .unwrap_or(0);
        file_lines.insert(source_file.id, lines);
    }
    let mut map = BTreeMap::new();
    for owner in owners {
        if map.contains_key(&owner) {
            continue;
        }
        let lines = input
            .modules
            .iter()
            .find(|module| module.id == owner)
            .and_then(|module| module.source_file_id)
            .and_then(|id| file_lines.get(&id).copied())
            .unwrap_or(0);
        map.insert(owner, lines);
    }
    map
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeReaderOwnerRuntimeState {
    pub(crate) source: String,
    pub(crate) remaining_helpers: BTreeSet<BindingName>,
    pub(crate) written_helpers: BTreeSet<BindingName>,
    pub(crate) uses_lazy_module: bool,
    pub(crate) uses_lazy_value: bool,
    pub(crate) can_localize_lazy_value: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeReaderClusterBlocker {
    NonSnippetUse(ReaderNonSnippetUseKind),
    MissingSnippet,
    InvalidNamespaceSnippet,
    InvalidReaderFunctionSnippet,
    RuntimeGlobalWrite,
    ClosureEscapes,
    MissingFreeBindingIndex,
    ReadsOtherMovableBinding,
    ReadsNonRuntimeBinding,
    OwnerSourceMissing,
    OwnerNameConflict,
}

/// Why a reader cluster's "non-snippet use" guard fired. Surfaced as a
/// sub-reason next to `RuntimeSetterMigrationBlockerReason::ReaderNonSnippetUse`
/// so guard-relaxation work can tell apart benign size-cap rejections from
/// cycle-correctness rejections without touching the rule itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderNonSnippetUseKind {
    /// Cluster's seed snippet set already exceeds the source-line cap.
    SeedClusterSizeCap,
    /// A queued reader is itself in `folded_runtime_definitions` — moving
    /// it would re-fold something already lowered.
    ReaderAlreadyFolded,
    /// After expansion, accumulated moved source lines exceed the cap.
    ExpandedClusterSizeCap,
    /// `folded_runtime_deps` non-empty and the cluster source-line tally
    /// exceeds the lower folded-deps cap.
    FoldedDepClusterSizeCap,
    /// One of the primary bindings has a folded non-snippet use that the
    /// movability heuristic can't accommodate.
    PrimaryBindingNonSnippetUse,
    /// Lazy-init cycle: a folded chunk in the runtime helper still calls
    /// a moved reader, and the writer module already imports the same
    /// runtime helper, so migration would create `runtime → writer →
    /// runtime`.
    LazyInitCycleImport,
    /// Reader is the bundle entrypoint's callee — migrating it would
    /// orphan the entrypoint's call site.
    UnfoldableEntrypointCallee,
    /// Reader's non-snippet runtime read is also referenced by the
    /// entrypoint — moving the reader detaches the entrypoint from the
    /// folded chunk that satisfies that read.
    UnfoldableEntrypointNonSnippetRead,
    /// Reader is the target of a namespace object binding (its identity
    /// is observed by namespace exports). Moving it would break the
    /// namespace surface.
    UnfoldableNamespaceObject,
    /// Reader is a namespace export helper (wires up `__copyProps`/
    /// `__esm`-style namespace re-exports). Moving breaks the wire-up.
    UnfoldableNamespaceExportHelper,
    /// Reader is already in `folded_runtime_definitions` — its
    /// definition lives in a folded chunk, can't migrate again.
    UnfoldableAlreadyFolded,
    /// Reader has a non-snippet runtime read that hasn't been folded
    /// into any runtime chunk. The canonical "real usage outside our
    /// region" — runtime code reads this binding from a site we don't
    /// control.
    UnfoldableUnfoldedNonSnippetRead,
}

impl ReaderNonSnippetUseKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SeedClusterSizeCap => "seed_cluster_size_cap",
            Self::ReaderAlreadyFolded => "reader_already_folded",
            Self::ExpandedClusterSizeCap => "expanded_cluster_size_cap",
            Self::FoldedDepClusterSizeCap => "folded_dep_cluster_size_cap",
            Self::PrimaryBindingNonSnippetUse => "primary_binding_non_snippet_use",
            Self::LazyInitCycleImport => "lazy_init_cycle_import",
            Self::UnfoldableEntrypointCallee => "unfoldable_entrypoint_callee",
            Self::UnfoldableEntrypointNonSnippetRead => "unfoldable_entrypoint_non_snippet_read",
            Self::UnfoldableNamespaceObject => "unfoldable_namespace_object",
            Self::UnfoldableNamespaceExportHelper => "unfoldable_namespace_export_helper",
            Self::UnfoldableAlreadyFolded => "unfoldable_already_folded",
            Self::UnfoldableUnfoldedNonSnippetRead => "unfoldable_unfolded_non_snippet_read",
        }
    }
}

/// Determine which condition of `runtime_reader_folded_non_snippet_use_can_move`
/// caused it to return `false` for this reader. Caller must have already
/// verified `runtime_binding_has_blocking_non_snippet_use` is true and
/// `can_move` is false; this picks the most specific reason in priority
/// order (entrypoint > namespace > folded > unfolded read).
pub(crate) fn classify_unfoldable_non_snippet_use(
    ctx: &RuntimeReaderClusterContext<'_>,
    reader: &BindingName,
) -> ReaderNonSnippetUseKind {
    if ctx.read_index.entrypoint_callee.as_ref() == Some(reader) {
        return ReaderNonSnippetUseKind::UnfoldableEntrypointCallee;
    }
    if ctx
        .read_index
        .entrypoint_non_snippet_runtime_reads
        .contains(reader)
    {
        return ReaderNonSnippetUseKind::UnfoldableEntrypointNonSnippetRead;
    }
    if ctx
        .read_index
        .namespace_exports_by_namespace
        .contains_key(reader)
    {
        return ReaderNonSnippetUseKind::UnfoldableNamespaceObject;
    }
    if ctx.read_index.namespace_export_helpers.contains(reader) {
        return ReaderNonSnippetUseKind::UnfoldableNamespaceExportHelper;
    }
    if ctx.folded_runtime_definitions.contains(reader) {
        return ReaderNonSnippetUseKind::UnfoldableAlreadyFolded;
    }
    // Caller has has_blocking==true → reader is in non_snippet_runtime_reads,
    // and can_move==false → reader is NOT in folded_non_snippet_runtime_reads
    // after the higher-priority filters. So this is the canonical "real
    // usage outside our region" case.
    ReaderNonSnippetUseKind::UnfoldableUnfoldedNonSnippetRead
}

/// Returns true when `reader` is a namespace target whose contributors
/// and helper are all reachable from `owner_module` — either as movable
/// runtime vars targeting the same owner, or as source bindings the
/// owner can already import by name. The namespace itself then lands in
/// owner, observing identities the owner can supply directly.
/// Returns true when the bundle entrypoint callee is reachable from
/// `owner_module` post-migration — either as a same-owner movable
/// runtime var, an owner-local binding, or a source-defined binding the
/// owner can import. The helper file then invokes the callee via ESM
/// import; the lazy-init cycle guard (LazyInitCycleImport) blocks any
/// unsafe runtime→owner→runtime cycle this could synthesize.
pub(crate) fn can_co_migrate_entrypoint_callee(
    ctx: &RuntimeReaderClusterContext<'_>,
    owner_module: ModuleId,
    reader: &BindingName,
) -> bool {
    if ctx.read_index.entrypoint_callee.as_ref() != Some(reader) {
        return false;
    }
    namespace_contributor_resolvable_in_owner(ctx, owner_module, reader)
}

pub(crate) fn can_co_migrate_namespace_target(
    ctx: &RuntimeReaderClusterContext<'_>,
    owner_module: ModuleId,
    reader: &BindingName,
) -> bool {
    let Some(namespace_export) = ctx.read_index.namespace_exports_by_namespace.get(reader) else {
        return false;
    };
    for contributor in namespace_export.exports.values() {
        if contributor == reader {
            continue;
        }
        if !namespace_contributor_resolvable_in_owner(ctx, owner_module, contributor) {
            return false;
        }
    }
    let helper = &namespace_export.helper;
    if helper == reader {
        return true;
    }
    namespace_contributor_resolvable_in_owner(ctx, owner_module, helper)
}

pub(crate) fn namespace_contributor_resolvable_in_owner(
    ctx: &RuntimeReaderClusterContext<'_>,
    owner_module: ModuleId,
    binding: &BindingName,
) -> bool {
    // Same-owner candidate runtime var — will migrate together with the cluster.
    if ctx
        .candidate_owners
        .get(binding)
        .is_some_and(|other| *other == owner_module)
    {
        return true;
    }
    // Already declared/imported in the owner module — importable by name.
    if ctx
        .owner_available_bindings
        .get(&owner_module)
        .is_some_and(|set| set.contains(binding))
    {
        return true;
    }
    // Defined by some other source module — owner can `import { x } from ...`.
    if matches!(
        ctx.all_source_definition_modules.get(binding),
        Some(Some(_))
    ) {
        return true;
    }
    false
}

/// Push the contributor bindings + helper of `reader`'s namespace export
/// onto the cluster walker's queue so they migrate together with the
/// namespace target itself. Already-moved bindings are skipped.
pub(crate) fn enqueue_namespace_co_migration(
    ctx: &RuntimeReaderClusterContext<'_>,
    reader: &BindingName,
    moved_snippets: &BTreeSet<BindingName>,
    queue: &mut Vec<BindingName>,
) {
    let Some(namespace_export) = ctx.read_index.namespace_exports_by_namespace.get(reader) else {
        return;
    };
    for contributor in namespace_export.exports.values() {
        if contributor == reader || moved_snippets.contains(contributor) {
            continue;
        }
        queue.push(contributor.clone());
    }
    if &namespace_export.helper != reader && !moved_snippets.contains(&namespace_export.helper) {
        queue.push(namespace_export.helper.clone());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeReaderClusterMigration {
    /// Primary runtime vars that can move as one same-writer component.
    /// This always includes the binding that seeded the cluster; it may
    /// also include other movable vars read by the same reader closure
    /// when they have the same owner module.
    pub(crate) primary_bindings: BTreeSet<BindingName>,
    pub(crate) extra_snippets: BTreeSet<BindingName>,
    pub(crate) extra_namespace_exports: BTreeSet<BindingName>,
    pub(crate) extra_runtime_deps: BTreeSet<BindingName>,
    pub(crate) extra_runtime_setter_deps: BTreeSet<BindingName>,
    pub(crate) extra_runtime_dep_aliases: BTreeMap<BindingName, BindingName>,
    /// Runtime deps that must explicitly stay owned by runtime because moving
    /// them to their writer would introduce a source-module cycle through this
    /// migrated reader's owner->runtime import.
    pub(crate) pinned_runtime_deps: BTreeSet<BindingName>,
    pub(crate) extra_source_deps: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    pub(crate) extra_runtime_reexport_source_deps: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    pub(crate) extra_noop_deps: BTreeSet<BindingName>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeReaderClusterMigrationProposal {
    pub(crate) seed_binding: BindingName,
    pub(crate) owner_module: ModuleId,
    pub(crate) source_lines: usize,
    pub(crate) migration: RuntimeReaderClusterMigration,
}

impl RuntimeReaderClusterMigrationProposal {
    pub(crate) fn estimated_runtime_savings(&self) -> usize {
        self.source_lines + self.migration.primary_bindings.len() * 4
    }
}

type RuntimeReaderClusterResult =
    Result<RuntimeReaderClusterMigration, RuntimeReaderClusterBlocker>;

const MAX_FOLDED_RUNTIME_DEP_READER_CLUSTER_LINES: usize = 200;
/// Floor for the reader-cluster line cap. `runtime_reader_cluster_cap_for_owner`
/// raises this proportionally for owners whose own source is large, but
/// never lowers it below this floor.
const MAX_RUNTIME_READER_MIGRATION_CLUSTER_LINES: usize = 10000;

pub(crate) fn merge_same_owner_overlapping_reader_migrations(
    ctx: &RuntimeReaderClusterContext<'_>,
    proposals: Vec<RuntimeReaderClusterMigrationProposal>,
) -> Vec<RuntimeReaderClusterMigrationProposal> {
    if proposals.len() < 2 {
        return proposals;
    }
    let proposal_local_bindings = proposals
        .iter()
        .map(|proposal| reader_migration_local_bindings(&proposal.migration))
        .collect::<Vec<_>>();
    let mut merged = Vec::<RuntimeReaderClusterMigrationProposal>::new();
    let mut assigned = vec![false; proposals.len()];

    for start in 0..proposals.len() {
        if assigned[start] {
            continue;
        }
        assigned[start] = true;
        let owner_module = proposals[start].owner_module;
        let mut component = vec![start];
        let mut component_local = proposal_local_bindings[start].clone();
        let mut changed = true;
        while changed {
            changed = false;
            for index in 0..proposals.len() {
                if assigned[index] || proposals[index].owner_module != owner_module {
                    continue;
                }
                if component_local
                    .intersection(&proposal_local_bindings[index])
                    .next()
                    .is_none()
                {
                    continue;
                }
                assigned[index] = true;
                component.push(index);
                component_local.extend(proposal_local_bindings[index].iter().cloned());
                changed = true;
            }
        }

        if component.len() == 1 {
            merged.push(proposals[start].clone());
            continue;
        }

        let Some(proposal) =
            merge_reader_migration_component(ctx, &proposals, &component, owner_module)
        else {
            for index in component {
                merged.push(proposals[index].clone());
            }
            continue;
        };
        merged.push(proposal);
    }

    merged
}

pub(crate) fn sort_reader_migration_proposals_by_preference(
    proposals: &mut [RuntimeReaderClusterMigrationProposal],
) {
    proposals.sort_by(|left, right| {
        // Primary vars are the actual setter-removal unit. Closure snippets can
        // be large, so primary coverage wins before estimated runtime-size
        // savings. The selector below preserves this order as its deterministic
        // tie-breaker after it has optimized whole conflict components.
        right
            .migration
            .primary_bindings
            .len()
            .cmp(&left.migration.primary_bindings.len())
            .then_with(|| {
                right
                    .estimated_runtime_savings()
                    .cmp(&left.estimated_runtime_savings())
            })
            .then_with(|| left.source_lines.cmp(&right.source_lines))
            .then_with(|| left.seed_binding.cmp(&right.seed_binding))
            .then_with(|| left.owner_module.cmp(&right.owner_module))
    });
}

pub(crate) fn select_non_conflicting_reader_migration_proposals(
    proposals: &[RuntimeReaderClusterMigrationProposal],
) -> Vec<RuntimeReaderClusterMigrationProposal> {
    if proposals.len() < 2 {
        return proposals.to_vec();
    }
    let conflicts = reader_migration_conflict_graph(proposals);
    let components = reader_migration_conflict_components(&conflicts);
    let mut selected_indices = Vec::<usize>::new();
    for component in components {
        if component.len() == 1 {
            selected_indices.push(component[0]);
        } else if component.len() <= MAX_EXACT_READER_MIGRATION_CONFLICT_COMPONENT {
            selected_indices.extend(select_exact_reader_migration_component(
                proposals, &conflicts, &component,
            ));
        } else {
            selected_indices.extend(select_large_reader_migration_component(
                proposals, &conflicts, &component,
            ));
        }
    }
    selected_indices.sort_unstable();
    selected_indices
        .into_iter()
        .map(|index| proposals[index].clone())
        .collect()
}

// Exact maximum-independent-set search is exponential. Keep it for genuinely
// small reader-conflict components and use the deterministic large-component
// selector beyond that bound so real bundle planning does not spend minutes
// exhaustively exploring a dense overlap cluster.
const MAX_EXACT_READER_MIGRATION_CONFLICT_COMPONENT: usize = 24;

pub(crate) fn reader_migration_conflict_graph(
    proposals: &[RuntimeReaderClusterMigrationProposal],
) -> Vec<BTreeSet<usize>> {
    let mut conflicts = vec![BTreeSet::<usize>::new(); proposals.len()];
    for left in 0..proposals.len() {
        for right in left + 1..proposals.len() {
            if reader_migration_proposals_conflict(&proposals[left], &proposals[right]) {
                conflicts[left].insert(right);
                conflicts[right].insert(left);
            }
        }
    }
    conflicts
}

pub(crate) fn reader_migration_proposals_conflict(
    left: &RuntimeReaderClusterMigrationProposal,
    right: &RuntimeReaderClusterMigrationProposal,
) -> bool {
    let left_migration = &left.migration;
    let right_migration = &right.migration;
    if left_migration
        .primary_bindings
        .intersection(&right_migration.primary_bindings)
        .next()
        .is_some()
        || left_migration
            .primary_bindings
            .intersection(&right_migration.pinned_runtime_deps)
            .next()
            .is_some()
        || right_migration
            .primary_bindings
            .intersection(&left_migration.pinned_runtime_deps)
            .next()
            .is_some()
    {
        return true;
    }
    if left_migration.primary_bindings.iter().any(|primary| {
        right_migration
            .extra_runtime_dep_aliases
            .contains_key(primary)
    }) || right_migration.primary_bindings.iter().any(|primary| {
        left_migration
            .extra_runtime_dep_aliases
            .contains_key(primary)
    }) {
        return true;
    }
    if left.owner_module == right.owner_module {
        return false;
    }
    let left_readers = left_migration
        .extra_snippets
        .iter()
        .chain(left_migration.extra_namespace_exports.iter())
        .collect::<BTreeSet<_>>();
    right_migration
        .extra_snippets
        .iter()
        .chain(right_migration.extra_namespace_exports.iter())
        .any(|reader| left_readers.contains(reader))
}

pub(crate) fn reader_migration_conflict_components(
    conflicts: &[BTreeSet<usize>],
) -> Vec<Vec<usize>> {
    let mut components = Vec::<Vec<usize>>::new();
    let mut seen = vec![false; conflicts.len()];
    for start in 0..conflicts.len() {
        if seen[start] {
            continue;
        }
        seen[start] = true;
        let mut stack = vec![start];
        let mut component = Vec::<usize>::new();
        while let Some(index) = stack.pop() {
            component.push(index);
            for next in &conflicts[index] {
                if !seen[*next] {
                    seen[*next] = true;
                    stack.push(*next);
                }
            }
        }
        component.sort_unstable();
        components.push(component);
    }
    components
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReaderMigrationSelectionScore {
    primary_bindings: usize,
    estimated_runtime_savings: usize,
    source_lines: usize,
}

impl ReaderMigrationSelectionScore {
    fn add_proposal(&mut self, proposal: &RuntimeReaderClusterMigrationProposal) {
        self.primary_bindings += proposal.migration.primary_bindings.len();
        self.estimated_runtime_savings += proposal.estimated_runtime_savings();
        self.source_lines += proposal.source_lines;
    }
}

pub(crate) fn reader_migration_selection_score(
    proposals: &[RuntimeReaderClusterMigrationProposal],
    indices: &[usize],
) -> ReaderMigrationSelectionScore {
    let mut score = ReaderMigrationSelectionScore::default();
    for index in indices {
        score.add_proposal(&proposals[*index]);
    }
    score
}

pub(crate) fn reader_migration_score_is_better(
    candidate: ReaderMigrationSelectionScore,
    incumbent: ReaderMigrationSelectionScore,
) -> bool {
    candidate.primary_bindings > incumbent.primary_bindings
        || candidate.primary_bindings == incumbent.primary_bindings
            && (candidate.estimated_runtime_savings > incumbent.estimated_runtime_savings
                || candidate.estimated_runtime_savings == incumbent.estimated_runtime_savings
                    && candidate.source_lines < incumbent.source_lines)
}

pub(crate) fn select_exact_reader_migration_component(
    proposals: &[RuntimeReaderClusterMigrationProposal],
    conflicts: &[BTreeSet<usize>],
    component: &[usize],
) -> Vec<usize> {
    debug_assert!(component.len() <= MAX_EXACT_READER_MIGRATION_CONFLICT_COMPONENT);
    let local_index = component
        .iter()
        .enumerate()
        .map(|(local, global)| (*global, local))
        .collect::<BTreeMap<_, _>>();
    let mut conflict_masks = vec![0u64; component.len()];
    for (local, global) in component.iter().enumerate() {
        for conflict in &conflicts[*global] {
            if let Some(conflict_local) = local_index.get(conflict) {
                conflict_masks[local] |= 1u64 << conflict_local;
            }
        }
    }
    let mut suffix_primary_bindings = vec![0usize; component.len() + 1];
    let mut suffix_runtime_savings = vec![0usize; component.len() + 1];
    for index in (0..component.len()).rev() {
        let proposal = &proposals[component[index]];
        suffix_primary_bindings[index] =
            suffix_primary_bindings[index + 1] + proposal.migration.primary_bindings.len();
        suffix_runtime_savings[index] =
            suffix_runtime_savings[index + 1] + proposal.estimated_runtime_savings();
    }

    let mut search = ReaderMigrationExactSearch {
        proposals,
        component,
        conflict_masks: &conflict_masks,
        suffix_primary_bindings: &suffix_primary_bindings,
        suffix_runtime_savings: &suffix_runtime_savings,
        best_score: ReaderMigrationSelectionScore::default(),
        best_selected: Vec::new(),
    };
    search.visit(
        0,
        0,
        ReaderMigrationSelectionScore::default(),
        &mut Vec::new(),
    );
    search.best_selected
}

pub(crate) struct ReaderMigrationExactSearch<'a> {
    proposals: &'a [RuntimeReaderClusterMigrationProposal],
    component: &'a [usize],
    conflict_masks: &'a [u64],
    suffix_primary_bindings: &'a [usize],
    suffix_runtime_savings: &'a [usize],
    best_score: ReaderMigrationSelectionScore,
    best_selected: Vec<usize>,
}

impl ReaderMigrationExactSearch<'_> {
    fn visit(
        &mut self,
        position: usize,
        blocked_mask: u64,
        score: ReaderMigrationSelectionScore,
        selected: &mut Vec<usize>,
    ) {
        if position == self.component.len() {
            if reader_migration_score_is_better(score, self.best_score) {
                self.best_score = score;
                self.best_selected = selected.clone();
            }
            return;
        }
        if score.primary_bindings + self.suffix_primary_bindings[position]
            < self.best_score.primary_bindings
        {
            return;
        }
        if score.primary_bindings + self.suffix_primary_bindings[position]
            == self.best_score.primary_bindings
            && score.estimated_runtime_savings + self.suffix_runtime_savings[position]
                < self.best_score.estimated_runtime_savings
        {
            return;
        }

        let local_bit = 1u64 << position;
        if blocked_mask & local_bit == 0 {
            let global_index = self.component[position];
            let proposal = &self.proposals[global_index];
            let mut include_score = score;
            include_score.add_proposal(proposal);
            selected.push(global_index);
            self.visit(
                position + 1,
                blocked_mask | local_bit | self.conflict_masks[position],
                include_score,
                selected,
            );
            selected.pop();
        }
        self.visit(position + 1, blocked_mask | local_bit, score, selected);
    }
}

pub(crate) fn select_large_reader_migration_component(
    proposals: &[RuntimeReaderClusterMigrationProposal],
    conflicts: &[BTreeSet<usize>],
    component: &[usize],
) -> Vec<usize> {
    let mut selected = Vec::<usize>::new();
    for index in component {
        if selected
            .iter()
            .all(|selected_index| !conflicts[*index].contains(selected_index))
        {
            selected.push(*index);
        }
    }

    let mut changed = true;
    while changed {
        changed = false;
        for candidate in component {
            if selected.contains(candidate) {
                continue;
            }
            let conflicting_selected = selected
                .iter()
                .copied()
                .filter(|selected_index| conflicts[*candidate].contains(selected_index))
                .collect::<Vec<_>>();
            if conflicting_selected.is_empty() {
                selected.push(*candidate);
                changed = true;
                continue;
            }
            let outside_conflicts = selected.iter().any(|selected_index| {
                !conflicting_selected.contains(selected_index)
                    && conflicts[*candidate].contains(selected_index)
            });
            if outside_conflicts {
                continue;
            }
            let candidate_score = reader_migration_selection_score(proposals, &[*candidate]);
            let conflicting_score =
                reader_migration_selection_score(proposals, &conflicting_selected);
            if !reader_migration_score_is_better(candidate_score, conflicting_score) {
                continue;
            }
            selected.retain(|selected_index| !conflicting_selected.contains(selected_index));
            selected.push(*candidate);
            selected.sort_unstable();
            changed = true;
        }
    }
    selected
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalizedRuntimeSetterDep {
    owner_module: ModuleId,
    initializer: Option<String>,
}

pub(crate) fn localize_reader_runtime_setter_deps(
    ctx: &RuntimeReaderClusterContext<'_>,
    proposals: &mut [RuntimeReaderClusterMigrationProposal],
) -> BTreeMap<BindingName, LocalizedRuntimeSetterDep> {
    let mut selected_primary_bindings = BTreeSet::<BindingName>::new();
    let mut moved_readers_by_owner = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
    let mut setter_dep_owners = BTreeMap::<BindingName, BTreeSet<ModuleId>>::new();
    let mut pinned_runtime_dep_owners = BTreeMap::<BindingName, BTreeSet<ModuleId>>::new();
    let mut aliased_runtime_deps = BTreeSet::<BindingName>::new();

    for proposal in proposals.iter() {
        selected_primary_bindings.extend(proposal.migration.primary_bindings.iter().cloned());
        moved_readers_by_owner
            .entry(proposal.owner_module)
            .or_default()
            .extend(
                proposal
                    .migration
                    .extra_snippets
                    .iter()
                    .chain(proposal.migration.extra_namespace_exports.iter())
                    .cloned(),
            );
        for dep in &proposal.migration.extra_runtime_setter_deps {
            setter_dep_owners
                .entry(dep.clone())
                .or_default()
                .insert(proposal.owner_module);
        }
        for dep in &proposal.migration.pinned_runtime_deps {
            pinned_runtime_dep_owners
                .entry(dep.clone())
                .or_default()
                .insert(proposal.owner_module);
        }
        aliased_runtime_deps.extend(proposal.migration.extra_runtime_dep_aliases.keys().cloned());
    }

    let mut localized = BTreeMap::<BindingName, LocalizedRuntimeSetterDep>::new();
    for (dep, owners) in setter_dep_owners {
        let Some(owner_module) = owners.iter().next().copied() else {
            continue;
        };
        if owners.len() != 1
            || selected_primary_bindings.contains(&dep)
            || aliased_runtime_deps.contains(&dep)
            || (ctx.movable_bindings.contains(&dep)
                && ctx
                    .candidate_owners
                    .get(&dep)
                    .is_none_or(|dep_owner| *dep_owner != owner_module))
            || pinned_runtime_dep_owners
                .get(&dep)
                .is_some_and(|owners| owners.iter().any(|owner| *owner != owner_module))
        {
            continue;
        }
        let moved_readers = moved_readers_by_owner
            .get(&owner_module)
            .cloned()
            .unwrap_or_default();
        if !runtime_setter_dep_can_localize_with_reader_owner(
            ctx,
            owner_module,
            &dep,
            &moved_readers,
        ) {
            continue;
        }
        let Some(initializer) = migratable_runtime_var_initializer(ctx.prelude, &dep) else {
            continue;
        };
        localized.insert(
            dep,
            LocalizedRuntimeSetterDep {
                owner_module,
                initializer,
            },
        );
    }

    if localized.is_empty() {
        return localized;
    }
    for proposal in proposals.iter_mut() {
        let local_for_owner = localized
            .iter()
            .filter_map(|(dep, localized)| {
                (localized.owner_module == proposal.owner_module).then_some(dep.clone())
            })
            .collect::<Vec<_>>();
        if local_for_owner.is_empty() {
            continue;
        }
        for dep in local_for_owner {
            proposal.migration.extra_runtime_setter_deps.remove(&dep);
            proposal.migration.pinned_runtime_deps.remove(&dep);
        }
    }
    localized
}

pub(crate) fn runtime_setter_dep_can_localize_with_reader_owner(
    ctx: &RuntimeReaderClusterContext<'_>,
    owner_module: ModuleId,
    binding: &BindingName,
    moved_readers: &BTreeSet<BindingName>,
) -> bool {
    if runtime_binding_has_blocking_non_snippet_use(ctx.read_index, binding)
        || ctx
            .read_index
            .namespace_exports_by_namespace
            .contains_key(binding)
        || ctx.folded_runtime_definitions.contains(binding)
    {
        return false;
    }
    if ctx
        .source_consumers_by_runtime_binding
        .get(&(ctx.source_file_id, binding.clone()))
        .is_some_and(|consumers| {
            consumers.iter().any(|consumer| {
                *consumer != owner_module
                    && module_dependency_path_exists(
                        ctx.module_dependencies_by_owner,
                        owner_module,
                        *consumer,
                    )
            })
        })
    {
        return false;
    }
    if ctx
        .owner_available_bindings
        .get(&owner_module)
        .is_none_or(|available| available.contains(binding))
    {
        return false;
    }

    let runtime_readers = runtime_readers_for_binding(ctx.read_index, binding);
    if runtime_readers
        .iter()
        .any(|reader| !moved_readers.contains(reader))
    {
        return false;
    }
    let runtime_writers = ctx
        .read_index
        .snippet_writers_by_binding
        .get(binding)
        .cloned()
        .unwrap_or_default();
    !runtime_writers
        .iter()
        .any(|writer| !moved_readers.contains(writer))
}

pub(crate) fn reader_migration_local_bindings(
    migration: &RuntimeReaderClusterMigration,
) -> BTreeSet<BindingName> {
    migration
        .primary_bindings
        .iter()
        .chain(migration.extra_snippets.iter())
        .chain(migration.extra_namespace_exports.iter())
        .cloned()
        .collect()
}

pub(crate) fn merge_reader_migration_component(
    ctx: &RuntimeReaderClusterContext<'_>,
    proposals: &[RuntimeReaderClusterMigrationProposal],
    component: &[usize],
    owner_module: ModuleId,
) -> Option<RuntimeReaderClusterMigrationProposal> {
    let seed_binding = component
        .iter()
        .filter_map(|index| proposals.get(*index))
        .map(|proposal| proposal.seed_binding.clone())
        .min()?;
    let mut migration = RuntimeReaderClusterMigration {
        primary_bindings: BTreeSet::new(),
        extra_snippets: BTreeSet::new(),
        extra_namespace_exports: BTreeSet::new(),
        extra_runtime_deps: BTreeSet::new(),
        extra_runtime_setter_deps: BTreeSet::new(),
        extra_runtime_dep_aliases: BTreeMap::new(),
        pinned_runtime_deps: BTreeSet::new(),
        extra_source_deps: BTreeMap::new(),
        extra_runtime_reexport_source_deps: BTreeMap::new(),
        extra_noop_deps: BTreeSet::new(),
    };

    for index in component {
        let proposal = proposals.get(*index)?;
        migration
            .primary_bindings
            .extend(proposal.migration.primary_bindings.iter().cloned());
        migration
            .extra_snippets
            .extend(proposal.migration.extra_snippets.iter().cloned());
        migration
            .extra_namespace_exports
            .extend(proposal.migration.extra_namespace_exports.iter().cloned());
        migration
            .extra_runtime_deps
            .extend(proposal.migration.extra_runtime_deps.iter().cloned());
        migration
            .extra_runtime_setter_deps
            .extend(proposal.migration.extra_runtime_setter_deps.iter().cloned());
        migration
            .extra_runtime_dep_aliases
            .extend(proposal.migration.extra_runtime_dep_aliases.clone());
        migration
            .pinned_runtime_deps
            .extend(proposal.migration.pinned_runtime_deps.iter().cloned());
        for (module_id, bindings) in &proposal.migration.extra_source_deps {
            migration
                .extra_source_deps
                .entry(*module_id)
                .or_default()
                .extend(bindings.iter().cloned());
        }
        for (module_id, bindings) in &proposal.migration.extra_runtime_reexport_source_deps {
            migration
                .extra_runtime_reexport_source_deps
                .entry(*module_id)
                .or_default()
                .extend(bindings.iter().cloned());
        }
        migration
            .extra_noop_deps
            .extend(proposal.migration.extra_noop_deps.iter().cloned());
    }

    let local_bindings = reader_migration_local_bindings(&migration);
    if migration
        .pinned_runtime_deps
        .intersection(&local_bindings)
        .next()
        .is_some()
    {
        return None;
    }
    migration
        .extra_runtime_deps
        .retain(|dep| !local_bindings.contains(dep));
    migration
        .extra_runtime_setter_deps
        .retain(|dep| !local_bindings.contains(dep));

    Some(RuntimeReaderClusterMigrationProposal {
        seed_binding,
        owner_module,
        source_lines: runtime_reader_migration_source_lines(ctx, &migration),
        migration,
    })
}

impl From<RuntimeReaderClusterBlocker> for RuntimeSetterMigrationBlockerReason {
    fn from(reason: RuntimeReaderClusterBlocker) -> Self {
        match reason {
            RuntimeReaderClusterBlocker::NonSnippetUse(_) => Self::ReaderNonSnippetUse,
            RuntimeReaderClusterBlocker::MissingSnippet => Self::ReaderSnippetMissing,
            RuntimeReaderClusterBlocker::InvalidNamespaceSnippet
            | RuntimeReaderClusterBlocker::InvalidReaderFunctionSnippet => {
                Self::ReaderNotMovableShape
            }
            RuntimeReaderClusterBlocker::RuntimeGlobalWrite => Self::ReaderWritesRuntimeBinding,
            RuntimeReaderClusterBlocker::ClosureEscapes => Self::ReaderClosureEscapes,
            RuntimeReaderClusterBlocker::MissingFreeBindingIndex => {
                Self::ReaderFreeBindingIndexMissing
            }
            RuntimeReaderClusterBlocker::ReadsOtherMovableBinding => {
                Self::ReaderReadsOtherMovableBinding
            }
            RuntimeReaderClusterBlocker::ReadsNonRuntimeBinding => {
                Self::ReaderReadsNonRuntimeBinding
            }
            RuntimeReaderClusterBlocker::OwnerSourceMissing => Self::OwnerSourceMissing,
            RuntimeReaderClusterBlocker::OwnerNameConflict => Self::OwnerNameConflict,
        }
    }
}

impl RuntimeReaderClusterBlocker {
    /// Returns a static label for sub-reason analytics, when the blocker
    /// carries a sub-classification. `None` means the blocker is itself
    /// the leaf reason.
    fn sub_reason(self) -> Option<&'static str> {
        match self {
            Self::NonSnippetUse(kind) => Some(kind.as_str()),
            _ => None,
        }
    }
}

pub(crate) fn migratable_runtime_reader_cluster_result(
    ctx: &RuntimeReaderClusterContext<'_>,
    owner_module: ModuleId,
    binding: &BindingName,
    initial_readers: BTreeSet<BindingName>,
) -> RuntimeReaderClusterResult {
    let mut primary_bindings = BTreeSet::<BindingName>::from([binding.clone()]);
    if initial_readers.is_empty() {
        return Ok(RuntimeReaderClusterMigration {
            primary_bindings,
            extra_snippets: BTreeSet::new(),
            extra_namespace_exports: BTreeSet::new(),
            extra_runtime_deps: BTreeSet::new(),
            extra_runtime_setter_deps: BTreeSet::new(),
            extra_runtime_dep_aliases: BTreeMap::new(),
            pinned_runtime_deps: BTreeSet::new(),
            extra_source_deps: BTreeMap::new(),
            extra_runtime_reexport_source_deps: BTreeMap::new(),
            extra_noop_deps: BTreeSet::new(),
        });
    }
    let cluster_cap = runtime_reader_cluster_cap_for_owner(ctx, owner_module);
    if runtime_reader_cluster_source_lines(ctx, &initial_readers) > cluster_cap {
        return Err(RuntimeReaderClusterBlocker::NonSnippetUse(
            ReaderNonSnippetUseKind::SeedClusterSizeCap,
        ));
    }
    if let Some(entrypoint_callee) = &ctx.read_index.entrypoint_callee
        && initial_readers.contains(entrypoint_callee)
        && !can_co_migrate_entrypoint_callee(ctx, owner_module, entrypoint_callee)
    {
        return Err(RuntimeReaderClusterBlocker::NonSnippetUse(
            ReaderNonSnippetUseKind::UnfoldableEntrypointCallee,
        ));
    }
    if initial_readers.iter().any(|reader| {
        ctx.read_index
            .entrypoint_non_snippet_runtime_reads
            .contains(reader)
    }) {
        return Err(RuntimeReaderClusterBlocker::NonSnippetUse(
            ReaderNonSnippetUseKind::UnfoldableEntrypointNonSnippetRead,
        ));
    }

    let owner_available_bindings = owner_declared_or_imported_bindings(ctx, owner_module)?;
    let mut moved_snippets = BTreeSet::<BindingName>::new();
    let mut moved_namespace_exports = BTreeSet::<BindingName>::new();
    let mut extra_runtime_deps = BTreeSet::<BindingName>::new();
    let mut extra_runtime_setter_deps = BTreeSet::<BindingName>::new();
    let mut folded_runtime_deps = BTreeSet::<BindingName>::new();
    let mut folded_non_snippet_snippets = BTreeSet::<BindingName>::new();
    let mut pinned_runtime_deps = BTreeSet::<BindingName>::new();
    let mut extra_source_deps = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
    let mut extra_runtime_reexport_source_deps = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
    let mut extra_noop_deps = BTreeSet::<BindingName>::new();
    let mut moved_source_lines = 0usize;
    let mut queue = initial_readers.into_iter().collect::<Vec<_>>();
    while let Some(reader) = queue.pop() {
        if !moved_snippets.insert(reader.clone()) {
            continue;
        }
        if ctx.folded_runtime_definitions.contains(&reader) {
            return Err(RuntimeReaderClusterBlocker::NonSnippetUse(
                ReaderNonSnippetUseKind::ReaderAlreadyFolded,
            ));
        }
        if runtime_binding_has_blocking_non_snippet_use(ctx.read_index, &reader) {
            if !runtime_reader_folded_non_snippet_use_can_move(ctx, &reader) {
                let detail = classify_unfoldable_non_snippet_use(ctx, &reader);
                // Phase 2: if the only thing blocking `can_move` is that
                // `reader` is itself a namespace target, try to co-migrate
                // the namespace: enqueue its contributors + helper so they
                // all land in the same owner. Only safe when every
                // contributor is itself movable (otherwise the namespace
                // would observe stale identities post-migration).
                if detail == ReaderNonSnippetUseKind::UnfoldableNamespaceObject
                    && can_co_migrate_namespace_target(ctx, owner_module, &reader)
                {
                    enqueue_namespace_co_migration(ctx, &reader, &moved_snippets, &mut queue);
                    folded_non_snippet_snippets.insert(reader.clone());
                } else if detail == ReaderNonSnippetUseKind::UnfoldableEntrypointCallee
                    && can_co_migrate_entrypoint_callee(ctx, owner_module, &reader)
                {
                    // Phase 3: the bundle entrypoint callee can co-migrate
                    // when its definition is reachable from the cluster's
                    // owner (either by being a same-owner movable binding
                    // or by being importable from a source module). The
                    // runtime helper's `__entry()` call site keeps working
                    // because the helper imports the callee from its new
                    // home; the lazy-init cycle guard catches any unsafe
                    // helper→owner→helper cycle that would result.
                    folded_non_snippet_snippets.insert(reader.clone());
                } else {
                    return Err(RuntimeReaderClusterBlocker::NonSnippetUse(detail));
                }
            } else {
                folded_non_snippet_snippets.insert(reader.clone());
            }
        }
        let snippet = ctx
            .prelude
            .snippets
            .get(&reader)
            .ok_or(RuntimeReaderClusterBlocker::MissingSnippet)?;
        moved_source_lines += snippet.source.lines().count().max(1);
        if moved_source_lines > cluster_cap {
            return Err(RuntimeReaderClusterBlocker::NonSnippetUse(
                ReaderNonSnippetUseKind::ExpandedClusterSizeCap,
            ));
        }
        let is_namespace_reader = ctx
            .read_index
            .namespace_exports_by_namespace
            .contains_key(&reader);
        if is_namespace_reader {
            if !is_migratable_namespace_reader_snippet(&reader, snippet.source.as_str()) {
                return Err(RuntimeReaderClusterBlocker::InvalidNamespaceSnippet);
            }
            moved_namespace_exports.insert(reader.clone());
        } else if !is_migratable_reader_function_snippet(&reader, snippet.source.as_str()) {
            return Err(RuntimeReaderClusterBlocker::InvalidReaderFunctionSnippet);
        }
        // A moved reader may initialize/update the primary runtime binding (or
        // another same-owner movable binding) as long as that binding moves
        // with the same cluster. If the written runtime binding must stay in
        // the helper file, keep the reader movable only when the write can be
        // lowered to a setter call; direct assignment to an ESM import would be
        // illegal after migration.
        let runtime_writes = ctx
            .read_index
            .runtime_writes_by_snippet
            .get(&reader)
            .cloned()
            .unwrap_or_default();
        let mut local_bindings = None;
        for write in runtime_writes {
            if primary_bindings.contains(&write) {
                continue;
            }
            if ctx.movable_bindings.contains(&write)
                && ctx
                    .candidate_owners
                    .get(&write)
                    .is_some_and(|write_owner| *write_owner == owner_module)
            {
                match runtime_binding_read_profile_diagnostic(ctx.read_index, &write) {
                    Ok(RuntimeBindingReadProfile::NoReads) => {
                        primary_bindings.insert(write.clone());
                        continue;
                    }
                    Ok(RuntimeBindingReadProfile::SnippetReaders(write_readers)) => {
                        if primary_bindings.insert(write.clone()) {
                            for write_reader in write_readers {
                                if !moved_snippets.contains(&write_reader) {
                                    queue.push(write_reader);
                                }
                            }
                        }
                        continue;
                    }
                    Ok(RuntimeBindingReadProfile::Rejected) | Err(_) => {}
                }
            }
            let local_bindings = local_bindings
                .get_or_insert_with(|| local_bindings_in_source(snippet.source.as_str()));
            if runtime_reader_write_can_use_setter(snippet.source.as_str(), local_bindings, &write)
            {
                extra_runtime_setter_deps.insert(write.clone());
                if ctx.movable_bindings.contains(&write) {
                    pinned_runtime_deps.insert(write.clone());
                }
                continue;
            }
            return Err(RuntimeReaderClusterBlocker::RuntimeGlobalWrite);
        }

        // Any runtime snippet that still reads this newly moved function would
        // force runtime -> owner imports. Pull function dependents into the same
        // cluster instead; reject later if one of them is not a simple function
        // declaration.
        for dependent in runtime_readers_for_binding(ctx.read_index, &reader) {
            if dependent != reader && !moved_snippets.contains(&dependent) {
                queue.push(dependent);
            }
        }

        let free_runtime_bindings = ctx
            .read_index
            .free_bindings_by_snippet
            .get(&reader)
            .ok_or(RuntimeReaderClusterBlocker::MissingFreeBindingIndex)?;
        for dep in free_runtime_bindings {
            if primary_bindings.contains(dep) || moved_snippets.contains(dep) {
                continue;
            }
            if ctx.movable_bindings.contains(dep) {
                let Some(dep_owner) = ctx.candidate_owners.get(dep) else {
                    return Err(RuntimeReaderClusterBlocker::ReadsOtherMovableBinding);
                };
                if *dep_owner != owner_module {
                    if module_dependency_path_exists(
                        ctx.module_dependencies_by_owner,
                        *dep_owner,
                        owner_module,
                    ) {
                        extra_runtime_deps.insert(dep.clone());
                        pinned_runtime_deps.insert(dep.clone());
                        continue;
                    }
                    extra_runtime_deps.insert(dep.clone());
                    continue;
                }
                let dep_readers = match runtime_binding_read_profile_diagnostic(ctx.read_index, dep)
                {
                    Ok(RuntimeBindingReadProfile::NoReads) => BTreeSet::new(),
                    Ok(RuntimeBindingReadProfile::SnippetReaders(readers)) => readers,
                    Ok(RuntimeBindingReadProfile::Rejected) => {
                        extra_runtime_deps.insert(dep.clone());
                        pinned_runtime_deps.insert(dep.clone());
                        continue;
                    }
                    Err(_reason) => {
                        extra_runtime_deps.insert(dep.clone());
                        pinned_runtime_deps.insert(dep.clone());
                        continue;
                    }
                };
                if primary_bindings.insert(dep.clone()) {
                    for dep_reader in dep_readers {
                        if !moved_snippets.contains(&dep_reader) {
                            queue.push(dep_reader);
                        }
                    }
                }
                continue;
            }
            if !ctx.prelude.defines(dep) {
                if owner_available_bindings.contains(dep) {
                    continue;
                }
                if runtime_reader_folded_runtime_dependency(ctx, owner_module, dep) {
                    extra_runtime_deps.insert(dep.clone());
                    folded_runtime_deps.insert(dep.clone());
                    continue;
                }
                if let Some(dep_module) = runtime_reader_source_dependency(ctx, owner_module, dep) {
                    extra_source_deps
                        .entry(dep_module)
                        .or_default()
                        .insert(dep.clone());
                    continue;
                }
                if let Some(dep_module) =
                    runtime_reader_source_reexport_dependency(ctx, owner_module, dep)
                {
                    extra_runtime_reexport_source_deps
                        .entry(dep_module)
                        .or_default()
                        .insert(dep.clone());
                    continue;
                }
                if runtime_reader_externalized_package_init_dependency(
                    ctx,
                    snippet.source.as_str(),
                    dep,
                ) {
                    extra_noop_deps.insert(dep.clone());
                    continue;
                }
                return Err(RuntimeReaderClusterBlocker::ReadsNonRuntimeBinding);
            }
            if let Some(closure) =
                runtime_private_function_dependency_closure(ctx, owner_module, dep, &moved_snippets)
            {
                queue.extend(
                    closure
                        .into_iter()
                        .filter(|binding| !moved_snippets.contains(binding)),
                );
                continue;
            }
            extra_runtime_deps.insert(dep.clone());
        }
    }

    if !primary_bindings.iter().all(|primary| {
        runtime_readers_for_binding(ctx.read_index, primary)
            .into_iter()
            .all(|reader| moved_snippets.contains(&reader))
    }) {
        return Err(RuntimeReaderClusterBlocker::ClosureEscapes);
    }

    if !moved_snippets.iter().all(|snippet| {
        runtime_readers_for_binding(ctx.read_index, snippet)
            .into_iter()
            .all(|reader| reader == *snippet || moved_snippets.contains(&reader))
    }) {
        return Err(RuntimeReaderClusterBlocker::ClosureEscapes);
    }

    for namespace in &moved_namespace_exports {
        let namespace_export = ctx
            .read_index
            .namespace_exports_by_namespace
            .get(namespace)
            .ok_or(RuntimeReaderClusterBlocker::MissingSnippet)?;
        for dep in namespace_export.exports.values() {
            if primary_bindings.contains(dep) || moved_snippets.contains(dep) {
                continue;
            }
            if let Some(dep_owner) = ctx.candidate_owners.get(dep) {
                if *dep_owner != owner_module {
                    // The namespace object can still move to this cluster's
                    // owner when one of its getter targets belongs to a
                    // different writer. Keep that target as a runtime dep for
                    // now; if a later primary-only migration moves the target
                    // to its writer, `migrated_extra_runtime_deps_for_owner`
                    // rewires this owner to import it directly. When that
                    // would create an owner -> dep-owner -> owner cycle, pin
                    // the target in runtime instead of allowing the later
                    // primary-only migration.
                    extra_runtime_deps.insert(dep.clone());
                    if module_dependency_path_exists(
                        ctx.module_dependencies_by_owner,
                        *dep_owner,
                        owner_module,
                    ) {
                        pinned_runtime_deps.insert(dep.clone());
                    }
                    continue;
                }
                extra_runtime_deps.insert(dep.clone());
                continue;
            }
            if !ctx.prelude.defines(dep) {
                if owner_available_bindings.contains(dep) {
                    continue;
                }
                if runtime_reader_folded_runtime_dependency(ctx, owner_module, dep) {
                    extra_runtime_deps.insert(dep.clone());
                    folded_runtime_deps.insert(dep.clone());
                    continue;
                }
                if let Some(dep_module) = runtime_reader_source_dependency(ctx, owner_module, dep) {
                    extra_source_deps
                        .entry(dep_module)
                        .or_default()
                        .insert(dep.clone());
                    continue;
                }
                if let Some(dep_module) =
                    runtime_reader_source_reexport_dependency(ctx, owner_module, dep)
                {
                    extra_runtime_reexport_source_deps
                        .entry(dep_module)
                        .or_default()
                        .insert(dep.clone());
                    continue;
                }
                return Err(RuntimeReaderClusterBlocker::ReadsNonRuntimeBinding);
            }
            extra_runtime_deps.insert(dep.clone());
        }
    }

    if !extra_runtime_setter_deps.is_empty() {
        let setter_deps_read_after_rewrite = runtime_setter_deps_read_after_write_rewrite(
            ctx,
            &moved_snippets,
            &moved_namespace_exports,
            &extra_runtime_setter_deps,
        );
        extra_runtime_deps.retain(|dep| {
            !extra_runtime_setter_deps.contains(dep) || setter_deps_read_after_rewrite.contains(dep)
        });
        if runtime_setter_imports_conflict(
            &extra_runtime_setter_deps,
            &owner_available_bindings,
            &primary_bindings,
            &moved_snippets,
            &moved_namespace_exports,
            &extra_noop_deps,
        ) {
            return Err(RuntimeReaderClusterBlocker::OwnerNameConflict);
        }
    }

    let extra_runtime_dep_aliases = runtime_dep_aliases_for_owner_conflicts(
        ctx,
        &moved_snippets,
        &moved_namespace_exports,
        &extra_runtime_deps,
        &primary_bindings,
        &extra_noop_deps,
        &owner_available_bindings,
    )
    .ok_or(RuntimeReaderClusterBlocker::OwnerNameConflict)?;

    if moved_snippets
        .iter()
        .chain(
            extra_runtime_deps
                .iter()
                .filter(|dep| !extra_runtime_dep_aliases.contains_key(*dep)),
        )
        .chain(extra_runtime_reexport_source_deps.values().flatten())
        .chain(extra_noop_deps.iter())
        .chain(primary_bindings.iter())
        .any(|binding| owner_available_bindings.contains(binding))
    {
        return Err(RuntimeReaderClusterBlocker::OwnerNameConflict);
    }

    for primary in &primary_bindings {
        extra_runtime_deps.remove(primary);
    }

    if !folded_runtime_deps.is_empty()
        && runtime_reader_cluster_source_lines(ctx, &moved_snippets)
            > MAX_FOLDED_RUNTIME_DEP_READER_CLUSTER_LINES
    {
        return Err(RuntimeReaderClusterBlocker::NonSnippetUse(
            ReaderNonSnippetUseKind::FoldedDepClusterSizeCap,
        ));
    }
    let folded_non_snippet_primary_bindings = primary_bindings
        .iter()
        .filter(|binding| {
            ctx.read_index
                .folded_non_snippet_runtime_reads
                .contains(*binding)
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    if folded_non_snippet_primary_bindings
        .iter()
        .any(|binding| !runtime_reader_folded_non_snippet_use_can_move(ctx, binding))
    {
        return Err(RuntimeReaderClusterBlocker::NonSnippetUse(
            ReaderNonSnippetUseKind::PrimaryBindingNonSnippetUse,
        ));
    }

    // If a folded runtime chunk still calls a moved reader, the runtime helper
    // file will import that reader from the writer module. Only allow that
    // runtime -> writer edge when the writer no longer needs to import the same
    // runtime helper file: otherwise we would synthesize a brittle ESM cycle
    // (`runtime -> writer -> runtime`) during module evaluation.
    if !folded_non_snippet_snippets.is_empty() || !folded_non_snippet_primary_bindings.is_empty() {
        let mut locally_owned_runtime = primary_bindings.clone();
        locally_owned_runtime.extend(moved_snippets.iter().cloned());
        locally_owned_runtime.extend(moved_namespace_exports.iter().cloned());
        if !runtime_reader_cycle_imports_can_move(RuntimeReaderCycleImportGate {
            ctx,
            owner_module,
            primary_bindings: &primary_bindings,
            moved_snippets: &moved_snippets,
            moved_namespace_exports: &moved_namespace_exports,
            locally_owned_runtime: &locally_owned_runtime,
            extra_runtime_deps: &extra_runtime_deps,
            folded_runtime_deps: &folded_runtime_deps,
            extra_runtime_setter_deps: &extra_runtime_setter_deps,
            extra_runtime_reexport_source_deps: &extra_runtime_reexport_source_deps,
        })? {
            return Err(RuntimeReaderClusterBlocker::NonSnippetUse(
                ReaderNonSnippetUseKind::LazyInitCycleImport,
            ));
        }
    }

    Ok(RuntimeReaderClusterMigration {
        primary_bindings,
        extra_snippets: moved_snippets,
        extra_namespace_exports: moved_namespace_exports,
        extra_runtime_deps,
        extra_runtime_setter_deps,
        extra_runtime_dep_aliases,
        pinned_runtime_deps,
        extra_source_deps,
        extra_runtime_reexport_source_deps,
        extra_noop_deps,
    })
}

pub(crate) fn migratable_runtime_primary_with_retained_readers_result(
    ctx: &RuntimeReaderClusterContext<'_>,
    owner_module: ModuleId,
    binding: &BindingName,
) -> Option<RuntimeReaderClusterMigration> {
    if !retained_runtime_primary_readers_are_lazy_safe(ctx, binding) {
        return None;
    }
    let owner_available_bindings = owner_declared_or_imported_bindings(ctx, owner_module).ok()?;
    if owner_available_bindings.contains(binding) {
        return None;
    }
    let primary_bindings = BTreeSet::from([binding.clone()]);
    if !runtime_reader_cycle_imports_can_move(RuntimeReaderCycleImportGate {
        ctx,
        owner_module,
        primary_bindings: &primary_bindings,
        moved_snippets: &BTreeSet::new(),
        moved_namespace_exports: &BTreeSet::new(),
        locally_owned_runtime: &primary_bindings,
        extra_runtime_deps: &BTreeSet::new(),
        folded_runtime_deps: &BTreeSet::new(),
        extra_runtime_setter_deps: &BTreeSet::new(),
        extra_runtime_reexport_source_deps: &BTreeMap::new(),
    })
    .ok()?
    {
        return None;
    }
    Some(RuntimeReaderClusterMigration {
        primary_bindings,
        extra_snippets: BTreeSet::new(),
        extra_namespace_exports: BTreeSet::new(),
        extra_runtime_deps: BTreeSet::new(),
        extra_runtime_setter_deps: BTreeSet::new(),
        extra_runtime_dep_aliases: BTreeMap::new(),
        pinned_runtime_deps: BTreeSet::new(),
        extra_source_deps: BTreeMap::new(),
        extra_runtime_reexport_source_deps: BTreeMap::new(),
        extra_noop_deps: BTreeSet::new(),
    })
}

fn retained_runtime_primary_readers_are_lazy_safe(
    ctx: &RuntimeReaderClusterContext<'_>,
    binding: &BindingName,
) -> bool {
    runtime_readers_for_binding(ctx.read_index, binding)
        .into_iter()
        .all(|reader| {
            ctx.prelude.snippets.get(&reader).is_some_and(|snippet| {
                is_migratable_private_runtime_function_dependency(&reader, snippet.source.as_str())
            })
        })
}

pub(crate) fn owner_declared_or_imported_bindings(
    ctx: &RuntimeReaderClusterContext<'_>,
    owner_module: ModuleId,
) -> Result<BTreeSet<BindingName>, RuntimeReaderClusterBlocker> {
    ctx.owner_available_bindings
        .get(&owner_module)
        .cloned()
        .ok_or(RuntimeReaderClusterBlocker::OwnerSourceMissing)
}

pub(crate) fn runtime_reader_write_can_use_setter(
    source: &str,
    local_bindings: &BTreeSet<String>,
    binding: &BindingName,
) -> bool {
    let setter_name = runtime_helper_setter_name(binding);
    if local_bindings.contains(setter_name.as_str()) {
        return false;
    }
    let helpers = BTreeSet::from([binding.clone()]);
    let rewritten = rewrite_runtime_helper_writes(source, &helpers);
    if rewritten == source {
        return false;
    }
    !implicit_global_writes_in_source(rewritten.as_str())
        .into_iter()
        .any(|write| write == *binding)
}

pub(crate) fn runtime_setter_deps_read_after_write_rewrite(
    ctx: &RuntimeReaderClusterContext<'_>,
    moved_snippets: &BTreeSet<BindingName>,
    moved_namespace_exports: &BTreeSet<BindingName>,
    setter_deps: &BTreeSet<BindingName>,
) -> BTreeSet<BindingName> {
    let mut reads = BTreeSet::<BindingName>::new();
    for snippet in moved_snippets {
        let Some(source) = ctx
            .prelude
            .snippets
            .get(snippet)
            .map(|snippet| snippet.source.as_str())
        else {
            continue;
        };
        let rewritten = rewrite_runtime_helper_writes(source, setter_deps);
        reads.extend(
            runtime_import_identifiers_in_source(rewritten.as_str())
                .into_iter()
                .map(BindingName::new)
                .filter(|binding| setter_deps.contains(binding)),
        );
    }
    for namespace in moved_namespace_exports {
        let Some(namespace_export) = ctx.read_index.namespace_exports_by_namespace.get(namespace)
        else {
            continue;
        };
        reads.extend(
            runtime_import_identifiers_in_source(
                runtime_namespace_export_statement(namespace_export).as_str(),
            )
            .into_iter()
            .map(BindingName::new)
            .filter(|binding| setter_deps.contains(binding)),
        );
    }
    reads
}

pub(crate) fn runtime_setter_imports_conflict(
    setter_deps: &BTreeSet<BindingName>,
    owner_available_bindings: &BTreeSet<BindingName>,
    primary_bindings: &BTreeSet<BindingName>,
    moved_snippets: &BTreeSet<BindingName>,
    moved_namespace_exports: &BTreeSet<BindingName>,
    extra_noop_deps: &BTreeSet<BindingName>,
) -> bool {
    setter_deps.iter().any(|dep| {
        let setter = BindingName::new(runtime_helper_setter_name(dep));
        owner_available_bindings.contains(&setter)
            || primary_bindings.contains(&setter)
            || moved_snippets.contains(&setter)
            || moved_namespace_exports.contains(&setter)
            || extra_noop_deps.contains(&setter)
    })
}

pub(crate) fn runtime_setter_dep_cycle_is_safe(
    ctx: &RuntimeReaderClusterContext<'_>,
    setter_deps: &BTreeSet<BindingName>,
) -> bool {
    setter_deps
        .iter()
        .all(|dep| runtime_binding_is_bare_var_declaration(ctx.prelude, dep))
}

pub(crate) fn runtime_binding_is_bare_var_declaration(
    prelude: &RuntimePrelude,
    binding: &BindingName,
) -> bool {
    let Some(snippet) = prelude.snippets.get(binding) else {
        return false;
    };
    let trimmed = snippet.source.trim();
    let Some(rest) = trimmed.strip_prefix("var") else {
        return false;
    };
    if !rest.starts_with(|c: char| c.is_ascii_whitespace()) {
        return false;
    }
    let Some(rest) = rest.trim_start().strip_suffix(';') else {
        return false;
    };
    split_top_level_properties(rest)
        .into_iter()
        .any(|declarator| {
            let declarator = declarator.trim();
            let Some((name, next)) = parse_identifier(declarator, 0) else {
                return false;
            };
            name == binding.as_str() && skip_ws(declarator.as_bytes(), next) == declarator.len()
        })
}

pub(crate) fn migratable_folded_non_snippet_runtime_read_result(
    ctx: &RuntimeReaderClusterContext<'_>,
    owner_module: ModuleId,
    binding: &BindingName,
) -> Option<RuntimeReaderClusterMigration> {
    if !ctx
        .read_index
        .folded_non_snippet_runtime_reads
        .contains(binding)
        || ctx
            .read_index
            .entrypoint_non_snippet_runtime_reads
            .contains(binding)
        || ctx.read_index.namespace_export_helpers.contains(binding)
        || ctx
            .read_index
            .namespace_exports_by_namespace
            .contains_key(binding)
        || !runtime_readers_for_binding(ctx.read_index, binding).is_empty()
    {
        return None;
    }
    let primary_bindings = BTreeSet::from([binding.clone()]);
    if !runtime_reader_cycle_imports_can_move(RuntimeReaderCycleImportGate {
        ctx,
        owner_module,
        primary_bindings: &primary_bindings,
        moved_snippets: &BTreeSet::new(),
        moved_namespace_exports: &BTreeSet::new(),
        locally_owned_runtime: &primary_bindings,
        extra_runtime_deps: &BTreeSet::new(),
        folded_runtime_deps: &BTreeSet::new(),
        extra_runtime_setter_deps: &BTreeSet::new(),
        extra_runtime_reexport_source_deps: &BTreeMap::new(),
    })
    .ok()?
    {
        return None;
    }
    Some(RuntimeReaderClusterMigration {
        primary_bindings,
        extra_snippets: BTreeSet::new(),
        extra_namespace_exports: BTreeSet::new(),
        extra_runtime_deps: BTreeSet::new(),
        extra_runtime_setter_deps: BTreeSet::new(),
        extra_runtime_dep_aliases: BTreeMap::new(),
        pinned_runtime_deps: BTreeSet::new(),
        extra_source_deps: BTreeMap::new(),
        extra_runtime_reexport_source_deps: BTreeMap::new(),
        extra_noop_deps: BTreeSet::new(),
    })
}

pub(crate) fn runtime_reader_owner_available_bindings(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
    lowered_runtime_sources: &BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
    owners: impl IntoIterator<Item = ModuleId>,
) -> BTreeMap<ModuleId, BTreeSet<BindingName>> {
    let mut by_owner = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
    for owner_module in owners {
        if by_owner.contains_key(&owner_module) {
            continue;
        }
        let Some(owner_source) = lowered_runtime_sources.get(&owner_module) else {
            continue;
        };
        let mut bindings = owner_source.local_definitions.clone();
        bindings.extend(program.model().graph().ast_imports_for(owner_module));
        if let Some(imports_by_target) = source_module_wiring.imports_by_module.get(&owner_module) {
            for bindings_from_target in imports_by_target.values() {
                bindings.extend(bindings_from_target.iter().cloned());
            }
        }
        by_owner.insert(owner_module, bindings);
    }
    by_owner
}

pub(crate) fn owner_state_needs_shared_lazy_helper(
    owner_state: &RuntimeReaderOwnerRuntimeState,
) -> bool {
    owner_state.uses_lazy_module
        || (owner_state.uses_lazy_value && !owner_state.can_localize_lazy_value)
}

pub(crate) struct RuntimeReaderCycleImportGate<'a> {
    ctx: &'a RuntimeReaderClusterContext<'a>,
    owner_module: ModuleId,
    primary_bindings: &'a BTreeSet<BindingName>,
    moved_snippets: &'a BTreeSet<BindingName>,
    moved_namespace_exports: &'a BTreeSet<BindingName>,
    locally_owned_runtime: &'a BTreeSet<BindingName>,
    extra_runtime_deps: &'a BTreeSet<BindingName>,
    folded_runtime_deps: &'a BTreeSet<BindingName>,
    extra_runtime_setter_deps: &'a BTreeSet<BindingName>,
    extra_runtime_reexport_source_deps: &'a BTreeMap<ModuleId, BTreeSet<BindingName>>,
}

pub(crate) fn runtime_reader_cycle_imports_can_move(
    gate: RuntimeReaderCycleImportGate<'_>,
) -> Result<bool, RuntimeReaderClusterBlocker> {
    if !runtime_setter_dep_cycle_is_safe(gate.ctx, gate.extra_runtime_setter_deps)
        || !gate.extra_runtime_reexport_source_deps.is_empty()
    {
        return Ok(false);
    }
    let owner_state = gate
        .ctx
        .owner_runtime_state
        .get(&gate.owner_module)
        .ok_or(RuntimeReaderClusterBlocker::OwnerSourceMissing)?;

    let owner_runtime_deps = owner_state
        .remaining_helpers
        .iter()
        .filter(|helper| !gate.locally_owned_runtime.contains(*helper))
        .cloned()
        .chain(gate.extra_runtime_deps.iter().cloned())
        .chain(gate.folded_runtime_deps.iter().cloned())
        .collect::<BTreeSet<_>>();

    let self_contained_owner_runtime_is_safe = gate.extra_runtime_deps.is_empty()
        && gate.folded_runtime_deps.is_empty()
        && !owner_state_needs_shared_lazy_helper(owner_state)
        && owner_state
            .remaining_helpers
            .iter()
            .all(|helper| gate.locally_owned_runtime.contains(helper))
        && owner_state
            .written_helpers
            .iter()
            .all(|helper| gate.primary_bindings.contains(helper));
    if self_contained_owner_runtime_is_safe {
        return Ok(true);
    }

    if owner_state.uses_lazy_module
        || owner_state
            .written_helpers
            .iter()
            .any(|helper| !gate.primary_bindings.contains(helper))
    {
        return Ok(false);
    }
    if !owner_runtime_imports_are_lazy_safe(owner_state.source.as_str(), &owner_runtime_deps) {
        return Ok(false);
    }
    if !folded_runtime_reads_are_lazy_safe(
        gate.ctx,
        gate.primary_bindings,
        gate.moved_snippets,
        gate.moved_namespace_exports,
    ) {
        return Ok(false);
    }
    Ok(true)
}

pub(crate) fn owner_runtime_imports_are_lazy_safe(
    source: &str,
    runtime_deps: &BTreeSet<BindingName>,
) -> bool {
    let statements = top_level_statement_slices(source);
    let lazy_bindings = statements
        .iter()
        .filter_map(|statement| lowered_lazy_initializer_statement_binding(statement))
        .collect::<BTreeSet<_>>();

    for statement in statements {
        let trimmed = statement.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("import ")
            || trimmed.starts_with("export ")
            || keyword_at(trimmed, 0, "function")
            || variable_declaration_without_initializer(trimmed)
        {
            continue;
        }
        if lowered_lazy_initializer_statement_binding(trimmed).is_some() {
            continue;
        }
        if lazy_bindings
            .iter()
            .any(|binding| source_contains_top_level_call(trimmed, binding.as_str()))
        {
            return false;
        }
        if identifier_read_facts_in_source(trimmed)
            .into_iter()
            .any(|fact| {
                runtime_deps.contains(&BindingName::new(fact.name.as_str()))
                    || fact.name == "lazyValue"
                    || fact.name == "lazyModule"
            })
        {
            return false;
        }
    }
    true
}

pub(crate) fn owner_runtime_writes_are_lazy_safe(
    source: &str,
    written_helpers: &BTreeSet<BindingName>,
) -> bool {
    if written_helpers.is_empty() {
        return true;
    }

    let statements = top_level_statement_slices(source);
    let lazy_bindings = statements
        .iter()
        .filter_map(|statement| lowered_lazy_initializer_statement_binding(statement))
        .collect::<BTreeSet<_>>();

    for statement in statements {
        let trimmed = statement.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("import ")
            || trimmed.starts_with("export ")
            || keyword_at(trimmed, 0, "function")
            || variable_declaration_without_initializer(trimmed)
        {
            continue;
        }
        if lowered_lazy_initializer_statement_binding(trimmed).is_some() {
            continue;
        }
        if lazy_bindings
            .iter()
            .any(|binding| source_contains_top_level_call(trimmed, binding.as_str()))
        {
            return false;
        }

        let local_bindings = local_bindings_in_source(trimmed);
        if implicit_global_writes_in_source(trimmed)
            .into_iter()
            .filter(|write| !local_bindings.contains(write.as_str()))
            .any(|write| written_helpers.contains(&write))
        {
            return false;
        }
    }
    true
}

pub(crate) fn folded_runtime_reads_are_lazy_safe(
    ctx: &RuntimeReaderClusterContext<'_>,
    primary_bindings: &BTreeSet<BindingName>,
    moved_snippets: &BTreeSet<BindingName>,
    moved_namespace_exports: &BTreeSet<BindingName>,
) -> bool {
    let moved_functions = moved_snippets
        .iter()
        .chain(moved_namespace_exports.iter())
        .cloned()
        .collect::<BTreeSet<_>>();
    for binding in primary_bindings {
        if !ctx
            .read_index
            .folded_non_snippet_runtime_reads
            .contains(binding)
        {
            continue;
        }
        if !ctx
            .read_index
            .folded_lazy_safe_runtime_reads
            .contains(binding)
            || ctx.read_index.folded_unsafe_runtime_reads.contains(binding)
        {
            return false;
        }
    }
    for binding in moved_functions {
        if !ctx
            .read_index
            .folded_non_snippet_runtime_reads
            .contains(&binding)
        {
            continue;
        }
        if ctx
            .read_index
            .folded_unsafe_runtime_reads
            .contains(&binding)
            || (!ctx
                .read_index
                .folded_lazy_safe_runtime_reads
                .contains(&binding)
                && !ctx
                    .read_index
                    .folded_non_call_runtime_reads
                    .contains(&binding))
        {
            return false;
        }
    }
    true
}

pub(crate) fn runtime_reader_owner_runtime_state(
    lowered_runtime_sources: &BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
    owners: impl IntoIterator<Item = ModuleId>,
) -> BTreeMap<ModuleId, RuntimeReaderOwnerRuntimeState> {
    let mut by_owner = BTreeMap::<ModuleId, RuntimeReaderOwnerRuntimeState>::new();
    for owner_module in owners {
        if by_owner.contains_key(&owner_module) {
            continue;
        }
        let Some(owner_source) = lowered_runtime_sources.get(&owner_module) else {
            continue;
        };
        let can_localize_lazy_value = if owner_source.uses_lazy_value
            && !owner_source.uses_lazy_module
        {
            let (localized, changed) = inline_remaining_lazy_value_wrappers_allowing_assignments(
                owner_source.source.as_str(),
            );
            changed && !source_contains_top_level_call(localized.as_str(), "lazyValue")
        } else {
            false
        };
        by_owner.insert(
            owner_module,
            RuntimeReaderOwnerRuntimeState {
                source: owner_source.source.clone(),
                remaining_helpers: owner_source.remaining_helpers.clone(),
                written_helpers: owner_source.written_helpers.clone(),
                uses_lazy_module: owner_source.uses_lazy_module,
                uses_lazy_value: owner_source.uses_lazy_value,
                can_localize_lazy_value,
            },
        );
    }
    by_owner
}

pub(crate) fn runtime_dep_aliases_for_owner_conflicts(
    ctx: &RuntimeReaderClusterContext<'_>,
    moved_snippets: &BTreeSet<BindingName>,
    moved_namespace_exports: &BTreeSet<BindingName>,
    extra_runtime_deps: &BTreeSet<BindingName>,
    primary_bindings: &BTreeSet<BindingName>,
    extra_noop_deps: &BTreeSet<BindingName>,
    owner_available_bindings: &BTreeSet<BindingName>,
) -> Option<BTreeMap<BindingName, BindingName>> {
    let mut aliases = BTreeMap::<BindingName, BindingName>::new();
    let mut reserved = owner_available_bindings.clone();
    reserved.extend(moved_snippets.iter().cloned());
    reserved.extend(moved_namespace_exports.iter().cloned());
    reserved.extend(primary_bindings.iter().cloned());
    reserved.extend(extra_noop_deps.iter().cloned());
    reserved.extend(extra_runtime_deps.iter().cloned());

    for dep in extra_runtime_deps
        .iter()
        .filter(|dep| owner_available_bindings.contains(*dep))
    {
        if !runtime_dep_alias_is_safe_in_moved_sources(
            ctx,
            moved_snippets,
            moved_namespace_exports,
            dep,
        ) {
            return None;
        }
        let alias = unique_runtime_dep_alias(dep, &mut reserved);
        aliases.insert(dep.clone(), alias);
    }

    Some(aliases)
}

pub(crate) fn unique_runtime_dep_alias(
    binding: &BindingName,
    reserved: &mut BTreeSet<BindingName>,
) -> BindingName {
    let base = format!("__reverts_runtime_{}", binding.as_str());
    for suffix in 0.. {
        let candidate = if suffix == 0 {
            BindingName::new(base.as_str())
        } else {
            BindingName::new(format!("{base}_{suffix}"))
        };
        if reserved.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("unbounded alias search should always find an identifier")
}

pub(crate) fn runtime_dep_alias_is_safe_in_moved_sources(
    ctx: &RuntimeReaderClusterContext<'_>,
    moved_snippets: &BTreeSet<BindingName>,
    moved_namespace_exports: &BTreeSet<BindingName>,
    dep: &BindingName,
) -> bool {
    for snippet in moved_snippets {
        let Some(source) = ctx
            .prelude
            .snippets
            .get(snippet)
            .map(|snippet| snippet.source.as_str())
        else {
            return false;
        };
        if !identifier_read_rename_sites_are_safe(source, dep) {
            return false;
        }
    }
    for namespace in moved_namespace_exports {
        let Some(namespace_export) = ctx.read_index.namespace_exports_by_namespace.get(namespace)
        else {
            return false;
        };
        let source = runtime_namespace_export_statement(namespace_export);
        if !identifier_read_rename_sites_are_safe(source.as_str(), dep) {
            return false;
        }
    }
    true
}

pub(crate) fn runtime_reader_source_dependency(
    ctx: &RuntimeReaderClusterContext<'_>,
    owner_module: ModuleId,
    binding: &BindingName,
) -> Option<ModuleId> {
    let dep_module = ctx
        .source_definition_modules
        .get(binding)
        .and_then(|module_id| *module_id)?;
    if dep_module == owner_module || ctx.folded_modules.contains(&dep_module) {
        return None;
    }
    let owner_already_depends_on_source = ctx
        .module_dependencies_by_owner
        .get(&owner_module)
        .is_some_and(|deps| deps.contains(&dep_module));
    // If the writer already imports the source module, re-use that edge.
    // Otherwise we may synthesize a new direct import for the moved reader
    // dependency, but only when it does not introduce a source-module cycle.
    if !owner_already_depends_on_source
        && module_dependency_path_exists(ctx.module_dependencies_by_owner, dep_module, owner_module)
    {
        return None;
    }
    Some(dep_module)
}

pub(crate) fn runtime_reader_source_reexport_dependency(
    ctx: &RuntimeReaderClusterContext<'_>,
    owner_module: ModuleId,
    binding: &BindingName,
) -> Option<ModuleId> {
    let dep_module = ctx
        .source_definition_modules
        .get(binding)
        .and_then(|module_id| *module_id)?;
    if dep_module == owner_module || ctx.folded_modules.contains(&dep_module) {
        return None;
    }
    let owner_already_depends_on_source = ctx
        .module_dependencies_by_owner
        .get(&owner_module)
        .is_some_and(|deps| deps.contains(&dep_module));
    if owner_already_depends_on_source
        || !module_dependency_path_exists(
            ctx.module_dependencies_by_owner,
            dep_module,
            owner_module,
        )
    {
        return None;
    }
    Some(dep_module)
}

pub(crate) fn runtime_reader_externalized_package_init_dependency(
    ctx: &RuntimeReaderClusterContext<'_>,
    snippet_source: &str,
    binding: &BindingName,
) -> bool {
    let Some(dep_module) = ctx
        .all_source_definition_modules
        .get(binding)
        .and_then(|module_id| *module_id)
    else {
        return false;
    };
    if !ctx.externalized_packages.contains(&dep_module) {
        return false;
    }
    let reads = identifier_read_facts_in_source(snippet_source)
        .into_iter()
        .filter(|fact| fact.name == binding.as_str())
        .collect::<Vec<_>>();
    !reads.is_empty() && reads.iter().all(|fact| fact.is_call_callee)
}

pub(crate) fn runtime_reader_folded_runtime_dependency(
    ctx: &RuntimeReaderClusterContext<'_>,
    owner_module: ModuleId,
    binding: &BindingName,
) -> bool {
    if !ctx.folded_runtime_definitions.contains(binding) {
        return false;
    }
    let Some(dep_module) = ctx
        .source_definition_modules
        .get(binding)
        .and_then(|module_id| *module_id)
    else {
        // Folded runtime chunks can contain minified locals whose names are not
        // unique across source modules.  They are still concrete definitions in
        // the runtime helper file, so a migrated reader may import them from
        // runtime directly.  The folded-non-snippet cycle gate below still
        // rejects cases where runtime would also need to import this reader
        // back from the writer while the writer imports folded runtime deps.
        return true;
    };
    if dep_module == owner_module {
        return false;
    }
    // The definition is already materialized in the runtime helper file.  Even
    // when the source-definition owner is no longer a folded stub (or the same
    // minified name resolves to a different source owner), the migrated reader
    // can depend on the runtime copy without importing that source module.
    true
}

pub(crate) fn runtime_reader_cluster_source_lines(
    ctx: &RuntimeReaderClusterContext<'_>,
    readers: &BTreeSet<BindingName>,
) -> usize {
    readers
        .iter()
        .filter_map(|reader| ctx.prelude.snippets.get(reader))
        .map(|snippet| snippet.source.lines().count().max(1))
        .sum()
}

pub(crate) fn runtime_reader_migration_source_lines(
    ctx: &RuntimeReaderClusterContext<'_>,
    migration: &RuntimeReaderClusterMigration,
) -> usize {
    runtime_reader_cluster_source_lines(ctx, &migration.extra_snippets)
        + migration.extra_namespace_exports.len()
}

pub(crate) fn folded_runtime_chunk_definitions(
    folded_chunks: &[RuntimeFoldedSourceChunk],
) -> BTreeSet<BindingName> {
    folded_chunks
        .iter()
        .flat_map(|chunk| top_level_definitions_in_source(chunk.source.as_str()))
        .collect()
}

pub(crate) fn runtime_binding_has_blocking_non_snippet_use(
    read_index: &RuntimeSourceReadIndex,
    binding: &BindingName,
) -> bool {
    if read_index
        .entrypoint_callee
        .as_ref()
        .is_some_and(|entrypoint| entrypoint == binding)
    {
        return true;
    }
    read_index.non_snippet_runtime_reads.contains(binding)
        || read_index.namespace_export_helpers.contains(binding)
}

pub(crate) fn runtime_reader_folded_non_snippet_use_can_move(
    ctx: &RuntimeReaderClusterContext<'_>,
    binding: &BindingName,
) -> bool {
    ctx.read_index
        .folded_non_snippet_runtime_reads
        .contains(binding)
        && !ctx
            .read_index
            .entrypoint_non_snippet_runtime_reads
            .contains(binding)
        && ctx
            .read_index
            .entrypoint_callee
            .as_ref()
            .is_none_or(|entrypoint| entrypoint != binding)
        && !ctx.read_index.namespace_export_helpers.contains(binding)
        && !ctx
            .read_index
            .namespace_exports_by_namespace
            .contains_key(binding)
        && !ctx.folded_runtime_definitions.contains(binding)
}

pub(crate) fn runtime_private_function_dependency_closure(
    ctx: &RuntimeReaderClusterContext<'_>,
    owner_module: ModuleId,
    seed: &BindingName,
    already_moved: &BTreeSet<BindingName>,
) -> Option<BTreeSet<BindingName>> {
    let mut closure = BTreeSet::<BindingName>::new();
    let mut queue = vec![seed.clone()];
    while let Some(binding) = queue.pop() {
        if already_moved.contains(&binding) || !closure.insert(binding.clone()) {
            continue;
        }
        if !runtime_private_function_dependency_can_move(ctx, owner_module, &binding) {
            return None;
        }
        for reader in runtime_readers_for_binding(ctx.read_index, &binding) {
            if reader == binding || already_moved.contains(&reader) || closure.contains(&reader) {
                continue;
            }
            queue.push(reader);
        }
    }
    Some(closure)
}

pub(crate) fn runtime_private_function_dependency_can_move(
    ctx: &RuntimeReaderClusterContext<'_>,
    owner_module: ModuleId,
    binding: &BindingName,
) -> bool {
    if ctx.movable_bindings.contains(binding)
        || runtime_binding_has_blocking_non_snippet_use(ctx.read_index, binding)
        || ctx
            .read_index
            .namespace_exports_by_namespace
            .contains_key(binding)
    {
        return false;
    }
    if ctx
        .source_consumers_by_runtime_binding
        .get(&(ctx.source_file_id, binding.clone()))
        .is_some_and(|consumers| consumers.iter().any(|consumer| *consumer != owner_module))
    {
        return false;
    }
    let Some(snippet) = ctx.prelude.snippets.get(binding) else {
        return false;
    };
    if !is_migratable_private_runtime_function_dependency(binding, snippet.source.as_str()) {
        return false;
    }
    ctx.read_index
        .runtime_writes_by_snippet
        .get(binding)
        .is_none_or(BTreeSet::is_empty)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeLazyFoldModule {
    pub(crate) source_file_id: u32,
    pub(crate) required_bindings: BTreeSet<BindingName>,
    pub(crate) stub_exports: BTreeSet<BindingName>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeFoldedSourceChunk {
    pub(crate) byte_start: u32,
    pub(crate) source: String,
}

pub(crate) fn lowered_runtime_sources(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
    eager_safe_analysis: &EagerSafeAnalysis,
    externalized_packages: &BTreeSet<ModuleId>,
) -> BTreeMap<ModuleId, LoweredRuntimeModuleSource> {
    let mut sources = BTreeMap::new();
    for module in program.model().modules() {
        if module.kind == ModuleKind::Package && externalized_packages.contains(&module.id) {
            continue;
        }
        let runtime_imports = program.model().graph().runtime_imports_for(module.id);
        let Some(source) = program.model().input().module_source_slice(module.id) else {
            continue;
        };
        let mut helper_kinds = runtime_helper_kinds(program.model().graph(), &runtime_imports);
        helper_kinds.extend(runtime_helper_kinds_for_source(
            program.model().graph(),
            source.source_file_id,
            source.source,
        ));
        // A reconstructed esbuild multi-handle module (synthetic source) carries
        // its parent bundle's nested `helper(()=>{...})` calls. The synthetic
        // source file has no prelude of its own, so inherit the PARENT source
        // file's helper classification (encoded in the synthetic path) — else
        // the raw helper aliases (`We`/`St` = __commonJS) survive the rename and
        // read as unresolved free vars.
        if let Some(parent_source_file_id) =
            synthetic_parent_source_file_id(source.source_file_path)
        {
            helper_kinds.extend(runtime_helper_kinds_for_source(
                program.model().graph(),
                parent_source_file_id,
                source.source,
            ));
        }
        if let Some(imports_by_target) = source_module_wiring.imports_by_module.get(&module.id) {
            for binding in imports_by_target
                .values()
                .flat_map(|bindings| bindings.iter())
            {
                helper_kinds.insert(binding.clone(), RuntimePreludeBindingKind::SourceBacked);
            }
        }
        // Cross-module export set: any binding this module exports stays
        // observable from other modules' call sites. Pass them to the
        // lowering pass so delazify / namespace decomposition refuse to
        // collapse bindings that external consumers still call as thunks.
        // Two sources of "this module's binding is observed externally":
        //   1. `import_export().exports_for(m)` — bindings the graph marked
        //      as explicit ESM exports.
        //   2. `source_module_wiring.exports_by_module[m]` — bindings the
        //      planner is about to emit `export { X }` for because some
        //      other module's source imports them. In bundle inputs the
        //      module rarely has a literal `export` keyword, so this is
        //      where the real cross-module surface lives.
        //
        // Phase 8 relaxation: bindings the cross-module eager-safe
        // analysis cleared are SUBTRACTED from the blocked set. They've
        // already been verified to be (a) outside any top-level
        // evaluation cycle (singleton SCC) and (b) only referenced by
        // consumers in zero-arg `X()` shape — i.e. mechanically
        // rewritable via the consumer-side pass below.
        let mut exported_bindings: BTreeSet<BindingName> = program
            .model()
            .graph()
            .import_export()
            .exports_for(module.id)
            .into_iter()
            .collect();
        if let Some(wiring_exports) = source_module_wiring.exports_by_module.get(&module.id) {
            exported_bindings.extend(wiring_exports.iter().cloned());
        }
        if let Some(eager_safe) = eager_safe_analysis
            .eager_safe_exports_by_module
            .get(&module.id)
        {
            for binding in eager_safe {
                exported_bindings.remove(binding);
            }
        }
        let empty_safe_targets = BTreeSet::<String>::new();
        let eager_safe_call_targets = eager_safe_analysis
            .safe_call_targets_by_module
            .get(&module.id)
            .unwrap_or(&empty_safe_targets);
        let mut lowering = lower_runtime_helpers_with_options(
            source.source,
            &helper_kinds,
            &exported_bindings,
            eager_safe_call_targets,
            false,
        );
        // Phase 8 cross-module rewrite: bindings this module imports
        // that the eager-safe analysis cleared no longer carry their
        // lazy thunk in the exporting module. Their `X()` call sites
        // in this consumer must be stripped to bare `X` so the
        // import-binding (now a direct value) is accessed instead of
        // attempting to invoke a non-function.
        let eagerified_imports =
            consumer_eagerified_imports(module.id, source_module_wiring, eager_safe_analysis);
        if !eagerified_imports.is_empty() {
            lowering.source = rewrite_eagerified_call_sites(&lowering.source, &eagerified_imports);
        }
        let local_definitions = top_level_definitions_in_source(lowering.source.as_str());
        let local_writes = implicit_global_writes_in_source(lowering.source.as_str());
        let mut planned_bindings = BTreeSet::<BindingName>::new();
        if let Some(module_imports) = source_module_wiring.imports_by_module.get(&module.id) {
            for bindings in module_imports.values() {
                planned_bindings.extend(bindings.iter().cloned());
            }
        }
        let remaining_helpers = lowering
            .remaining_helpers
            .into_iter()
            .filter(|binding| !planned_bindings.contains(binding))
            .filter(|binding| !local_definitions.contains(binding))
            .collect::<BTreeSet<_>>();
        let written_helpers = remaining_helpers
            .intersection(&local_writes)
            .cloned()
            .collect::<BTreeSet<_>>();
        let byte_start = source.span.map(|span| span.byte_start).unwrap_or_default();
        sources.insert(
            module.id,
            LoweredRuntimeModuleSource {
                source_file_id: source.source_file_id,
                source_file_path: source.source_file_path.to_string(),
                byte_start,
                source: lowering.source,
                lowered_helpers: lowering.lowered_helpers,
                remaining_helpers,
                local_definitions,
                local_writes,
                written_helpers,
                uses_lazy_module: lowering.uses_lazy_module,
                uses_lazy_value: lowering.uses_lazy_value,
                reshaped_bindings: lowering.reshaped_bindings,
            },
        );
    }
    sources
}

pub(crate) fn runtime_lazy_fold_plan(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
    lowered_runtime_sources: &BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
    externalized_packages: &BTreeSet<ModuleId>,
) -> RuntimeLazyFoldPlan {
    let mut plan = RuntimeLazyFoldPlan::default();
    let writer_modules_by_binding =
        runtime_writer_modules_by_binding(program, lowered_runtime_sources, externalized_packages);
    let remaining_modules_by_binding =
        runtime_remaining_modules_by_binding(lowered_runtime_sources);
    let foldable_import_modules_by_binding = runtime_foldable_import_modules_by_binding(
        program,
        source_module_wiring,
        lowered_runtime_sources,
    );
    let source_definition_modules =
        unique_source_definition_modules(program, externalized_packages);
    let all_source_definition_modules = unique_source_definition_modules(program, &BTreeSet::new());
    let module_dependencies_by_owner = module_dependency_modules_by_owner(program);
    let runtime_source_consumers =
        runtime_prelude_direct_import_consumers(program, lowered_runtime_sources);
    let read_indices_by_source = program
        .model()
        .graph()
        .runtime_preludes()
        .iter()
        .map(|(source_file_id, prelude)| (*source_file_id, runtime_source_read_index(prelude, &[])))
        .collect::<BTreeMap<_, _>>();
    let lowered_runtime_owner_modules = lowered_runtime_sources.keys().copied().collect::<Vec<_>>();
    let owner_available_bindings = runtime_reader_owner_available_bindings(
        program,
        source_module_wiring,
        lowered_runtime_sources,
        lowered_runtime_owner_modules.iter().copied(),
    );
    let owner_runtime_state = runtime_reader_owner_runtime_state(
        lowered_runtime_sources,
        lowered_runtime_owner_modules.iter().copied(),
    );
    let owner_source_lines =
        owner_module_source_lines(program, lowered_runtime_owner_modules.iter().copied());
    let stay_local_context = RuntimeLazyStayLocalContext {
        externalized_packages,
        writer_modules_by_binding: &writer_modules_by_binding,
        remaining_modules_by_binding: &remaining_modules_by_binding,
        foldable_import_modules_by_binding: &foldable_import_modules_by_binding,
        source_definition_modules: &source_definition_modules,
        all_source_definition_modules: &all_source_definition_modules,
        module_dependencies_by_owner: &module_dependencies_by_owner,
        runtime_source_consumers: &runtime_source_consumers,
        owner_available_bindings: &owner_available_bindings,
        owner_runtime_state: &owner_runtime_state,
        owner_source_lines: &owner_source_lines,
    };
    for module in program.model().modules() {
        if module.kind != ModuleKind::Application {
            continue;
        }
        if !program.package_imports_for(module.id).is_empty() {
            continue;
        }
        let Some(source) = lowered_runtime_sources.get(&module.id) else {
            continue;
        };
        if source.written_helpers.is_empty() {
            continue;
        }
        let Some(prelude) = program
            .model()
            .graph()
            .runtime_prelude(source.source_file_id)
        else {
            continue;
        };
        if !source
            .written_helpers
            .iter()
            .all(|binding| prelude.defines(binding))
        {
            continue;
        }
        let Some((folded_source, explicit_exports)) =
            strip_top_level_named_exports(source.source.as_str())
        else {
            continue;
        };
        if !source_is_lazy_preserving_foldable(folded_source.as_str()) {
            continue;
        }
        // A self-contained lazy writer module does not need to become a
        // runtime chunk at all. Keeping it as a normal source module lets the
        // runtime-var migration below move its written helper vars into the
        // same module and remove the setters entirely. Stay conservative:
        // skip folding only when every written helper is a migratable var,
        // has this module as its only writer/source reader, and any moved
        // reader's leftover runtime deps are stable helper-owned values.
        if read_indices_by_source
            .get(&source.source_file_id)
            .is_some_and(|read_index| {
                runtime_lazy_fold_can_stay_local(
                    &stay_local_context,
                    module.id,
                    source,
                    prelude,
                    read_index,
                )
            })
        {
            continue;
        }
        let folded_source =
            purify_folded_lazy_initializers(folded_source.as_str(), &source.written_helpers);
        let mut available_bindings = program.model().graph().ast_definitions_for(module.id);
        available_bindings.extend(program.model().graph().ast_imports_for(module.id));
        available_bindings.extend(source.written_helpers.iter().cloned());
        let mut stub_exports = program
            .model()
            .graph()
            .import_export()
            .exports_for(module.id)
            .into_iter()
            .collect::<BTreeSet<_>>();
        stub_exports.extend(explicit_exports);
        if let Some(extra_exports) = source_module_wiring.exports_by_module.get(&module.id) {
            stub_exports.extend(extra_exports.iter().cloned());
        }
        if stub_exports.is_empty()
            || !stub_exports
                .iter()
                .all(|binding| available_bindings.contains(binding))
        {
            continue;
        }
        let mut required_bindings = source.remaining_helpers.clone();
        required_bindings.extend(source.written_helpers.iter().cloned());
        plan.modules.insert(
            module.id,
            RuntimeLazyFoldModule {
                source_file_id: source.source_file_id,
                required_bindings,
                stub_exports,
            },
        );
        plan.chunks_by_source_file
            .entry(source.source_file_id)
            .or_default()
            .push(RuntimeFoldedSourceChunk {
                byte_start: source.byte_start,
                source: folded_source.trim().to_string(),
            });
    }
    for chunks in plan.chunks_by_source_file.values_mut() {
        chunks.sort_by_key(|chunk| chunk.byte_start);
    }
    plan
}

pub(crate) fn runtime_writer_modules_by_binding(
    program: &EnrichedProgram,
    lowered_runtime_sources: &BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
    externalized_packages: &BTreeSet<ModuleId>,
) -> BTreeMap<BindingName, BTreeSet<ModuleId>> {
    let modules_by_id = program
        .model()
        .modules()
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let mut writers = BTreeMap::<BindingName, BTreeSet<ModuleId>>::new();
    for (module_id, source) in lowered_runtime_sources {
        if modules_by_id.get(module_id).is_some_and(|module| {
            module.kind == ModuleKind::Package && externalized_packages.contains(module_id)
        }) {
            continue;
        }
        for binding in &source.written_helpers {
            writers
                .entry(binding.clone())
                .or_default()
                .insert(*module_id);
        }
    }
    writers
}

pub(crate) fn runtime_remaining_modules_by_binding(
    lowered_runtime_sources: &BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
) -> BTreeMap<BindingName, BTreeSet<ModuleId>> {
    let mut readers = BTreeMap::<BindingName, BTreeSet<ModuleId>>::new();
    for (module_id, source) in lowered_runtime_sources {
        for binding in &source.remaining_helpers {
            readers
                .entry(binding.clone())
                .or_default()
                .insert(*module_id);
        }
    }
    readers
}

pub(crate) fn runtime_foldable_import_modules_by_binding(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
    lowered_runtime_sources: &BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
) -> BTreeMap<BindingName, BTreeSet<ModuleId>> {
    let mut readers = BTreeMap::<BindingName, BTreeSet<ModuleId>>::new();
    for module in program.model().modules() {
        if module.kind != ModuleKind::Application
            || !program.package_imports_for(module.id).is_empty()
        {
            continue;
        }
        let Some(source) = lowered_runtime_sources.get(&module.id) else {
            continue;
        };
        if source.written_helpers.is_empty() {
            continue;
        }
        let Some((folded_source, _explicit_exports)) =
            strip_top_level_named_exports(source.source.as_str())
        else {
            continue;
        };
        if !source_is_lazy_preserving_foldable(folded_source.as_str()) {
            continue;
        }
        let Some(imports_by_target) = source_module_wiring.imports_by_module.get(&module.id) else {
            continue;
        };
        for binding in imports_by_target.values().flatten() {
            readers
                .entry(binding.clone())
                .or_default()
                .insert(module.id);
        }
    }
    readers
}

pub(crate) struct RuntimeLazyStayLocalContext<'a> {
    externalized_packages: &'a BTreeSet<ModuleId>,
    writer_modules_by_binding: &'a BTreeMap<BindingName, BTreeSet<ModuleId>>,
    remaining_modules_by_binding: &'a BTreeMap<BindingName, BTreeSet<ModuleId>>,
    foldable_import_modules_by_binding: &'a BTreeMap<BindingName, BTreeSet<ModuleId>>,
    source_definition_modules: &'a BTreeMap<BindingName, Option<ModuleId>>,
    all_source_definition_modules: &'a BTreeMap<BindingName, Option<ModuleId>>,
    module_dependencies_by_owner: &'a BTreeMap<ModuleId, BTreeSet<ModuleId>>,
    runtime_source_consumers: &'a BTreeMap<(u32, BindingName), BTreeSet<ModuleId>>,
    owner_available_bindings: &'a BTreeMap<ModuleId, BTreeSet<BindingName>>,
    owner_runtime_state: &'a BTreeMap<ModuleId, RuntimeReaderOwnerRuntimeState>,
    owner_source_lines: &'a BTreeMap<ModuleId, usize>,
}

pub(crate) fn runtime_lazy_fold_can_stay_local(
    ctx: &RuntimeLazyStayLocalContext<'_>,
    module_id: ModuleId,
    source: &LoweredRuntimeModuleSource,
    prelude: &RuntimePrelude,
    read_index: &RuntimeSourceReadIndex,
) -> bool {
    if source.written_helpers.is_empty() || source.uses_lazy_module || !source.uses_lazy_value {
        return false;
    }
    if !source.written_helpers.iter().all(|binding| {
        prelude.defines(binding)
            && migratable_runtime_var_initializer(prelude, binding).is_some()
            && ctx
                .writer_modules_by_binding
                .get(binding)
                .is_some_and(|writers| writers.len() == 1 && writers.contains(&module_id))
            && ctx
                .remaining_modules_by_binding
                .get(binding)
                .is_none_or(|readers| readers.iter().all(|reader| *reader == module_id))
            && ctx
                .foldable_import_modules_by_binding
                .get(binding)
                .is_none_or(|readers| readers.iter().all(|reader| *reader == module_id))
    }) {
        return false;
    }

    let movable_bindings = source.written_helpers.clone();
    let candidate_owners = movable_bindings
        .iter()
        .cloned()
        .map(|binding| (binding, module_id))
        .collect::<BTreeMap<_, _>>();
    let folded_modules = BTreeSet::<ModuleId>::new();
    let folded_runtime_definitions = BTreeSet::<BindingName>::new();
    let reader_cluster_context = RuntimeReaderClusterContext {
        source_file_id: source.source_file_id,
        owner_available_bindings: ctx.owner_available_bindings,
        source_consumers_by_runtime_binding: ctx.runtime_source_consumers,
        source_definition_modules: ctx.source_definition_modules,
        all_source_definition_modules: ctx.all_source_definition_modules,
        externalized_packages: ctx.externalized_packages,
        module_dependencies_by_owner: ctx.module_dependencies_by_owner,
        folded_modules: &folded_modules,
        folded_runtime_definitions: &folded_runtime_definitions,
        owner_runtime_state: ctx.owner_runtime_state,
        owner_source_lines: ctx.owner_source_lines,
        prelude,
        read_index,
        movable_bindings: &movable_bindings,
        candidate_owners: &candidate_owners,
    };

    let mut covered_primary_bindings = BTreeSet::<BindingName>::new();
    for binding in &source.written_helpers {
        match runtime_binding_read_profile_diagnostic(read_index, binding) {
            Ok(RuntimeBindingReadProfile::NoReads) => {
                covered_primary_bindings.insert(binding.clone());
            }
            Ok(RuntimeBindingReadProfile::SnippetReaders(readers)) => {
                if runtime_reader_cluster_source_lines(&reader_cluster_context, &readers)
                    > runtime_reader_cluster_cap_for_owner(&reader_cluster_context, module_id)
                {
                    return false;
                }
                let migration_result = migratable_runtime_reader_cluster_result(
                    &reader_cluster_context,
                    module_id,
                    binding,
                    readers,
                );
                let Ok(migration) = migration_result else {
                    return false;
                };
                // Keeping a foldable writer as a normal module is only a
                // runtime-size win when it can still go through the ordinary
                // runtime-var migration path. Stable runtime deps (values with
                // no writer module) are safe to import back from the helper;
                // writer-owned deps would interfere with the other writer's
                // migration/cycle gates, so those modules stay folded.
                let runtime_deps_are_stable = migration
                    .extra_runtime_deps
                    .iter()
                    .chain(migration.extra_runtime_dep_aliases.keys())
                    .all(|dep| {
                        ctx.writer_modules_by_binding
                            .get(dep)
                            .is_none_or(BTreeSet::is_empty)
                    });
                if !migration
                    .primary_bindings
                    .iter()
                    .all(|primary| source.written_helpers.contains(primary))
                    || !migration.extra_namespace_exports.is_empty()
                    || !migration.extra_runtime_setter_deps.is_empty()
                    || !runtime_deps_are_stable
                    || !migration.pinned_runtime_deps.is_empty()
                    || !migration.extra_source_deps.is_empty()
                    || !migration.extra_runtime_reexport_source_deps.is_empty()
                {
                    return false;
                }
                covered_primary_bindings.extend(migration.primary_bindings);
            }
            Ok(RuntimeBindingReadProfile::Rejected) | Err(_) => return false,
        }
    }
    covered_primary_bindings == source.written_helpers
}

pub(crate) fn strip_top_level_named_exports(
    source: &str,
) -> Option<(String, BTreeSet<BindingName>)> {
    let bytes = source.as_bytes();
    let mut output = String::new();
    let mut exports = BTreeSet::new();
    let mut cursor = 0usize;
    let mut last = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;

    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            _ if paren_depth == 0
                && bracket_depth == 0
                && brace_depth == 0
                && keyword_at(source, cursor, "import") =>
            {
                return None;
            }
            _ if paren_depth == 0
                && bracket_depth == 0
                && brace_depth == 0
                && keyword_at(source, cursor, "export") =>
            {
                let export_start = cursor;
                cursor = skip_ws(bytes, cursor + "export".len());
                if bytes.get(cursor) != Some(&b'{') {
                    return None;
                }
                let export_end = find_matching_brace(source, cursor)?;
                exports.extend(parse_named_export_bindings(
                    &source[cursor + 1..export_end],
                )?);
                cursor = skip_ws(bytes, export_end + 1);
                if keyword_at(source, cursor, "from") {
                    return None;
                }
                if bytes.get(cursor) == Some(&b';') {
                    cursor += 1;
                }
                output.push_str(&source[last..export_start]);
                last = cursor;
            }
            b'(' => {
                paren_depth += 1;
                cursor += 1;
            }
            b'[' => {
                bracket_depth += 1;
                cursor += 1;
            }
            b'{' => {
                brace_depth += 1;
                cursor += 1;
            }
            b')' => {
                paren_depth = paren_depth.saturating_sub(1);
                cursor += 1;
            }
            b']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                cursor += 1;
            }
            b'}' => {
                brace_depth = brace_depth.saturating_sub(1);
                cursor += 1;
            }
            _ => cursor += 1,
        }
    }
    output.push_str(&source[last..]);
    Some((output, exports))
}

pub(crate) fn parse_named_export_bindings(source: &str) -> Option<BTreeSet<BindingName>> {
    let mut bindings = BTreeSet::new();
    for item in split_top_level_properties(source) {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        if item.starts_with("type ") || item == "type" {
            return None;
        }
        let parts = item.split_whitespace().collect::<Vec<_>>();
        if parts.contains(&"as") {
            return None;
        }
        let local = parts.first().copied()?;
        let (binding, end) = parse_identifier(local, 0)?;
        if end != local.len() || is_js_keyword(binding) {
            return None;
        }
        bindings.insert(BindingName::new(binding));
    }
    Some(bindings)
}

pub(crate) fn source_is_lazy_preserving_foldable(source: &str) -> bool {
    let mut saw_lazy_initializer = false;
    for statement in top_level_statement_slices(source) {
        let statement = statement.trim();
        if statement.is_empty() {
            continue;
        }
        if lowered_lazy_initializer_statement_binding(statement).is_some() {
            saw_lazy_initializer = true;
            continue;
        }
        if variable_declaration_without_initializer(statement) {
            continue;
        }
        return false;
    }
    saw_lazy_initializer
}

pub(crate) fn purify_folded_lazy_initializers(
    source: &str,
    writable_helpers: &BTreeSet<BindingName>,
) -> String {
    top_level_statement_slices(source)
        .into_iter()
        .filter_map(|statement| {
            let statement = statement.trim();
            if statement.is_empty() {
                return None;
            }
            Some(
                pure_lazy_initializer_replacement(statement, writable_helpers)
                    .unwrap_or_else(|| statement.to_string()),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn purify_private_runtime_lazy_initializers(
    source: &str,
    writable_helpers: &BTreeSet<BindingName>,
) -> String {
    let pure_value_bindings = pure_runtime_value_bindings(source);
    let bytes = source.as_bytes();
    let mut edits = Vec::<(usize, usize, String)>::new();
    let mut cursor = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        if paren_depth == 0
            && bracket_depth == 0
            && brace_depth == 0
            && let Some((initializer, after)) =
                try_parse_runtime_lazy_initializer_declaration(source, cursor)
        {
            if let Some(replacement) = private_runtime_lazy_initializer_replacement(
                &initializer,
                writable_helpers,
                &pure_value_bindings,
            ) {
                edits.push((initializer.span.0, initializer.span.1, replacement));
            }
            cursor = after;
            continue;
        }
        match bytes[cursor] {
            b'(' => paren_depth += 1,
            b')' => paren_depth = paren_depth.saturating_sub(1),
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b'{' => brace_depth += 1,
            b'}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
        cursor += 1;
    }
    if edits.is_empty() {
        return source.to_string();
    }
    apply_text_edits(source, &edits)
}

pub(crate) fn variable_declaration_without_initializer(statement: &str) -> bool {
    let statement = statement.trim().trim_end_matches(';').trim();
    let Some((keyword, after_keyword)) = declaration_keyword_at_start(statement) else {
        return false;
    };
    if keyword == "const" {
        return false;
    }
    let mut cursor = skip_ws(statement.as_bytes(), after_keyword);
    let Some((_binding, next)) = parse_identifier(statement, cursor) else {
        return false;
    };
    cursor = next;
    !contains_top_level_initializer_operator(statement, cursor)
}

pub(crate) fn lowered_lazy_initializer_statement_binding(statement: &str) -> Option<BindingName> {
    let statement = statement.trim().trim_end_matches(';').trim();
    let (_keyword, after_keyword) = declaration_keyword_at_start(statement)?;
    let cursor = skip_ws(statement.as_bytes(), after_keyword);
    let (binding, after_binding) = parse_identifier(statement, cursor)?;
    let equals = skip_ws(statement.as_bytes(), after_binding);
    if statement.as_bytes().get(equals) != Some(&b'=') {
        return None;
    }
    let initializer = statement[skip_ws(statement.as_bytes(), equals + 1)..].trim();
    looks_like_lowered_lazy_initializer(initializer).then(|| BindingName::new(binding))
}

pub(crate) fn looks_like_lowered_lazy_initializer(initializer: &str) -> bool {
    let compact = compact_js_source(initializer);
    compact.starts_with("lazyValue(()=>{") && compact.ends_with("})")
}

pub(crate) fn ensure_planned_module_exports(
    plan: &mut EmitPlan,
    program: &EnrichedProgram,
    module_id: ModuleId,
    bindings: &BTreeSet<BindingName>,
) {
    let Some(path) = module_output_path(program, module_id) else {
        return;
    };
    let Some(file) = plan.files.iter_mut().find(|file| file.path == path) else {
        return;
    };
    let existing = file
        .exports
        .iter()
        .map(|export| export.binding.clone())
        .collect::<BTreeSet<_>>();
    let missing = bindings
        .iter()
        .filter(|binding| !existing.contains(*binding))
        .cloned()
        .collect::<BTreeSet<_>>();
    if missing.is_empty() {
        return;
    }
    file.push_source(named_export_statement(missing.iter()));
    for binding in missing {
        file.add_export_with_source_backed(binding, true);
    }
}

pub(crate) fn source_module_wiring(
    program: &EnrichedProgram,
    externalized_packages: &BTreeSet<ModuleId>,
    external_package_adapters: &BTreeMap<ModuleId, ExternalPackageAdapterPlan>,
    source_facts: &SourceModuleFacts,
) -> SourceModuleWiring {
    let mut wiring = SourceModuleWiring::default();
    let candidate_reads_by_module = &source_facts.candidate_reads_by_module;
    let modules_by_id = program
        .model()
        .modules()
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let exportable_bindings_by_module = &source_facts.exportable_bindings_by_module;

    for dependency in &program.model().input().dependencies {
        let ModuleDependencyTarget::Module(target_module_id) = dependency.target else {
            continue;
        };
        let Some(from_module) = modules_by_id.get(&dependency.from_module_id) else {
            continue;
        };
        let Some(target_module) = modules_by_id.get(&target_module_id) else {
            continue;
        };
        if (from_module.kind == ModuleKind::Package
            && externalized_packages.contains(&from_module.id))
            || (target_module.kind == ModuleKind::Package
                && externalized_packages.contains(&target_module.id))
        {
            continue;
        }
        let Some(candidate_reads) = candidate_reads_by_module.get(&dependency.from_module_id)
        else {
            continue;
        };
        let adapter_bindings = external_package_adapters
            .get(&target_module_id)
            .map(|adapter| &adapter.bindings);
        let Some(target_bindings) =
            adapter_bindings.or_else(|| exportable_bindings_by_module.get(&target_module_id))
        else {
            continue;
        };
        let imported_bindings = candidate_reads
            .intersection(target_bindings)
            .cloned()
            .collect::<BTreeSet<_>>();
        if imported_bindings.is_empty() {
            continue;
        }
        wiring
            .imports_by_module
            .entry(dependency.from_module_id)
            .or_default()
            .entry(target_module_id)
            .or_default()
            .extend(imported_bindings.iter().cloned());
        wiring
            .exports_by_module
            .entry(target_module_id)
            .or_default()
            .extend(imported_bindings);
    }

    wiring
}

pub(crate) fn candidate_source_reads_by_module_with_exportable(
    program: &EnrichedProgram,
    exportable_bindings_by_module: &BTreeMap<ModuleId, BTreeSet<BindingName>>,
) -> BTreeMap<ModuleId, BTreeSet<BindingName>> {
    let mut reads = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
    let local_bindings_by_module = program
        .model()
        .modules()
        .iter()
        .filter_map(|module| {
            let source = program.model().input().module_source_slice(module.id)?;
            Some((module.id, local_bindings_in_source(source.source)))
        })
        .collect::<BTreeMap<_, _>>();
    for (module_id, binding) in program.model().graph().def_use().unresolved_reads() {
        if local_bindings_by_module
            .get(&module_id)
            .is_some_and(|bindings| bindings.contains(binding.as_str()))
        {
            continue;
        }
        reads.entry(module_id).or_default().insert(binding);
    }
    let empty_bindings = BTreeSet::<BindingName>::new();
    let empty_local_bindings = BTreeSet::<String>::new();
    for module in program.model().modules() {
        let Some(source) = program.model().input().module_source_slice(module.id) else {
            continue;
        };
        let exportable_bindings = exportable_bindings_by_module
            .get(&module.id)
            .unwrap_or(&empty_bindings);
        let local_bindings = local_bindings_by_module
            .get(&module.id)
            .unwrap_or(&empty_local_bindings);
        reads.entry(module.id).or_default().extend(
            identifiers_in_source(source.source)
                .into_iter()
                .map(BindingName::new)
                .filter(|binding| {
                    !exportable_bindings.contains(binding)
                        && !local_bindings.contains(binding.as_str())
                }),
        );
    }
    reads
}

pub(crate) fn source_exportable_bindings(
    program: &EnrichedProgram,
    module_id: ModuleId,
) -> BTreeSet<BindingName> {
    let mut bindings = source_definition_bindings(program, module_id);
    bindings.extend(program.model().graph().ast_imports_for(module_id));
    if let Some(source) = program.model().input().module_source_slice(module_id) {
        bindings.extend(named_reexported_bindings(source.source));
    }
    bindings
}

pub(crate) fn source_definition_bindings(
    program: &EnrichedProgram,
    module_id: ModuleId,
) -> BTreeSet<BindingName> {
    let mut bindings = program.model().graph().ast_definitions_for(module_id);
    if let Some(source) = program.model().input().module_source_slice(module_id) {
        bindings.extend(top_level_definitions_in_source(source.source));
        // A bare assignment to a host runtime global (`onmessage = …`,
        // `self = …`) is a side effect, not a module definition. Treating it
        // as a definition makes the module the apparent owner of that name, so
        // another module referencing the same global resolves to an import of
        // it — which dangles once lowering prunes the write from the owner's
        // emitted body. Host globals resolve at runtime; no module owns them.
        bindings.extend(
            implicit_global_writes_in_source(source.source)
                .into_iter()
                .filter(|binding| !is_runtime_global_identifier(binding.as_str())),
        );
    }
    bindings
}

pub(crate) fn named_reexported_bindings(source: &str) -> BTreeSet<BindingName> {
    source_statements(source)
        .into_iter()
        .filter(|statement| statement.starts_with("export {") && statement.contains("} from "))
        .flat_map(|statement| named_reexport_specifiers(statement).unwrap_or_default())
        .map(|specifier| BindingName::new(specifier.exported))
        .collect()
}

pub(crate) fn pure_named_barrel_reexports(source: &str) -> Option<BTreeSet<BindingName>> {
    let statements = source_statements(source);
    if statements.is_empty() {
        return None;
    }
    let mut direct_reexports = BTreeSet::<BindingName>::new();
    let mut imported_locals = BTreeSet::<BindingName>::new();
    let mut local_exports = BTreeSet::<BindingName>::new();
    for statement in statements {
        if statement.starts_with("export {") && statement.contains("} from ") {
            let specifiers = named_reexport_specifiers(statement)?;
            if specifiers.is_empty() || specifiers.iter().any(|specifier| specifier.is_aliased) {
                return None;
            }
            direct_reexports.extend(
                specifiers
                    .into_iter()
                    .map(|specifier| BindingName::new(specifier.exported)),
            );
            continue;
        }
        if statement.starts_with("import ") {
            let specifiers = named_import_specifiers(statement)?;
            if specifiers.is_empty() || specifiers.iter().any(|specifier| specifier.is_aliased) {
                return None;
            }
            imported_locals.extend(
                specifiers
                    .into_iter()
                    .map(|specifier| BindingName::new(specifier.local)),
            );
            continue;
        }
        if statement.starts_with("export {") {
            let specifiers = local_named_export_specifiers(statement)?;
            if specifiers.is_empty() || specifiers.iter().any(|specifier| specifier.is_aliased) {
                return None;
            }
            local_exports.extend(
                specifiers
                    .into_iter()
                    .map(|specifier| BindingName::new(specifier.exported)),
            );
            continue;
        }
        return None;
    }
    if imported_locals != local_exports {
        return None;
    }
    direct_reexports.extend(local_exports);
    (!direct_reexports.is_empty()).then_some(direct_reexports)
}

pub(crate) fn extra_exports_for_module<'a>(
    _program: &EnrichedProgram,
    _module_id: ModuleId,
    sources: impl IntoIterator<Item = Option<&'a BTreeSet<BindingName>>>,
) -> BTreeSet<BindingName> {
    let mut exports = BTreeSet::new();
    for source in sources.into_iter().flatten() {
        exports.extend(source.iter().cloned());
    }
    exports
}

pub(crate) fn unique_source_definition_modules(
    program: &EnrichedProgram,
    externalized_packages: &BTreeSet<ModuleId>,
) -> BTreeMap<BindingName, Option<ModuleId>> {
    let definition_bindings_by_module = source_definition_bindings_by_module(program);
    unique_source_definition_modules_from_bindings(
        program,
        externalized_packages,
        &definition_bindings_by_module,
    )
}

pub(crate) fn runtime_owner_definition_modules(
    program: &EnrichedProgram,
    externalized_packages: &BTreeSet<ModuleId>,
) -> BTreeMap<BindingName, Option<ModuleId>> {
    let definition_bindings_by_module = runtime_owner_definition_bindings_by_module(program);
    unique_source_definition_modules_from_bindings(
        program,
        externalized_packages,
        &definition_bindings_by_module,
    )
}

pub(crate) fn runtime_owner_definition_modules_by_source(
    program: &EnrichedProgram,
    externalized_packages: &BTreeSet<ModuleId>,
) -> BTreeMap<u32, BTreeMap<BindingName, Option<ModuleId>>> {
    let definition_bindings_by_module = runtime_owner_definition_bindings_by_module(program);
    unique_source_definition_modules_by_source_from_bindings(
        program,
        externalized_packages,
        &definition_bindings_by_module,
    )
}

pub(crate) fn runtime_owner_definition_bindings_by_module(
    program: &EnrichedProgram,
) -> BTreeMap<ModuleId, BTreeSet<BindingName>> {
    let mut definition_bindings_by_module = source_definition_bindings_by_module(program);
    for symbol in program.model().symbols() {
        definition_bindings_by_module
            .entry(symbol.module_id)
            .or_default()
            .insert(BindingName::new(symbol.name.clone()));
    }
    definition_bindings_by_module
}

pub(crate) fn source_definition_bindings_by_module(
    program: &EnrichedProgram,
) -> BTreeMap<ModuleId, BTreeSet<BindingName>> {
    program
        .model()
        .modules()
        .iter()
        .map(|module| (module.id, source_definition_bindings(program, module.id)))
        .collect()
}

pub(crate) fn unique_source_definition_modules_from_bindings(
    program: &EnrichedProgram,
    externalized_packages: &BTreeSet<ModuleId>,
    definition_bindings_by_module: &BTreeMap<ModuleId, BTreeSet<BindingName>>,
) -> BTreeMap<BindingName, Option<ModuleId>> {
    let mut definitions = BTreeMap::<BindingName, Option<ModuleId>>::new();
    for module in program.model().modules() {
        if module.kind == ModuleKind::Package && externalized_packages.contains(&module.id) {
            continue;
        }
        let Some(source_definitions) = definition_bindings_by_module.get(&module.id) else {
            continue;
        };
        for binding in source_definitions {
            definitions
                .entry(binding.clone())
                .and_modify(|module_id| *module_id = None)
                .or_insert(Some(module.id));
        }
    }
    definitions
}

pub(crate) fn unique_source_definition_modules_by_source_from_bindings(
    program: &EnrichedProgram,
    externalized_packages: &BTreeSet<ModuleId>,
    definition_bindings_by_module: &BTreeMap<ModuleId, BTreeSet<BindingName>>,
) -> BTreeMap<u32, BTreeMap<BindingName, Option<ModuleId>>> {
    let mut definitions = BTreeMap::<u32, BTreeMap<BindingName, Option<ModuleId>>>::new();
    for module in program.model().modules() {
        if module.kind == ModuleKind::Package && externalized_packages.contains(&module.id) {
            continue;
        }
        let Some(source_file_id) = module.source_file_id else {
            continue;
        };
        let Some(source_definitions) = definition_bindings_by_module.get(&module.id) else {
            continue;
        };
        let definitions_for_source = definitions.entry(source_file_id).or_default();
        for binding in source_definitions {
            definitions_for_source
                .entry(binding.clone())
                .and_modify(|module_id| *module_id = None)
                .or_insert(Some(module.id));
        }
    }
    definitions
}

pub(crate) fn module_output_path(program: &EnrichedProgram, module_id: ModuleId) -> Option<String> {
    let module = program
        .model()
        .modules()
        .iter()
        .find(|module| module.id == module_id)?;
    let raw_path = program
        .semantic_names()
        .module_path(module.id)
        .unwrap_or(module.semantic_path.as_str());
    Some(normalized_module_output_path(module.id, raw_path))
}

pub(crate) fn normalized_module_output_path(module_id: ModuleId, raw_path: &str) -> String {
    let raw_path = raw_path.trim();
    if is_safe_typescript_module_path(raw_path) {
        return raw_path.to_string();
    }
    let slug = output_path_slug(strip_source_extension(raw_path));
    format!("modules/{}-{slug}.ts", module_id.0)
}

fn is_safe_typescript_module_path(path: &str) -> bool {
    if !path.ends_with(".ts") && !path.ends_with(".tsx") {
        return false;
    }
    path.split('/').all(|segment| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    })
}

fn strip_source_extension(path: &str) -> &str {
    for extension in [".tsx", ".ts", ".jsx", ".mjs", ".cjs", ".js"] {
        if let Some(stripped) = path.strip_suffix(extension) {
            return stripped;
        }
    }
    path
}

fn output_path_slug(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut last_was_separator = false;
    for ch in value.chars() {
        let mapped = if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '/') {
            ch
        } else {
            '-'
        };
        if mapped == '-' {
            if last_was_separator {
                continue;
            }
            last_was_separator = true;
        } else {
            last_was_separator = false;
        }
        output.push(mapped);
    }
    let trimmed = output.trim_matches(|ch| matches!(ch, '-' | '/' | '.'));
    if trimmed.is_empty() {
        "module".to_string()
    } else {
        trimmed.to_string()
    }
}

pub(crate) fn runtime_helper_kinds(
    graph: &RevertsGraph,
    runtime_imports: &[RuntimePreludeImport],
) -> BTreeMap<BindingName, RuntimePreludeBindingKind> {
    let mut helpers = BTreeMap::new();
    for import in runtime_imports {
        let Some(prelude) = graph.runtime_prelude(import.source_file_id) else {
            continue;
        };
        let Some(kind) = prelude.binding_kind(&import.binding) else {
            continue;
        };
        helpers.insert(import.binding.clone(), kind);
    }
    helpers
}

/// Parse the parent source file id from a synthetic source path of the form
/// `__reverts_synthetic__/<parent_id>/<name>.js` (produced by reverts-bundle
/// for reconstructed esbuild multi-handle modules). Returns `None` for any
/// non-synthetic path.
pub(crate) fn synthetic_parent_source_file_id(path: &str) -> Option<u32> {
    path.strip_prefix("__reverts_synthetic__/")?
        .split_once('/')
        .and_then(|(parent, _rest)| parent.parse::<u32>().ok())
}

pub(crate) fn runtime_helper_kinds_for_source(
    graph: &RevertsGraph,
    source_file_id: u32,
    source: &str,
) -> BTreeMap<BindingName, RuntimePreludeBindingKind> {
    let source_identifiers = identifiers_in_source(source)
        .into_iter()
        .map(BindingName::new)
        .collect::<BTreeSet<_>>();
    graph
        .runtime_prelude(source_file_id)
        .map(|prelude| {
            prelude
                .bindings
                .iter()
                .filter(|(binding, _kind)| source_identifiers.contains(*binding))
                .map(|(binding, kind)| (binding.clone(), *kind))
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeHelperLowering {
    source: String,
    lowered_helpers: BTreeSet<BindingName>,
    remaining_helpers: BTreeSet<BindingName>,
    uses_lazy_module: bool,
    uses_lazy_value: bool,
    /// Top-level bindings whose shape changed during delazify / namespace
    /// decomposition. The IR-derived `BindingShape` from the pre-lowering
    /// solution no longer matches the emitted RHS (e.g. a binding that was
    /// `Callable` because it wrapped `lazyValue(...)` is now a plain value
    /// literal), so the planner must downgrade these bindings' shape to
    /// `Unknown` to keep the audit consistent.
    reshaped_bindings: BTreeSet<BindingName>,
}

#[cfg(test)]
pub(crate) fn lower_runtime_helpers(
    source: &str,
    helper_kinds: &BTreeMap<BindingName, RuntimePreludeBindingKind>,
    exported_bindings: &BTreeSet<BindingName>,
    eager_safe_call_targets: &BTreeSet<String>,
) -> RuntimeHelperLowering {
    lower_runtime_helpers_with_options(
        source,
        helper_kinds,
        exported_bindings,
        eager_safe_call_targets,
        true,
    )
}

pub(crate) fn lower_runtime_helpers_with_options(
    source: &str,
    helper_kinds: &BTreeMap<BindingName, RuntimePreludeBindingKind>,
    exported_bindings: &BTreeSet<BindingName>,
    eager_safe_call_targets: &BTreeSet<String>,
    allow_local_lazy_value_wrappers: bool,
) -> RuntimeHelperLowering {
    let mut lowered = source.to_string();
    let mut lowered_helpers = BTreeSet::new();
    let mut uses_lazy_module = false;
    let mut uses_lazy_value = false;
    for (helper, kind) in helper_kinds {
        let result = match kind {
            RuntimePreludeBindingKind::CommonJsWrapper => {
                lower_commonjs_wrapper_helper(lowered.as_str(), helper.as_str())
            }
            RuntimePreludeBindingKind::LazyInitializer => {
                lower_lazy_initializer_helper(lowered.as_str(), helper.as_str())
            }
            RuntimePreludeBindingKind::SourceBacked => None,
        };
        if let Some(next) = result {
            let helper_removed = !contains_call_to_identifier(next.as_str(), helper.as_str());
            lowered = next;
            if helper_removed {
                lowered_helpers.insert(helper.clone());
            }
            match kind {
                RuntimePreludeBindingKind::CommonJsWrapper => uses_lazy_module = true,
                RuntimePreludeBindingKind::LazyInitializer => uses_lazy_value = true,
                RuntimePreludeBindingKind::SourceBacked => {}
            }
        }
    }
    let remaining_helpers: BTreeSet<BindingName> = helper_kinds
        .iter()
        .filter(|(helper, kind)| match kind {
            RuntimePreludeBindingKind::CommonJsWrapper
            | RuntimePreludeBindingKind::LazyInitializer => {
                contains_call_to_identifier(lowered.as_str(), helper.as_str())
            }
            RuntimePreludeBindingKind::SourceBacked => {
                contains_identifier_reference(lowered.as_str(), helper.as_str())
            }
        })
        .map(|(helper, _kind)| helper)
        .cloned()
        .collect();
    // Final pass: collapse trivial `var X = lazyValue(() => { return EXPR; })`
    // and `var X = lazyModule((e, m) => { m.exports = EXPR; })` back to
    // `var X = EXPR;` when EXPR is pure and X is only used as an immediate
    // call `X()`. This unwinds the bundler's lazy memoization for values
    // that never needed it, so the emitted code reads like the pre-bundle
    // source instead of a lazy thunk wrap.
    let mut reshaped_bindings = BTreeSet::<BindingName>::new();
    if uses_lazy_value || uses_lazy_module {
        let (next, delazified) =
            delazify_pure_value_bindings(&lowered, exported_bindings, eager_safe_call_targets);
        lowered = next;
        reshaped_bindings.extend(delazified);
        // After delazify produced `var X = { fn1, fn2, ... };` for CommonJS
        // modules whose body was `exports.fn1 = ...; exports.fn2 = ...;`,
        // explode the namespace object back into individual top-level
        // bindings — restoring the `export function ...` shape consumers
        // would have written in ESM. Safe only for objects whose every value
        // is a function/class/arrow expression (API surfaces), and whose
        // binding is accessed exclusively via `X.<key>` member access.
        let (next, decomposed) = decompose_function_namespace_objects(&lowered, exported_bindings);
        lowered = next;
        reshaped_bindings.extend(decomposed);
        let can_inline_remaining_lazy_values = {
            let local_definitions = top_level_definitions_in_source(&lowered);
            remaining_helpers
                .iter()
                .all(|binding| local_definitions.contains(binding))
        };
        // Anything that survived the pure-value delazify pass still needs
        // lazy CommonJS semantics, but it does not need to import the shared
        // `lazyModule` helper. Inline the tiny memoizing wrapper locally so
        // thousands of recovered CJS modules stop depending on the central
        // runtime helper file just for this loader.
        let (next, _inlined_lazy_modules) = inline_remaining_lazy_module_wrappers(&lowered);
        lowered = next;
        // Same for lazyValue thunks that must remain callable (exported,
        // first-class, or impure bodies): keep memoization semantics, but make
        // the wrapper local to the declaring module when that removes the
        // only runtime-helper edge. If the module already needs runtime
        // bindings, keeping the shared helper is smaller and avoids replacing
        // one import specifier with another generated local var.
        if allow_local_lazy_value_wrappers && can_inline_remaining_lazy_values {
            let (next, _inlined_lazy_values) = inline_remaining_lazy_value_wrappers(&lowered);
            lowered = next;
        }
        // Re-derive the flags from the post-rewrite source: if every lazy
        // thunk collapsed, the helper module no longer needs the helper.
        uses_lazy_value = source_contains_top_level_call(&lowered, "lazyValue");
        uses_lazy_module = source_contains_top_level_call(&lowered, "lazyModule");
    }
    RuntimeHelperLowering {
        source: lowered,
        lowered_helpers,
        remaining_helpers,
        uses_lazy_module,
        uses_lazy_value,
        reshaped_bindings,
    }
}

/// Whether `source` contains an immediate-call reference `<name>(...)` that
/// isn't inside a string, comment, template, or regex literal. Used to
/// determine whether the helper module still needs to export a runtime helper
/// after the delazify pass has had a chance to remove lazy thunks.
pub(crate) fn source_contains_top_level_call(source: &str, name: &str) -> bool {
    let bytes = source.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        if !is_identifier_start(bytes[cursor]) {
            cursor += 1;
            continue;
        }
        let start = cursor;
        cursor += 1;
        while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
            cursor += 1;
        }
        if &source[start..cursor] != name {
            continue;
        }
        let after = skip_ws(bytes, cursor);
        if bytes.get(after) == Some(&b'(') {
            return true;
        }
    }
    false
}

pub(crate) fn identifiers_in_source(source: &str) -> BTreeSet<String> {
    value_identifiers_in_source(source)
}

pub(crate) fn contains_call_to_identifier(source: &str, identifier: &str) -> bool {
    identifier_read_facts_in_source(source)
        .into_iter()
        .any(|fact| fact.name == identifier && fact.is_call_callee)
}

pub(crate) fn contains_identifier_reference(source: &str, identifier: &str) -> bool {
    identifier_read_facts_in_source(source)
        .into_iter()
        .any(|fact| fact.name == identifier)
}

#[cfg(test)]
pub(crate) fn identifier_reference_positions<'a>(
    source: &'a str,
    identifier: &'a str,
) -> impl Iterator<Item = usize> + 'a {
    identifier_read_facts_in_source(source)
        .into_iter()
        .filter(move |fact| fact.name == identifier)
        .map(|fact| fact.byte_end)
}

pub(crate) fn lazy_helper_import_names_for_source(
    source: &LoweredRuntimeModuleSource,
) -> Vec<&'static str> {
    let mut names = Vec::new();
    if source.uses_lazy_module {
        names.push("lazyModule");
    }
    if source.uses_lazy_value {
        names.push("lazyValue");
    }
    names
}
