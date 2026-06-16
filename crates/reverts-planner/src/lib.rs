mod binding_owner;
mod byte_lexer;
mod cli_entrypoint;
mod compiler_recovery;
mod compute_modules;
mod destructure_writes;
mod eager_safe_analysis;
mod external_package_adapter_emit;
mod identifiers;
mod import_coalesce;
mod package_runtime;
mod plan_error;
mod pure_reexport_bypass;
mod relative_paths;
mod runtime_helper_emission;
mod runtime_helper_strip;
mod runtime_helper_writes;
mod runtime_namespace_rewrite;
mod runtime_setter_migration_blocker;
mod runtime_singleton_inline;
mod runtime_source_read;
mod runtime_var_migration;
mod source_module_facts;

use runtime_source_read::{
    RuntimeBindingReadProfile, RuntimeSourceReadIndex, runtime_binding_read_profile_diagnostic,
    runtime_readers_for_binding, runtime_source_read_index,
};
mod external_adapters;
mod node_builtin_require;
mod noop_runtime_helpers;
mod runtime_orphan_prune;
mod runtime_proxy_inline;
mod statement_parsers;
mod statements;

#[cfg(test)]
mod tests;

use external_adapters::{
    ExternalPackageAdapterPlan, adapter_owned_runtime_bindings, external_package_adapter_analysis,
    populate_external_package_adapter_file,
};
use node_builtin_require::{
    NodeBuiltinRequireRewrite, rewrite_node_builtin_require_calls,
    rewrite_node_builtin_require_calls_with_imports, runtime_create_require_helpers,
};
use noop_runtime_helpers::{
    drop_bare_void_zero_top_level_statements, expand_line_removal_edits,
    localizable_noop_runtime_helpers, noop_runtime_helpers_in_source,
    private_noop_runtime_helpers_in_source, rewrite_noop_runtime_helper_calls,
    strip_runtime_noop_declarations,
};
use runtime_orphan_prune::prune_orphan_runtime_bindings;
use runtime_proxy_inline::inline_single_use_runtime_proxy_functions;

use binding_owner::{BindingOwner, BindingOwnerPlan, RuntimeOwnerImportPartition};
use package_runtime::{
    PackageRuntimeHelperKey, PackageRuntimeHelperUsage, PackageRuntimeImportEmitter,
    PackageRuntimeOwner, emit_package_runtime_helper_files, emit_package_runtime_helper_import,
    package_runtime_island_plan, partition_package_runtime_bindings,
};
use runtime_singleton_inline::{
    RuntimeSingletonInlineEmitContext, RuntimeSingletonInlinePlan,
    emit_runtime_singleton_inline_helpers, module_exported_bindings,
    partition_runtime_singleton_inline_bindings, runtime_singleton_inline_plan,
};
use runtime_var_migration::{
    RuntimeOwnedSnippetMigration, RuntimeVarMigrationPlan, compute_runtime_var_migration_plan,
};

use source_module_facts::SourceModuleFacts;

use destructure_writes::{
    array_destructuring_assignment_writes, object_destructuring_assignment_writes,
    rewrite_array_destructuring_helper_writes, rewrite_object_destructuring_helper_writes,
    split_top_level_properties,
};
use eager_safe_analysis::{
    EagerSafeAnalysis, compute_eager_safe_analysis, consumer_eagerified_imports,
    rewrite_eagerified_call_sites, should_compute_cross_module_eager_safe_analysis,
};

use runtime_helper_strip::migratable_runtime_var_initializer;
#[allow(unused_imports)]
use runtime_helper_writes::{
    inline_internal_setter_calls, is_simple_update_target, rewrite_runtime_helper_writes,
    update_operator_at,
};

use pure_reexport_bypass::{
    PureReexportBypassPlan, folded_stub_modules_with_internal_consumers, pure_reexport_bypass_plan,
};
use runtime_namespace_rewrite::rewrite_runtime_namespace_member_accesses;

use statement_parsers::{
    coalesce_consecutive_uninitialized_var_declarations, parse_generated_default_import_statement,
    parse_generated_named_export_statement, parse_generated_named_import_statement,
};

use byte_lexer::{
    expect_arrow, find_byte, find_matching_brace, find_matching_bracket, find_matching_paren,
    looks_like_regex_literal, skip_non_code_at, skip_quoted, skip_regex_literal,
    skip_template_literal, skip_ws, skip_ws_and_comments,
};
use identifiers::{
    declaration_keyword_at, declaration_keyword_at_start, find_declaration_keyword,
    is_identifier_like, is_planner_synthetic_binding, keyword_at, parse_identifier,
    parse_identifier_after_function_keyword, parse_identifier_after_keyword,
};
#[allow(unused_imports)]
use import_coalesce::{
    coalesce_top_level_import_declarations, first_local_for_import,
    import_statement_local_bindings, parse_named_import_clause,
    parse_runtime_prelude_direct_import, split_import_clause_and_specifier,
};

pub use plan_error::PlanError;
use relative_paths::relative_import_specifier;

pub use compiler_recovery::{
    CompilerRecoveryAction, CompilerRecoveryDecision, SourceCompilerStrategy,
};
pub use runtime_setter_migration_blocker::{
    RuntimeSetterMigrationBindingKey, RuntimeSetterMigrationBindingStatus,
    RuntimeSetterMigrationBlockerReason, RuntimeSetterMigrationBlockerReport,
};

#[allow(unused_imports)]
use statements::{
    default_import_statement, default_named_import_alias_statement, named_export_statement,
    named_import_alias_statement, named_import_statement, named_reexport_statement,
    namespace_import_statement, node_require_prelude_statement, noop_function_statement,
    runtime_helper_import_statement, runtime_helper_setter_declarations,
    runtime_helper_setter_name, runtime_helpers_path, runtime_namespace_export_statement,
    variable_declaration_statement,
};

use std::collections::{BTreeMap, BTreeSet};

use reverts_graph::{
    RevertsGraph, RuntimeEntrypoint, RuntimeNamespaceExport, RuntimePrelude,
    RuntimePreludeBindingKind, RuntimePreludeImport,
};
use reverts_input::{
    ModuleDependencyTarget, ModuleInput, PackageAttributionInput, PackageAttributionStatus,
    PackageEmissionMode,
};
use reverts_ir::{BindingName, BindingShape, ModuleId, ModuleKind};
use reverts_js::{
    ParseGoal, collect_identifier_read_facts, collect_top_level_statement_facts,
    format_source_pretty, is_ascii_identifier_continue as is_identifier_continue,
    is_ascii_identifier_start as is_identifier_start, is_js_keyword, skip_block_comment,
    skip_line_comment,
};
use reverts_model::EnrichedProgram;
use reverts_package::{PackageResolution, accepted_external_module_ids};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EmitPlan {
    pub files: Vec<PlannedFile>,
}

impl EmitPlan {
    pub fn push_file(&mut self, mut file: PlannedFile) {
        file.coalesce_consecutive_uninitialized_var_declarations();
        file.coalesce_generated_named_imports();
        file.coalesce_generated_default_named_imports();
        file.coalesce_generated_named_exports();
        self.files.push(file);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub path: String,
    pub imports: Vec<PlannedImport>,
    pub bindings: Vec<PlannedBinding>,
    pub exports: Vec<PlannedExport>,
    /// Late, readability-only binding renames. These are applied by the
    /// emitter after all source recovery/lowering is complete but before
    /// final codegen and parse audit, so graph/planner facts stay keyed by
    /// original recovered names.
    pub readability_renames: Vec<PlannedRename>,
    pub body: Vec<String>,
    pub compiler_recovery: CompilerRecoveryDecision,
}

impl PlannedFile {
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            imports: Vec::new(),
            bindings: Vec::new(),
            exports: Vec::new(),
            readability_renames: Vec::new(),
            body: Vec::new(),
            compiler_recovery: CompilerRecoveryDecision::default(),
        }
    }

    pub fn add_import(&mut self, import: PlannedImport) {
        self.imports.push(import);
    }

    pub fn add_binding(&mut self, binding: PlannedBinding) {
        self.bindings.push(binding);
    }

    pub fn add_export(&mut self, binding: BindingName) {
        self.add_export_with_source_backed(binding, false);
    }

    pub fn add_export_with_source_backed(&mut self, binding: BindingName, source_backed: bool) {
        self.exports.push(PlannedExport {
            binding,
            source_backed,
        });
    }

    pub fn push_source(&mut self, source: impl Into<String>) {
        self.body.push(source.into());
    }

    pub fn add_readability_rename(&mut self, rename: PlannedRename) {
        self.readability_renames.push(rename);
    }

    pub fn set_compiler_recovery(&mut self, compiler_recovery: CompilerRecoveryDecision) {
        self.compiler_recovery = compiler_recovery;
    }

    fn coalesce_generated_named_imports(&mut self) {
        let mut imports_by_specifier = BTreeMap::<String, BTreeSet<BindingName>>::new();
        let mut first_index_by_specifier = BTreeMap::<String, usize>::new();
        let mut duplicate_indices = BTreeSet::<usize>::new();
        for (index, source) in self.body.iter().enumerate() {
            let Some((bindings, specifier)) = parse_generated_named_import_statement(source) else {
                continue;
            };
            imports_by_specifier
                .entry(specifier.clone())
                .or_default()
                .extend(bindings);
            use std::collections::btree_map::Entry;
            match first_index_by_specifier.entry(specifier) {
                Entry::Vacant(entry) => {
                    entry.insert(index);
                }
                Entry::Occupied(_) => {
                    duplicate_indices.insert(index);
                }
            }
        }
        if duplicate_indices.is_empty() {
            return;
        }
        let mut replacements = BTreeMap::<usize, String>::new();
        for (specifier, index) in first_index_by_specifier {
            let Some(bindings) = imports_by_specifier.get(&specifier) else {
                continue;
            };
            replacements.insert(
                index,
                named_import_statement(bindings.iter(), specifier.as_str()),
            );
        }
        let mut merged =
            Vec::with_capacity(self.body.len().saturating_sub(duplicate_indices.len()));
        for (index, source) in self.body.iter().enumerate() {
            if duplicate_indices.contains(&index) {
                continue;
            }
            if let Some(replacement) = replacements.get(&index) {
                merged.push(replacement.clone());
            } else {
                merged.push(source.clone());
            }
        }
        self.body = merged;
    }

    fn coalesce_generated_default_named_imports(&mut self) {
        let mut named_by_specifier = BTreeMap::<String, (usize, BTreeSet<BindingName>)>::new();
        let mut defaults_by_specifier = BTreeMap::<String, Vec<(usize, BindingName)>>::new();
        for (index, source) in self.body.iter().enumerate() {
            if let Some((bindings, specifier)) = parse_generated_named_import_statement(source) {
                named_by_specifier.insert(specifier, (index, bindings));
                continue;
            }
            if let Some((binding, specifier)) = parse_generated_default_import_statement(source) {
                defaults_by_specifier
                    .entry(specifier)
                    .or_default()
                    .push((index, binding));
            }
        }

        let mut removals = BTreeSet::<usize>::new();
        let mut replacements = BTreeMap::<usize, String>::new();
        for (specifier, (named_index, bindings)) in named_by_specifier {
            let Some(defaults) = defaults_by_specifier.get(&specifier) else {
                continue;
            };
            let [(default_index, default_binding)] = defaults.as_slice() else {
                continue;
            };
            let replacement_index = (*default_index).min(named_index);
            let removed_index = (*default_index).max(named_index);
            replacements.insert(
                replacement_index,
                default_named_import_alias_statement(
                    default_binding,
                    bindings.iter().map(|binding| (binding.as_str(), binding)),
                    specifier.as_str(),
                ),
            );
            removals.insert(removed_index);
        }
        if removals.is_empty() {
            return;
        }

        let mut merged = Vec::with_capacity(self.body.len().saturating_sub(removals.len()));
        for (index, source) in self.body.iter().enumerate() {
            if removals.contains(&index) {
                continue;
            }
            if let Some(replacement) = replacements.get(&index) {
                merged.push(replacement.clone());
            } else {
                merged.push(source.clone());
            }
        }
        self.body = merged;
    }

    fn coalesce_generated_named_exports(&mut self) {
        let mut exported_bindings = BTreeSet::<BindingName>::new();
        let mut first_index = None::<usize>;
        let mut duplicate_indices = BTreeSet::<usize>::new();
        for (index, source) in self.body.iter().enumerate() {
            let Some(bindings) = parse_generated_named_export_statement(source) else {
                continue;
            };
            exported_bindings.extend(bindings);
            if first_index.is_none() {
                first_index = Some(index);
            } else {
                duplicate_indices.insert(index);
            }
        }
        if duplicate_indices.is_empty() {
            return;
        }
        let Some(first_index) = first_index else {
            return;
        };
        let replacement = named_export_statement(exported_bindings.iter());
        let mut merged =
            Vec::with_capacity(self.body.len().saturating_sub(duplicate_indices.len()));
        for (index, source) in self.body.iter().enumerate() {
            if duplicate_indices.contains(&index) {
                continue;
            }
            if index == first_index {
                merged.push(replacement.clone());
            } else {
                merged.push(source.clone());
            }
        }
        self.body = merged;
    }

    fn coalesce_consecutive_uninitialized_var_declarations(&mut self) {
        for source in &mut self.body {
            *source = coalesce_consecutive_uninitialized_var_declarations(source);
        }
    }

    #[must_use]
    pub const fn source_strategy(&self) -> SourceCompilerStrategy {
        self.compiler_recovery.strategy
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedImport {
    pub namespace: BindingName,
    pub resolution: PackageResolution,
    pub source_backed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedRename {
    pub original: BindingName,
    pub renamed: BindingName,
}

impl PlannedRename {
    #[must_use]
    pub fn new(original: BindingName, renamed: BindingName) -> Self {
        Self { original, renamed }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedBinding {
    pub original: BindingName,
    pub emitted: BindingName,
    pub shape: BindingShape,
    pub source_backed: bool,
    /// Paper #7 downstream: property names observed on this binding when
    /// shape is `NamespaceObject`. Empty for every other shape and for
    /// namespaces whose members the solver could not see.
    pub known_members: BTreeSet<BindingName>,
}

impl PlannedBinding {
    #[must_use]
    pub fn new(
        original: BindingName,
        emitted: BindingName,
        shape: BindingShape,
        source_backed: bool,
    ) -> Self {
        Self {
            original,
            emitted,
            shape,
            source_backed,
            known_members: BTreeSet::new(),
        }
    }

    #[must_use]
    pub fn with_known_members(mut self, known_members: BTreeSet<BindingName>) -> Self {
        self.known_members = known_members;
        self
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedExport {
    pub binding: BindingName,
    pub source_backed: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ImportExportPlanner;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannerAnalysis {
    external_package_adapters: BTreeMap<ModuleId, ExternalPackageAdapterPlan>,
    externalized_packages: BTreeSet<ModuleId>,
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
        let source_module_wiring =
            source_module_wiring(program, &externalized_packages, &source_facts);
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_source_module_imports(
    program: &EnrichedProgram,
    module_id: ModuleId,
    path: &str,
    source_module_wiring: &SourceModuleWiring,
    pure_reexport_bypasses: &PureReexportBypassPlan,
    runtime_lazy_folds: &RuntimeLazyFoldPlan,
    omitted_folded_stub_modules: &BTreeSet<ModuleId>,
    binding_owners: &BindingOwnerPlan,
    file: &mut PlannedFile,
    planned_bindings: &mut BTreeSet<BindingName>,
    used_runtime_helper_files: &mut BTreeMap<u32, BTreeSet<BindingName>>,
    exported_runtime_helper_bindings: &mut BTreeMap<u32, BTreeSet<BindingName>>,
) -> bool {
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_localized_noop_runtime_helpers(
    program: &EnrichedProgram,
    module_id: ModuleId,
    source_module_wiring: &SourceModuleWiring,
    lowered_source: Option<&LoweredRuntimeModuleSource>,
    namespace_member_rewrite: Option<
        &runtime_namespace_rewrite::RuntimeNamespaceMemberAccessRewrite,
    >,
    node_builtin_require_rewrite: Option<&NodeBuiltinRequireRewrite>,
    remaining_runtime_helpers: &BTreeSet<BindingName>,
) -> BTreeSet<BindingName> {
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_runtime_import_partitions(
    program: &EnrichedProgram,
    module_id: ModuleId,
    runtime_import_groups: BTreeMap<u32, BTreeSet<BindingName>>,
    binding_owners: &BindingOwnerPlan,
    namespace_member_rewrite: Option<
        &runtime_namespace_rewrite::RuntimeNamespaceMemberAccessRewrite,
    >,
    source_runtime_refs: &BTreeSet<BindingName>,
    lowered_helpers: &BTreeSet<BindingName>,
    written_runtime_helpers: &BTreeSet<BindingName>,
    consumed_node_builtin_require_helpers: &BTreeSet<BindingName>,
    localized_noop_runtime_helpers: &BTreeSet<BindingName>,
    remaining_runtime_helpers: &BTreeSet<BindingName>,
    planned_bindings: &BTreeSet<BindingName>,
    local_source_definitions: &BTreeSet<BindingName>,
    local_source_writes: &BTreeSet<BindingName>,
) -> Vec<(u32, RuntimeOwnerImportPartition)> {
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn try_localize_lazy_value(
    lowered_source: Option<&LoweredRuntimeModuleSource>,
    namespace_member_rewrite: Option<
        &runtime_namespace_rewrite::RuntimeNamespaceMemberAccessRewrite,
    >,
    has_runtime_edge_before_lazy_helpers: bool,
    written_runtime_helpers: &BTreeSet<BindingName>,
    has_runtime_group_imports: bool,
    runtime_singleton_inlines: &RuntimeSingletonInlinePlan,
    module_id: ModuleId,
    lowered_runtime_bindings: &BTreeSet<BindingName>,
) -> Option<String> {
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
    migrated_extra_runtime_setter_deps_by_source: &BTreeMap<u32, BTreeSet<BindingName>>,
) {
    if migrated_extra_snippets.is_empty() && migrated_extra_namespace_exports.is_empty() {
        return;
    }
    let mut migrated_chunks = Vec::<(u32, u8, String)>::new();
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
        migrated_chunks.push((
            namespace_export.byte_start,
            1,
            rename_identifier_reads_in_source(
                runtime_namespace_export_statement(namespace_export).as_str(),
                &migrated_runtime_dep_aliases,
            ),
        ));
    }
    migrated_chunks.sort_by_key(|(byte_start, kind, _source)| (*byte_start, *kind));
    for (_, _, source) in migrated_chunks {
        file.push_source(source);
    }
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
        if source_backed && emitted != original {
            file.add_readability_rename(PlannedRename::new(original.clone(), emitted.clone()));
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_lowered_package_runtime_imports(
    program: &EnrichedProgram,
    module_id: ModuleId,
    module_path: &str,
    file: &mut PlannedFile,
    planned_bindings: &mut BTreeSet<BindingName>,
    used_package_runtime_helper_files: &mut BTreeMap<
        PackageRuntimeHelperKey,
        PackageRuntimeHelperUsage,
    >,
    source_file_id: u32,
    binding_owners: &BindingOwnerPlan,
    package_runtime_owner: Option<&PackageRuntimeOwner>,
    remaining_runtime_helpers: &BTreeSet<BindingName>,
    written_runtime_helpers: &BTreeSet<BindingName>,
) -> (
    BTreeSet<BindingName>,
    BTreeSet<BindingName>,
    BTreeSet<BindingName>,
    BTreeSet<BindingName>,
) {
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn try_post_inline_localize_lazy_value(
    lowered_source: &LoweredRuntimeModuleSource,
    namespace_member_rewrite: Option<
        &runtime_namespace_rewrite::RuntimeNamespaceMemberAccessRewrite,
    >,
    already_localized: Option<&String>,
    has_runtime_edge_before_lazy_helpers: bool,
    remaining_runtime_helpers: &BTreeSet<BindingName>,
    written_runtime_helpers: &BTreeSet<BindingName>,
    package_remaining_helpers: &BTreeSet<BindingName>,
    package_written_helpers: &BTreeSet<BindingName>,
    lazy_helper_names: &[&'static str],
    runtime_import_partitions: &[(u32, RuntimeOwnerImportPartition)],
    runtime_singleton_inlines: &RuntimeSingletonInlinePlan,
    binding_owners: &BindingOwnerPlan,
    package_runtime_owner: Option<&PackageRuntimeOwner>,
    module_id: ModuleId,
) -> Option<String> {
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_runtime_import_partitions(
    program: &EnrichedProgram,
    module_id: ModuleId,
    module_path: &str,
    file: &mut PlannedFile,
    planned_bindings: &mut BTreeSet<BindingName>,
    used_runtime_helper_files: &mut BTreeMap<u32, BTreeSet<BindingName>>,
    exported_runtime_helper_bindings: &mut BTreeMap<u32, BTreeSet<BindingName>>,
    required_runtime_helper_bindings: &mut BTreeMap<u32, BTreeSet<BindingName>>,
    used_package_runtime_helper_files: &mut BTreeMap<
        PackageRuntimeHelperKey,
        PackageRuntimeHelperUsage,
    >,
    emitted_inline_runtime_helpers: &mut BTreeSet<(u32, BindingName)>,
    runtime_import_partitions: Vec<(u32, RuntimeOwnerImportPartition)>,
    runtime_singleton_inlines: &RuntimeSingletonInlinePlan,
    binding_owners: &BindingOwnerPlan,
    package_runtime_owner: Option<&PackageRuntimeOwner>,
) {
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

/// Emit the runtime-helper companion import for this module's
/// lowered source — a single `import { helpers, __reverts_set_*,
/// lazyValue, lazyModule } from "runtime-helpers/..."` covering
/// remaining-read helpers, written setters, and the lazy helper
/// imports — and add `PlannedBinding` entries for the read bindings.
/// Skipped entirely when every category is empty.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_lowered_runtime_helper_import(
    program: &EnrichedProgram,
    module_id: ModuleId,
    module_path: &str,
    file: &mut PlannedFile,
    planned_bindings: &mut BTreeSet<BindingName>,
    source_file_id: u32,
    remaining_runtime_helpers: &BTreeSet<BindingName>,
    written_runtime_helpers: &BTreeSet<BindingName>,
    lazy_helper_names: &[&'static str],
) {
    if remaining_runtime_helpers.is_empty()
        && written_runtime_helpers.is_empty()
        && lazy_helper_names.is_empty()
    {
        return;
    }
    let specifier =
        relative_import_specifier(module_path, runtime_helpers_path(source_file_id).as_str());
    file.push_source(runtime_helper_import_statement(
        remaining_runtime_helpers,
        written_runtime_helpers,
        lazy_helper_names,
        specifier.as_str(),
    ));
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_lowered_runtime_helper_usage(
    lowered_source: &LoweredRuntimeModuleSource,
    remaining_runtime_helpers: &BTreeSet<BindingName>,
    written_runtime_helpers: &BTreeSet<BindingName>,
    lazy_helper_names: &[&'static str],
    used_runtime_helper_files: &mut BTreeMap<u32, BTreeSet<BindingName>>,
    exported_runtime_helper_bindings: &mut BTreeMap<u32, BTreeSet<BindingName>>,
    required_runtime_helper_bindings: &mut BTreeMap<u32, BTreeSet<BindingName>>,
    used_runtime_helper_setters: &mut BTreeMap<u32, BTreeSet<BindingName>>,
    used_lazy_module: &mut BTreeSet<u32>,
    used_lazy_value: &mut BTreeSet<u32>,
    exported_lazy_module: &mut BTreeSet<u32>,
    exported_lazy_value: &mut BTreeSet<u32>,
) {
    let source_file_id = lowered_source.source_file_id;
    if !remaining_runtime_helpers.is_empty()
        || !written_runtime_helpers.is_empty()
        || !lazy_helper_names.is_empty()
    {
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_migrated_runtime_extra_alias_imports(
    module_path: &str,
    file: &mut PlannedFile,
    planned_bindings: &mut BTreeSet<BindingName>,
    used_runtime_helper_files: &mut BTreeMap<u32, BTreeSet<BindingName>>,
    exported_runtime_helper_bindings: &mut BTreeMap<u32, BTreeSet<BindingName>>,
    required_runtime_helper_bindings: &mut BTreeMap<u32, BTreeSet<BindingName>>,
    migrated_runtime_extra_runtime_dep_aliases: &BTreeMap<u32, BTreeMap<BindingName, BindingName>>,
) {
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_migrated_extra_runtime_reexport_imports(
    program: &EnrichedProgram,
    module_id: ModuleId,
    module_path: &str,
    file: &mut PlannedFile,
    planned_bindings: &mut BTreeSet<BindingName>,
    used_runtime_helper_files: &mut BTreeMap<u32, BTreeSet<BindingName>>,
    exported_runtime_helper_bindings: &mut BTreeMap<u32, BTreeSet<BindingName>>,
    migrated_extra_runtime_reexport_deps: &BTreeMap<u32, BTreeSet<BindingName>>,
) {
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_migrated_extra_owner_imports(
    program: &EnrichedProgram,
    module_id: ModuleId,
    module_path: &str,
    file: &mut PlannedFile,
    planned_bindings: &mut BTreeSet<BindingName>,
    migrated_extra_source_deps: &BTreeMap<ModuleId, BTreeSet<BindingName>>,
    migrated_extra_runtime_owner_deps: &BTreeMap<ModuleId, BTreeSet<BindingName>>,
    migrated_extra_runtime_owner_dep_aliases: &BTreeMap<
        ModuleId,
        BTreeMap<BindingName, BindingName>,
    >,
) {
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn filter_remaining_helpers_by_write_rewrite(
    program: &EnrichedProgram,
    module_id: ModuleId,
    source_module_wiring: &SourceModuleWiring,
    lowered_source: Option<&LoweredRuntimeModuleSource>,
    namespace_member_rewrite: Option<
        &runtime_namespace_rewrite::RuntimeNamespaceMemberAccessRewrite,
    >,
    node_builtin_require_rewrite: Option<&NodeBuiltinRequireRewrite>,
    written_runtime_helpers: &BTreeSet<BindingName>,
    migrated_extra_runtime_deps: &BTreeSet<BindingName>,
    remaining_runtime_helpers: BTreeSet<BindingName>,
) -> BTreeSet<BindingName> {
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn push_folded_noop_and_migrated_exports(
    folded: &RuntimeLazyFoldModule,
    runtime_stub_exports: &BTreeSet<BindingName>,
    direct_stub_exports: &BTreeMap<ModuleId, BTreeSet<BindingName>>,
    migrated_extra_noop_deps: &BTreeSet<BindingName>,
    migrated_local_bindings: &BTreeSet<BindingName>,
    migrated_extra_namespace_bindings: &BTreeSet<BindingName>,
    file: &mut PlannedFile,
    planned_bindings: &mut BTreeSet<BindingName>,
) {
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
    file: &mut PlannedFile,
) {
    if migrated_extra_snippets.is_empty() && migrated_extra_namespace_exports.is_empty() {
        return;
    }
    let migrated_extra_runtime_dep_aliases = migrated_extra_runtime_dep_aliases
        .values()
        .flat_map(|aliases| aliases.iter())
        .map(|(original, alias)| (original.clone(), alias.clone()))
        .collect::<BTreeMap<_, _>>();
    let migrated_extra_runtime_dep_aliases = &migrated_extra_runtime_dep_aliases;
    let mut migrated_chunks = Vec::<(u32, u8, String)>::new();
    for (source_file_id, binding) in migrated_extra_snippets {
        let Some(prelude) = program.model().graph().runtime_prelude(*source_file_id) else {
            continue;
        };
        let Some(snippet) = prelude.snippets.get(binding) else {
            continue;
        };
        migrated_chunks.push((
            snippet.byte_start,
            0,
            rename_identifier_reads_in_source(
                snippet.source.as_str(),
                migrated_extra_runtime_dep_aliases,
            ),
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
        migrated_chunks.push((
            namespace_export.byte_start,
            1,
            rename_identifier_reads_in_source(
                runtime_namespace_export_statement(namespace_export).as_str(),
                migrated_extra_runtime_dep_aliases,
            ),
        ));
    }
    migrated_chunks.sort_by_key(|(byte_start, kind, _source)| (*byte_start, *kind));
    for (_, _, source) in migrated_chunks {
        file.push_source(source);
    }
}

/// Emit `import { X, Y } from runtime` for each (source_file_id, bindings)
/// pair the migration plan recorded as still needed by this owner module
/// after its lazy fold. Each binding is registered in the helper-file /
/// exported / required indexes and turned into a `PlannedBinding`
/// derived from the program's shape/known-members data.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_runtime_extra_deps_imports(
    program: &EnrichedProgram,
    module_id: ModuleId,
    path: &str,
    deps_by_source: &BTreeMap<u32, BTreeSet<BindingName>>,
    file: &mut PlannedFile,
    planned_bindings: &mut BTreeSet<BindingName>,
    used_runtime_helper_files: &mut BTreeMap<u32, BTreeSet<BindingName>>,
    exported_runtime_helper_bindings: &mut BTreeMap<u32, BTreeSet<BindingName>>,
    required_runtime_helper_bindings: &mut BTreeMap<u32, BTreeSet<BindingName>>,
) {
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
        let analysis = PlannerAnalysis::from_program(program);
        runtime_setter_migration_blocker_report(
            program,
            &analysis.source_module_wiring,
            &analysis.lowered_runtime_sources,
            &analysis.runtime_lazy_folds,
            &analysis.source_suppressed_packages,
        )
    }

    pub fn plan_enriched_program(self, program: &EnrichedProgram) -> Result<EmitPlan, PlanError> {
        let mut plan = EmitPlan::default();
        let mut used_runtime_helper_files = BTreeMap::<u32, BTreeSet<BindingName>>::new();
        let mut exported_runtime_helper_bindings = BTreeMap::<u32, BTreeSet<BindingName>>::new();
        let mut required_runtime_helper_bindings = BTreeMap::<u32, BTreeSet<BindingName>>::new();
        let mut used_runtime_helper_setters = BTreeMap::<u32, BTreeSet<BindingName>>::new();
        let mut used_lazy_module = BTreeSet::<u32>::new();
        let mut used_lazy_value = BTreeSet::<u32>::new();
        let mut exported_lazy_module = BTreeSet::<u32>::new();
        let mut exported_lazy_value = BTreeSet::<u32>::new();
        let analysis = PlannerAnalysis::from_program(program);
        let external_package_adapters = &analysis.external_package_adapters;
        let externalized_packages = &analysis.externalized_packages;
        let source_suppressed_packages = &analysis.source_suppressed_packages;
        let source_module_wiring = &analysis.source_module_wiring;
        let lowered_runtime_sources = &analysis.lowered_runtime_sources;
        let runtime_lazy_folds = &analysis.runtime_lazy_folds;
        let omitted_folded_stub_modules =
            folded_stub_modules_with_internal_consumers(runtime_lazy_folds, source_module_wiring);
        let pure_reexport_bypasses =
            pure_reexport_bypass_plan(program, source_module_wiring, externalized_packages);
        let runtime_var_migrations = compute_runtime_var_migration_plan(
            program,
            source_module_wiring,
            lowered_runtime_sources,
            runtime_lazy_folds,
            source_suppressed_packages,
        );
        let package_runtime_islands = package_runtime_island_plan(
            program,
            lowered_runtime_sources,
            runtime_lazy_folds,
            &runtime_var_migrations,
            source_suppressed_packages,
        );
        let runtime_prelude_direct_imports = runtime_prelude_direct_imports(program);
        let runtime_singleton_inlines = runtime_singleton_inline_plan(
            program,
            source_module_wiring,
            lowered_runtime_sources,
            runtime_lazy_folds,
            &runtime_var_migrations,
            &runtime_prelude_direct_imports,
            source_suppressed_packages,
        );
        let mut used_package_runtime_helper_files =
            BTreeMap::<PackageRuntimeHelperKey, PackageRuntimeHelperUsage>::new();
        let runtime_edge_direct_prelude_imports = runtime_edge_direct_prelude_imports(
            program,
            lowered_runtime_sources,
            runtime_lazy_folds,
            &runtime_prelude_direct_imports,
        );
        let binding_owners = BindingOwnerPlan::from_parts(
            &runtime_var_migrations,
            &runtime_prelude_direct_imports,
            &package_runtime_islands,
        );

        detect_folded_lazy_helper_use(
            runtime_lazy_folds,
            &mut used_lazy_module,
            &mut used_lazy_value,
        );

        for module in program.model().modules() {
            compute_modules::plan_one_module(
                program,
                module,
                &mut plan,
                &mut used_runtime_helper_files,
                &mut exported_runtime_helper_bindings,
                &mut required_runtime_helper_bindings,
                &mut used_runtime_helper_setters,
                &mut used_lazy_module,
                &mut used_lazy_value,
                &mut exported_lazy_module,
                &mut exported_lazy_value,
                &mut used_package_runtime_helper_files,
                external_package_adapters,
                externalized_packages,
                source_suppressed_packages,
                source_module_wiring,
                lowered_runtime_sources,
                runtime_lazy_folds,
                &omitted_folded_stub_modules,
                &pure_reexport_bypasses,
                &runtime_var_migrations,
                &runtime_prelude_direct_imports,
                &runtime_singleton_inlines,
                &runtime_edge_direct_prelude_imports,
                &binding_owners,
            )?;
        }

        emit_package_runtime_helper_files(
            program,
            &mut plan,
            &used_package_runtime_helper_files,
            externalized_packages,
        )?;

        if let Some((_prelude, entrypoint)) = runtime_entrypoint(program) {
            used_runtime_helper_files
                .entry(entrypoint.source_file_id)
                .or_default()
                .insert(entrypoint.callee.clone());
            exported_runtime_helper_bindings
                .entry(entrypoint.source_file_id)
                .or_default()
                .insert(entrypoint.callee.clone());
            required_runtime_helper_bindings
                .entry(entrypoint.source_file_id)
                .or_default()
                .insert(entrypoint.callee.clone());
        }

        runtime_helper_emission::emit_runtime_helper_files(
            &runtime_helper_emission::RuntimeHelperEmissionContext {
                program,
                runtime_var_migrations: &runtime_var_migrations,
                binding_owners: &binding_owners,
                runtime_lazy_folds,
                externalized_packages,
                external_package_adapters,
                used_runtime_helper_files: &used_runtime_helper_files,
                exported_runtime_helper_bindings: &exported_runtime_helper_bindings,
                required_runtime_helper_bindings: &required_runtime_helper_bindings,
                used_runtime_helper_setters: &used_runtime_helper_setters,
                used_lazy_module: &used_lazy_module,
                used_lazy_value: &used_lazy_value,
                exported_lazy_module: &exported_lazy_module,
                exported_lazy_value: &exported_lazy_value,
            },
            &mut plan,
        )?;

        cli_entrypoint::emit_cli_entrypoint(program, &mut plan);

        Ok(plan)
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
    loop {
        let mut removed = Vec::<BindingName>::new();
        for (binding, owner_module) in &selected {
            let Some(snippet) = prelude.snippets.get(binding) else {
                removed.push(binding.clone());
                continue;
            };
            if read_index.entrypoint_callee.as_ref() == Some(binding)
                || read_index.namespace_export_helpers.contains(binding)
            {
                removed.push(binding.clone());
                continue;
            }

            let local_bindings = local_bindings_in_source(snippet.source.as_str());
            let runtime_reads = runtime_import_identifiers_in_source(snippet.source.as_str())
                .into_iter()
                .map(BindingName::new)
                .filter(|dep| prelude.defines(dep))
                .collect::<BTreeSet<_>>();
            let blocking_cross_owner_dep = runtime_reads.iter().find(|dep| {
                let Some(dep_owner) = selected.get(dep) else {
                    return false;
                };
                dep_owner != owner_module
                    && selected_owner_dependency_creates_cycle(
                        &selected,
                        read_index,
                        module_dependencies_by_owner,
                        *owner_module,
                        *dep_owner,
                    )
            });
            if let Some(dep) = blocking_cross_owner_dep {
                let binding_lines = snippet.source.lines().count().max(1);
                let dep_lines = prelude
                    .snippets
                    .get(dep)
                    .map(|dep_snippet| dep_snippet.source.lines().count().max(1))
                    .unwrap_or(usize::MAX);
                if dep_lines < binding_lines {
                    // Break owner<->owner cycles by pinning the smaller
                    // dependency in runtime. The larger recovered owner
                    // snippet can then import it as a stable runtime dep
                    // instead of discarding the whole recovered component.
                    removed.push(dep.clone());
                } else {
                    removed.push(binding.clone());
                }
                continue;
            }

            let runtime_writes = implicit_global_writes_in_source(snippet.source.as_str())
                .into_iter()
                .filter(|write| !local_bindings.contains(write.as_str()))
                .filter(|write| prelude.defines(write))
                .collect::<BTreeSet<_>>();
            let blocking_write = runtime_writes.iter().find(|dep| {
                selected
                    .get(dep)
                    .is_none_or(|dep_owner| dep_owner != owner_module)
            });
            if blocking_write.is_some() {
                removed.push(binding.clone());
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
                removed.push(binding.clone());
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
                removed.push(binding.clone());
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
                removed.push(binding.clone());
                continue;
            }

            if let Some(namespace_export) = read_index.namespace_exports_by_namespace.get(binding)
                && namespace_export.exports.values().any(|target| {
                    let Some(target_owner) = selected.get(target) else {
                        return false;
                    };
                    target_owner != owner_module
                        && selected_owner_dependency_creates_cycle(
                            &selected,
                            read_index,
                            module_dependencies_by_owner,
                            *owner_module,
                            *target_owner,
                        )
                })
            {
                removed.push(binding.clone());
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
            let snippet = prelude.snippets.get(&binding)?;
            let local_bindings = local_bindings_in_source(snippet.source.as_str());
            let mut extra_runtime_deps = BTreeSet::<BindingName>::new();
            let mut extra_noop_deps = BTreeSet::<BindingName>::new();
            for dep in runtime_import_identifiers_in_source(snippet.source.as_str())
                .into_iter()
                .map(BindingName::new)
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
            let runtime_writes = implicit_global_writes_in_source(snippet.source.as_str())
                .into_iter()
                .filter(|write| !local_bindings.contains(write.as_str()))
                .filter(|write| prelude.defines(write))
                .collect::<BTreeSet<_>>();
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

pub(crate) fn selected_owner_dependency_creates_cycle(
    selected: &BTreeMap<BindingName, ModuleId>,
    read_index: &RuntimeSourceReadIndex,
    module_dependencies_by_owner: &BTreeMap<ModuleId, BTreeSet<ModuleId>>,
    owner_module: ModuleId,
    dep_owner: ModuleId,
) -> bool {
    module_dependency_path_exists(module_dependencies_by_owner, dep_owner, owner_module)
        || selected_owner_path_exists(selected, read_index, dep_owner, owner_module)
}

pub(crate) fn selected_owner_path_exists(
    selected: &BTreeMap<BindingName, ModuleId>,
    read_index: &RuntimeSourceReadIndex,
    from_owner: ModuleId,
    target_owner: ModuleId,
) -> bool {
    if from_owner == target_owner {
        return true;
    }
    let mut visited = BTreeSet::<ModuleId>::new();
    let mut stack = vec![from_owner];
    while let Some(owner) = stack.pop() {
        if !visited.insert(owner) {
            continue;
        }
        for (binding, _) in selected
            .iter()
            .filter(|(_, binding_owner)| **binding_owner == owner)
        {
            for dep in selected_runtime_dependencies_for_binding(read_index, binding) {
                let Some(dep_owner) = selected.get(&dep) else {
                    continue;
                };
                if *dep_owner == owner {
                    continue;
                }
                if *dep_owner == target_owner {
                    return true;
                }
                stack.push(*dep_owner);
            }
        }
    }
    false
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
        if runtime_reader_write_setter_deps.contains(&(*source_id, binding.clone())) {
            report.add_reason(
                *source_id,
                binding.clone(),
                RuntimeSetterMigrationBlockerReason::RuntimeReaderWriteSetterDependency,
            );
            continue;
        }
        let eligible = eligible_writers
            .get(&(*source_id, binding.clone()))
            .cloned()
            .unwrap_or_default();
        if eligible.is_empty() {
            if excluded_folded.contains_key(&(*source_id, binding.clone())) {
                report.add_reason(
                    *source_id,
                    binding.clone(),
                    RuntimeSetterMigrationBlockerReason::FoldedWriterOnly,
                );
            } else if excluded_externalized.contains_key(&(*source_id, binding.clone())) {
                report.add_reason(
                    *source_id,
                    binding.clone(),
                    RuntimeSetterMigrationBlockerReason::ExternalizedPackageWriterOnly,
                );
            } else {
                report.add_reason(
                    *source_id,
                    binding.clone(),
                    RuntimeSetterMigrationBlockerReason::NoEligibleWriter,
                );
            }
            continue;
        }
        if eligible.len() > 1 {
            report.add_reason(
                *source_id,
                binding.clone(),
                RuntimeSetterMigrationBlockerReason::MultipleEligibleWriters,
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
                report.add_reason(
                    source_id,
                    binding,
                    RuntimeSetterMigrationBlockerReason::MissingRuntimePrelude,
                );
            }
            continue;
        };
        let mut initialized_candidates = Vec::<(BindingName, ModuleId)>::new();
        for (binding, owner_module) in candidates {
            if migratable_runtime_var_initializer(prelude, &binding).is_none() {
                report.add_reason(
                    source_id,
                    binding,
                    RuntimeSetterMigrationBlockerReason::InitializerNotMigratable,
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
            match runtime_binding_read_profile_diagnostic(&read_index, &binding) {
                Ok(RuntimeBindingReadProfile::NoReads) => {
                    report.add_reason(
                        source_id,
                        binding.clone(),
                        RuntimeSetterMigrationBlockerReason::NoDiagnosticStatus,
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
                        Ok(_) => report.add_reason(
                            source_id,
                            binding,
                            RuntimeSetterMigrationBlockerReason::ReaderClusterOverlapsMigratedBinding,
                        ),
                        Err(blocker) => report.add_reason_with_sub(
                            source_id,
                            binding,
                            blocker.into(),
                            blocker.sub_reason(),
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
                                Ok(_) => report.add_reason(
                                    source_id,
                                    binding,
                                    RuntimeSetterMigrationBlockerReason::ReaderClusterOverlapsMigratedBinding,
                                ),
                                Err(blocker) => report.add_reason_with_sub(
                                    source_id,
                                    binding,
                                    blocker.into(),
                                    blocker.sub_reason(),
                                ),
                            }
                            continue;
                        }
                    }
                    report.add_reason(source_id, binding, reason)
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
    NamespaceTargetDifferentWriter,
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
            || ctx.movable_bindings.contains(&dep)
            || aliased_runtime_deps.contains(&dep)
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
        .is_some_and(|consumers| consumers.iter().any(|consumer| *consumer != owner_module))
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
    let runtime_writers = runtime_snippet_writers_for_binding(ctx.prelude, binding);
    !runtime_writers
        .iter()
        .any(|writer| !moved_readers.contains(writer))
}

pub(crate) fn runtime_snippet_writers_for_binding(
    prelude: &RuntimePrelude,
    binding: &BindingName,
) -> BTreeSet<BindingName> {
    prelude
        .snippets
        .iter()
        .filter_map(|(snippet_binding, snippet)| {
            let local_bindings = local_bindings_in_source(snippet.source.as_str());
            implicit_global_writes_in_source(snippet.source.as_str())
                .into_iter()
                .filter(|write| !local_bindings.contains(write.as_str()))
                .any(|write| write == *binding)
                .then(|| snippet_binding.clone())
        })
        .collect()
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
            RuntimeReaderClusterBlocker::NamespaceTargetDifferentWriter => {
                Self::NamespaceTargetDifferentWriter
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
        let local_bindings = local_bindings_in_source(snippet.source.as_str());
        let runtime_writes = implicit_global_writes_in_source(snippet.source.as_str())
            .into_iter()
            .filter(|write| !local_bindings.contains(write.as_str()))
            .filter(|write| ctx.prelude.defines(write))
            .collect::<BTreeSet<_>>();
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
            if runtime_reader_write_can_use_setter(snippet.source.as_str(), &local_bindings, &write)
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
                    return Err(RuntimeReaderClusterBlocker::NamespaceTargetDifferentWriter);
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

    let old_safe_model = gate.extra_runtime_deps.is_empty()
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
    if old_safe_model {
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

pub(crate) fn module_dependency_path_exists(
    dependencies: &BTreeMap<ModuleId, BTreeSet<ModuleId>>,
    from: ModuleId,
    target: ModuleId,
) -> bool {
    from == target
        || dependencies
            .get(&from)
            .is_some_and(|reachable| reachable.contains(&target))
}

pub(crate) fn module_dependency_modules_by_owner(
    program: &EnrichedProgram,
) -> BTreeMap<ModuleId, BTreeSet<ModuleId>> {
    let mut direct_dependencies = BTreeMap::<ModuleId, BTreeSet<ModuleId>>::new();
    let mut modules = BTreeSet::<ModuleId>::new();
    for dependency in &program.model().input().dependencies {
        let ModuleDependencyTarget::Module(target_module_id) = dependency.target else {
            continue;
        };
        modules.insert(dependency.from_module_id);
        modules.insert(target_module_id);
        direct_dependencies
            .entry(dependency.from_module_id)
            .or_default()
            .insert(target_module_id);
    }
    let mut transitive_dependencies = BTreeMap::<ModuleId, BTreeSet<ModuleId>>::new();
    for module_id in modules {
        let mut reachable = BTreeSet::<ModuleId>::new();
        let mut stack = direct_dependencies
            .get(&module_id)
            .into_iter()
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        while let Some(next) = stack.pop() {
            if !reachable.insert(next) {
                continue;
            }
            if let Some(next_modules) = direct_dependencies.get(&next) {
                stack.extend(next_modules.iter().copied());
            }
        }
        if !reachable.is_empty() {
            transitive_dependencies.insert(module_id, reachable);
        }
    }
    transitive_dependencies
}

pub(crate) fn source_suppressed_package_dependency_closure(
    program: &EnrichedProgram,
    seed_modules: &BTreeSet<ModuleId>,
    source_preserved_packages: &BTreeSet<ModuleId>,
    ownership_proven_packages: &BTreeSet<ModuleId>,
) -> BTreeSet<ModuleId> {
    let modules_by_id = program
        .model()
        .modules()
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let (outgoing_dependencies, incoming_dependencies) = direct_module_dependency_indexes(program);
    let mut reachable = seed_modules
        .iter()
        .copied()
        .filter(|module_id| {
            modules_by_id
                .get(module_id)
                .is_some_and(|module| module.kind == ModuleKind::Package)
                && !source_preserved_packages.contains(module_id)
        })
        .collect::<BTreeSet<_>>();
    let mut stack = reachable.iter().copied().collect::<Vec<_>>();
    while let Some(module_id) = stack.pop() {
        for dependency_id in outgoing_dependencies
            .get(&module_id)
            .into_iter()
            .flatten()
            .copied()
        {
            let Some(dependency) = modules_by_id.get(&dependency_id) else {
                continue;
            };
            if dependency.kind != ModuleKind::Package
                || source_preserved_packages.contains(&dependency_id)
                || !ownership_proven_packages.contains(&dependency_id)
                || !reachable.insert(dependency_id)
            {
                continue;
            }
            stack.push(dependency_id);
        }
    }

    loop {
        let removed = reachable
            .iter()
            .copied()
            .filter(|module_id| !seed_modules.contains(module_id))
            .filter(|module_id| {
                let Some(module) = modules_by_id.get(module_id).copied() else {
                    return false;
                };
                incoming_dependencies
                    .get(module_id)
                    .into_iter()
                    .flatten()
                    .any(|consumer_id| {
                        modules_by_id.get(consumer_id).is_some_and(|consumer| {
                            !reachable.contains(consumer_id)
                                && !source_suppressed_consumer_is_boundary(module, consumer)
                        })
                    })
            })
            .collect::<Vec<_>>();
        if removed.is_empty() {
            break;
        }
        for module_id in removed {
            reachable.remove(&module_id);
        }
    }

    reachable
}

pub(crate) fn source_suppressed_consumer_is_boundary(
    module: &ModuleInput,
    consumer: &ModuleInput,
) -> bool {
    match consumer.kind {
        ModuleKind::Application => true,
        ModuleKind::Package => !source_suppressed_same_package_consumer(module, consumer),
        ModuleKind::Builtin => false,
    }
}

pub(crate) fn source_suppressed_same_package_consumer(
    module: &ModuleInput,
    consumer: &ModuleInput,
) -> bool {
    let Some(module_package) = module.package_name.as_deref().map(str::trim) else {
        return false;
    };
    let Some(consumer_package) = consumer.package_name.as_deref().map(str::trim) else {
        return false;
    };
    !module_package.is_empty() && module_package == consumer_package
}

pub(crate) fn package_ownership_proven_module_ids(program: &EnrichedProgram) -> BTreeSet<ModuleId> {
    let modules_by_id = program
        .model()
        .modules()
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    program
        .model()
        .input()
        .package_attributions
        .iter()
        .filter_map(|attribution| {
            let module = modules_by_id.get(&attribution.module_id).copied()?;
            package_attribution_proves_package_ownership(attribution, module)
                .then_some(attribution.module_id)
        })
        .collect()
}

pub(crate) fn package_attribution_proves_package_ownership(
    attribution: &PackageAttributionInput,
    module: &ModuleInput,
) -> bool {
    if module.kind != ModuleKind::Package
        || module.package_name.as_deref() != Some(attribution.package_name.as_str())
    {
        return false;
    }
    if let Some(attribution_version) = attribution.package_version.as_deref()
        && module
            .package_version
            .as_deref()
            .is_some_and(|module_version| {
                !module_version.trim().is_empty() && module_version != attribution_version
            })
    {
        return false;
    }
    (attribution.status == PackageAttributionStatus::Accepted
        && attribution.emission_mode == PackageEmissionMode::ExternalImport
        && attribution.export_specifier.is_some())
        || (attribution.status == PackageAttributionStatus::Rejected
            && attribution.emission_mode == PackageEmissionMode::ApplicationSource
            && attribution.package_version.is_some())
}

pub(crate) fn direct_module_dependency_indexes(
    program: &EnrichedProgram,
) -> (
    BTreeMap<ModuleId, BTreeSet<ModuleId>>,
    BTreeMap<ModuleId, BTreeSet<ModuleId>>,
) {
    let mut outgoing = BTreeMap::<ModuleId, BTreeSet<ModuleId>>::new();
    let mut incoming = BTreeMap::<ModuleId, BTreeSet<ModuleId>>::new();
    for dependency in &program.model().input().dependencies {
        let ModuleDependencyTarget::Module(target_module_id) = dependency.target else {
            continue;
        };
        outgoing
            .entry(dependency.from_module_id)
            .or_default()
            .insert(target_module_id);
        incoming
            .entry(target_module_id)
            .or_default()
            .insert(dependency.from_module_id);
    }
    (outgoing, incoming)
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
    let local_bindings = local_bindings_in_source(snippet.source.as_str());
    !implicit_global_writes_in_source(snippet.source.as_str())
        .into_iter()
        .filter(|write| !local_bindings.contains(write.as_str()))
        .any(|write| ctx.prelude.defines(&write))
}

pub(crate) fn is_migratable_private_runtime_function_dependency(
    binding: &BindingName,
    source: &str,
) -> bool {
    let source = source.trim();
    if let Some(rest) = source.strip_prefix("async")
        && rest.starts_with(|c: char| c.is_ascii_whitespace())
    {
        return function_declaration_names_binding(rest.trim_start(), binding);
    }
    function_declaration_names_binding(source, binding)
}

pub(crate) fn is_migratable_reader_function_snippet(binding: &BindingName, source: &str) -> bool {
    let source = source.trim();
    if let Some(rest) = source.strip_prefix("async")
        && rest.starts_with(|c: char| c.is_ascii_whitespace())
    {
        let rest = rest.trim_start();
        return function_declaration_names_binding(rest, binding);
    }
    class_declaration_names_binding(source, binding)
        || function_declaration_names_binding(source, binding)
        || variable_declaration_names_function_like_binding(source, binding)
}

pub(crate) fn is_migratable_namespace_reader_snippet(binding: &BindingName, source: &str) -> bool {
    let source = source.trim();
    for keyword in ["var", "let", "const"] {
        let Some(rest) = source.strip_prefix(keyword) else {
            continue;
        };
        if !rest.starts_with(|c: char| c.is_ascii_whitespace()) {
            continue;
        }
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_suffix(';') else {
            continue;
        };
        let mut splitter = rest.splitn(2, '=');
        let lhs = splitter.next().unwrap_or("").trim();
        let rhs = splitter.next().unwrap_or("").trim();
        if lhs != binding.as_str() {
            continue;
        }
        let compact_rhs = rhs
            .chars()
            .filter(|character| !character.is_ascii_whitespace())
            .collect::<String>();
        if compact_rhs == "{}" {
            return true;
        }
    }
    false
}

pub(crate) fn function_declaration_names_binding(source: &str, binding: &BindingName) -> bool {
    if !keyword_at(source, 0, "function") {
        return false;
    }
    parse_identifier_after_function_keyword(source, 0)
        .is_some_and(|(name, _)| name == binding.as_str())
}

pub(crate) fn class_declaration_names_binding(source: &str, binding: &BindingName) -> bool {
    if !keyword_at(source, 0, "class") {
        return false;
    }
    parse_identifier_after_keyword(source, 0, "class")
        .is_some_and(|(name, _)| name == binding.as_str())
        && is_migratable_reader_class(source)
}

pub(crate) fn is_migratable_reader_class(source: &str) -> bool {
    let Some(open) = find_top_level_byte(source, b'{') else {
        return false;
    };
    let header = source[..open].trim();
    if !migratable_reader_class_header(header) {
        return false;
    }
    let Some(close) = find_matching_brace(source, open) else {
        return false;
    };
    if skip_ws(source.as_bytes(), close + 1) != source.len() {
        return false;
    }
    !class_body_has_top_level_computed_key(source) && !class_body_has_eager_static_element(source)
}

pub(crate) fn migratable_reader_class_header(header: &str) -> bool {
    if header.contains('[') {
        return false;
    }
    if let Some(extends) = header
        .split_once("extends")
        .map(|(_, extends)| extends.trim())
    {
        let (base, end) = match parse_identifier(extends, 0) {
            Some(parsed) => parsed,
            None => return false,
        };
        if end != extends.len() || !is_runtime_global_identifier(base) {
            return false;
        }
    }
    true
}

pub(crate) fn class_body_has_top_level_computed_key(source: &str) -> bool {
    let Some(open) = find_top_level_byte(source, b'{') else {
        return true;
    };
    let Some(close) = find_matching_brace(source, open) else {
        return true;
    };
    let bytes = source.as_bytes();
    let mut cursor = open + 1;
    while cursor < close {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            // `field = [];` is an instance-field initializer, not a
            // computed property key. It is evaluated when instances are
            // constructed, so moving the class with its reader cluster does
            // not introduce class-definition-time reads.
            b'=' => {
                cursor =
                    find_statement_end(&source[..close], cursor + 1).map_or(close, |end| end + 1);
            }
            b'[' => return true,
            b'(' => {
                let Some(end) = find_matching_paren(source, cursor) else {
                    return true;
                };
                cursor = end + 1;
            }
            b'{' => {
                let Some(end) = find_matching_brace(source, cursor) else {
                    return true;
                };
                cursor = end + 1;
            }
            _ => cursor += 1,
        }
    }
    false
}

pub(crate) fn class_body_has_eager_static_element(source: &str) -> bool {
    let Some(open) = find_top_level_byte(source, b'{') else {
        return true;
    };
    let Some(close) = find_matching_brace(source, open) else {
        return true;
    };
    let bytes = source.as_bytes();
    let mut cursor = open + 1;
    while cursor < close {
        cursor = skip_ws_and_comments(bytes, cursor, close);
        if cursor >= close {
            break;
        }
        if keyword_at(source, cursor, "static")
            && static_class_element_is_eager(source, cursor + "static".len(), close)
        {
            return true;
        }
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'(' => {
                let Some(end) = find_matching_paren(source, cursor) else {
                    return true;
                };
                cursor = end + 1;
            }
            b'{' => {
                let Some(end) = find_matching_brace(source, cursor) else {
                    return true;
                };
                cursor = end + 1;
            }
            _ => cursor += 1,
        }
    }
    false
}

pub(crate) fn static_class_element_is_eager(
    source: &str,
    after_static: usize,
    close: usize,
) -> bool {
    let bytes = source.as_bytes();
    let cursor = skip_ws_and_comments(bytes, after_static, close);
    if cursor >= close {
        return false;
    }
    match bytes[cursor] {
        // `static() {}` / `static = ...` / `static;` are instance
        // members named "static", not static elements, so they don't run
        // during class definition.
        b'(' | b'=' | b';' => return false,
        // Static blocks run immediately while the class is being defined.
        b'{' => return true,
        _ => {}
    }

    let mut cursor = cursor;
    while cursor < close {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            // Method/accessor parameter list: safe, because the body is not
            // evaluated until the method is called after the writer module's
            // same-module assignment has run.
            b'(' => return false,
            b'=' => return static_field_initializer_is_eager(source, cursor, close),
            // Empty static fields don't read migrated runtime state.
            b';' => return false,
            // Static blocks run immediately while the class is being defined.
            b'{' => return true,
            _ => cursor += 1,
        }
    }
    false
}

pub(crate) fn static_field_initializer_is_eager(source: &str, equals: usize, close: usize) -> bool {
    let bytes = source.as_bytes();
    let initializer_start = skip_ws_and_comments(bytes, equals + 1, close);
    if initializer_start >= close {
        return false;
    }
    let initializer_end = find_statement_end(&source[..close], initializer_start).unwrap_or(close);
    let initializer = source[initializer_start..initializer_end].trim();
    !initializer.is_empty() && !is_pure_initializer_expression(initializer)
}

pub(crate) fn variable_declaration_names_function_like_binding(
    source: &str,
    binding: &BindingName,
) -> bool {
    let source = source.trim();
    for keyword in ["var", "let", "const"] {
        let Some(rest) = source.strip_prefix(keyword) else {
            continue;
        };
        if !rest.starts_with(|c: char| c.is_ascii_whitespace()) {
            continue;
        }
        let Some(rest) = rest.trim_start().strip_suffix(';') else {
            continue;
        };
        let mut splitter = rest.splitn(2, '=');
        let lhs = splitter.next().unwrap_or("").trim();
        let rhs = splitter.next().unwrap_or("").trim();
        if lhs != binding.as_str() {
            continue;
        }
        if expression_is_function_like_reader(rhs) {
            return true;
        }
    }
    false
}

pub(crate) fn expression_is_function_like_reader(source: &str) -> bool {
    let source = source.trim();
    if keyword_at(source, 0, "function") || looks_like_arrow_function_expression(source) {
        return true;
    }
    if let Some(rest) = source.strip_prefix("async")
        && rest.starts_with(|c: char| c.is_ascii_whitespace())
    {
        let rest = rest.trim_start();
        return keyword_at(rest, 0, "function") || looks_like_arrow_function_expression(rest);
    }
    false
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeLazyFoldModule {
    pub(crate) source_file_id: u32,
    pub(crate) required_bindings: BTreeSet<BindingName>,
    pub(crate) stub_exports: BTreeSet<BindingName>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeFoldedSourceChunk {
    byte_start: u32,
    source: String,
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
                let Ok(migration) = migratable_runtime_reader_cluster_result(
                    &reader_cluster_context,
                    module_id,
                    binding,
                    readers,
                ) else {
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

pub(crate) fn coalesce_runtime_lazy_initializer_call_runs(source: &str) -> String {
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
            edits.extend(coalesced_lazy_body_call_run_edits(
                source,
                initializer.body,
                initializer.body_span,
            ));
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
        source.to_string()
    } else {
        apply_text_edits(source, &edits)
    }
}

pub(crate) fn coalesced_lazy_body_call_run_edits(
    full_source: &str,
    body: &str,
    body_span: (usize, usize),
) -> Vec<(usize, usize, String)> {
    let statements = top_level_statement_spans(body);
    let mut edits = Vec::<(usize, usize, String)>::new();
    let mut run = Vec::<(usize, usize, String)>::new();
    for (start, end) in statements {
        let statement = &body[start..end];
        if let Some(expression) = coalescible_lazy_body_expression(statement) {
            run.push((start, end, expression));
            continue;
        }
        flush_lazy_body_call_run_edit(full_source, body_span, &mut run, &mut edits);
    }
    flush_lazy_body_call_run_edit(full_source, body_span, &mut run, &mut edits);
    edits
}

pub(crate) fn flush_lazy_body_call_run_edit(
    full_source: &str,
    body_span: (usize, usize),
    run: &mut Vec<(usize, usize, String)>,
    edits: &mut Vec<(usize, usize, String)>,
) {
    if run.len() < 2 {
        run.clear();
        return;
    }
    let (first_start, _, _) = run.first().expect("run length checked");
    let (_, last_end, _) = run.last().expect("run length checked");
    let absolute_start = body_span.0 + *first_start;
    let absolute_end = body_span.0 + *last_end;
    let first_statement = &full_source[absolute_start..body_span.0 + run[0].1];
    let leading = statement_leading_whitespace(first_statement);
    let expression = run
        .iter()
        .map(|(_, _, expression)| expression.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    edits.push((
        absolute_start,
        absolute_end,
        format!("{leading}{expression};"),
    ));
    run.clear();
}

pub(crate) fn statement_leading_whitespace(statement: &str) -> &str {
    let trimmed_start = statement.trim_start();
    &statement[..statement.len() - trimmed_start.len()]
}

pub(crate) fn coalescible_lazy_body_expression(statement: &str) -> Option<String> {
    let trimmed = statement.trim();
    let expression = trimmed.strip_suffix(';').unwrap_or(trimmed).trim();
    if expression == "void 0" {
        return Some(expression.to_string());
    }
    let (callee, after_callee) = parse_identifier(expression, 0)?;
    if is_js_keyword(callee) {
        return None;
    }
    let bytes = expression.as_bytes();
    let open = skip_ws(bytes, after_callee);
    if bytes.get(open) != Some(&b'(') {
        return None;
    }
    let close = find_matching_paren(expression, open)?;
    if !expression[open + 1..close].trim().is_empty() {
        return None;
    }
    (skip_ws(bytes, close + 1) == expression.len()).then(|| expression.to_string())
}

pub(crate) fn compact_pure_static_runtime_literals(source: &str) -> String {
    let mut edits = Vec::<(usize, usize, String)>::new();
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        if matches!(bytes[cursor], b'{' | b'[')
            && container_literal_can_start_runtime_compaction(source, cursor)
        {
            let literal_end = match bytes[cursor] {
                b'{' => find_matching_brace(source, cursor),
                b'[' => find_matching_bracket(source, cursor),
                _ => None,
            };
            let Some(literal_end) = literal_end else {
                cursor += 1;
                continue;
            };
            let literal = &source[cursor..=literal_end];
            if let Some(replacement) = compact_static_container_literal(literal) {
                edits.push((cursor, literal_end + 1, replacement));
                cursor = literal_end + 1;
                continue;
            }
        }
        cursor += 1;
    }
    if edits.is_empty() {
        source.to_string()
    } else {
        apply_text_edits(source, &edits)
    }
}

pub(crate) fn container_literal_can_start_runtime_compaction(
    source: &str,
    literal_start: usize,
) -> bool {
    let bytes = source.as_bytes();
    let Some(previous) = previous_non_ws(bytes, literal_start) else {
        return false;
    };
    match bytes[previous] {
        b'=' => {
            !matches!(
                previous_non_ws(bytes, previous).map(|index| bytes[index]),
                Some(b'=' | b'!' | b'<' | b'>')
            ) && !matches!(bytes.get(previous + 1), Some(b'=') | Some(b'>'))
        }
        b':' | b'(' | b'[' | b',' | b'?' => true,
        _ => false,
    }
}

pub(crate) fn compact_static_container_literal(literal: &str) -> Option<String> {
    if literal.lines().count() < 6
        || !static_container_literal_is_compaction_safe(literal)
        || !static_literal_is_text_compaction_safe(literal)
    {
        return None;
    }
    let replacement = compact_static_literal_text(literal);
    if replacement.lines().count() < literal.lines().count() && replacement.len() < literal.len() {
        Some(replacement)
    } else {
        None
    }
}

pub(crate) fn static_literal_is_text_compaction_safe(literal: &str) -> bool {
    let blocked_keywords = [
        "await", "break", "case", "catch", "class", "const", "continue", "do", "else", "for",
        "function", "if", "let", "return", "switch", "throw", "try", "var", "while", "yield",
    ];
    let bytes = literal.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(literal, cursor) {
            cursor = next;
            continue;
        }
        if bytes[cursor] == b'=' && bytes.get(cursor + 1) == Some(&b'>') {
            return false;
        }
        if is_identifier_start(bytes[cursor]) {
            let start = cursor;
            cursor += 1;
            while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
                cursor += 1;
            }
            let word = &literal[start..cursor];
            if blocked_keywords.contains(&word) {
                return false;
            }
            continue;
        }
        cursor += 1;
    }
    true
}

pub(crate) fn compact_static_literal_text(literal: &str) -> String {
    let bytes = literal.as_bytes();
    let mut output = String::with_capacity(literal.len());
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' => {
                let next = skip_quoted(bytes, cursor, bytes[cursor]);
                output.push_str(&literal[cursor..next]);
                cursor = next;
            }
            b'`' => {
                let next = skip_template_literal(bytes, cursor);
                output.push_str(&literal[cursor..next]);
                cursor = next;
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                let next = skip_line_comment(bytes, cursor + 2);
                if compact_removed_separator_needs_space(&output, literal, next) {
                    output.push(' ');
                }
                cursor = next;
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                let next = skip_block_comment(bytes, cursor + 2);
                if compact_removed_separator_needs_space(&output, literal, next) {
                    output.push(' ');
                }
                cursor = next;
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                let next = skip_regex_literal(bytes, cursor);
                output.push_str(&literal[cursor..next]);
                cursor = next;
            }
            byte if byte.is_ascii_whitespace() => {
                let next = skip_ws(bytes, cursor);
                if compact_removed_separator_needs_space(&output, literal, next) {
                    output.push(' ');
                }
                cursor = next;
            }
            _ => {
                output.push(bytes[cursor] as char);
                cursor += 1;
            }
        }
    }
    output
}

pub(crate) fn compact_removed_separator_needs_space(
    output: &str,
    source: &str,
    next: usize,
) -> bool {
    let Some(previous) = output.as_bytes().last().copied() else {
        return false;
    };
    let Some(next) = source.as_bytes().get(next).copied() else {
        return false;
    };
    (is_identifier_continue(previous) && is_identifier_continue(next))
        || matches!(
            (previous, next),
            (b'+', b'+') | (b'-', b'-') | (b'/', b'/') | (b'/', b'*')
        )
}

pub(crate) fn static_container_literal_is_compaction_safe(literal: &str) -> bool {
    match literal.as_bytes().first().copied() {
        Some(b'{') => compactable_object_container_literal(literal),
        Some(b'[') => compactable_array_container_literal(literal),
        _ => false,
    }
}

pub(crate) fn compactable_object_container_literal(source: &str) -> bool {
    let Some(close) = find_matching_brace(source, 0) else {
        return false;
    };
    if skip_ws(source.as_bytes(), close + 1) != source.len() {
        return false;
    }
    split_top_level_properties(&source[1..close])
        .into_iter()
        .all(compactable_object_container_property)
}

pub(crate) fn compactable_object_container_property(property: &str) -> bool {
    let property = property.trim();
    if property.is_empty() {
        return false;
    }
    if let Some(spread) = property.strip_prefix("...") {
        return simple_runtime_reference_expression(spread.trim());
    }
    if let Some(colon) = find_top_level_byte(property, b':') {
        if !pure_object_property_key(property[..colon].trim()) {
            return false;
        }
        return compactable_container_value_expression(property[colon + 1..].trim());
    }
    simple_runtime_reference_expression(property)
}

pub(crate) fn compactable_array_container_literal(source: &str) -> bool {
    let Some(close) = find_matching_bracket(source, 0) else {
        return false;
    };
    if skip_ws(source.as_bytes(), close + 1) != source.len() {
        return false;
    }
    split_top_level_properties(&source[1..close])
        .into_iter()
        .all(|element| {
            let element = element.trim();
            if element.is_empty() {
                return false;
            }
            if let Some(spread) = element.strip_prefix("...") {
                return simple_runtime_reference_expression(spread.trim());
            }
            compactable_container_value_expression(element)
        })
}

pub(crate) fn compactable_container_value_expression(source: &str) -> bool {
    let source = source.trim();
    if source.is_empty() {
        return false;
    }
    if matches!(source, "void 0")
        || is_literal_expression(source)
        || regex_literal_covers_source(source)
    {
        return true;
    }
    if source.as_bytes().first() == Some(&b'{') {
        return compactable_object_container_literal(source);
    }
    if source.as_bytes().first() == Some(&b'[') {
        return compactable_array_container_literal(source);
    }
    simple_runtime_reference_expression(source)
}

pub(crate) fn regex_literal_covers_source(source: &str) -> bool {
    let bytes = source.as_bytes();
    bytes.first() == Some(&b'/')
        && looks_like_regex_literal(bytes, 0)
        && skip_regex_literal(bytes, 0) == bytes.len()
}

pub(crate) fn simple_runtime_reference_expression(source: &str) -> bool {
    let source = source.trim();
    let bytes = source.as_bytes();
    let Some((first, mut cursor)) = parse_identifier(source, 0) else {
        return false;
    };
    if is_js_keyword(first) {
        return false;
    }
    while cursor < bytes.len() {
        if bytes.get(cursor) != Some(&b'.') || bytes.get(cursor + 1) == Some(&b'.') {
            return false;
        }
        cursor += 1;
        let Some((member, next)) = parse_identifier(source, cursor) else {
            return false;
        };
        if is_js_keyword(member) {
            return false;
        }
        cursor = next;
    }
    true
}

pub(crate) fn apply_text_edits(source: &str, edits: &[(usize, usize, String)]) -> String {
    let mut edits = edits.to_vec();
    edits.sort_by_key(|(start, _, _)| *start);
    let mut output = String::with_capacity(source.len());
    let mut cursor = 0usize;
    for (start, end, replacement) in edits {
        debug_assert!(start >= cursor, "text edits must not overlap");
        output.push_str(&source[cursor..start]);
        output.push_str(replacement.as_str());
        cursor = end;
    }
    output.push_str(&source[cursor..]);
    output
}

pub(crate) fn private_runtime_lazy_initializer_replacement(
    initializer: &ParsedRuntimeLazyInitializer<'_>,
    writable_helpers: &BTreeSet<BindingName>,
    pure_value_bindings: &BTreeSet<BindingName>,
) -> Option<String> {
    let assignments = pure_runtime_lazy_body_assignments(
        initializer.body,
        writable_helpers,
        pure_value_bindings,
    )?;
    if assignments.is_empty() {
        return None;
    }
    let mut lines = assignments
        .into_iter()
        .map(|(target, value)| format!("{target} = {value};"))
        .collect::<Vec<_>>();
    lines.push(format!(
        "var {} = () => {{}};",
        initializer.binding.as_str()
    ));
    Some(lines.join("\n"))
}

pub(crate) fn pure_runtime_value_bindings(source: &str) -> BTreeSet<BindingName> {
    let mut bindings = BTreeSet::new();
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 {
            if keyword_at(source, cursor, "function")
                && let Some((binding, next)) =
                    parse_identifier_after_function_keyword(source, cursor)
            {
                bindings.insert(BindingName::new(binding));
                cursor = next;
                continue;
            }
            if keyword_at(source, cursor, "class")
                && let Some((binding, next)) =
                    parse_identifier_after_keyword(source, cursor, "class")
            {
                bindings.insert(BindingName::new(binding));
                cursor = next;
                continue;
            }
            if let Some((binding, value, after)) = parse_pure_top_level_var_value(source, cursor) {
                if is_pure_initializer_expression(value) {
                    bindings.insert(binding);
                }
                cursor = after;
                continue;
            }
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
    bindings
}

pub(crate) fn parse_pure_top_level_var_value(
    source: &str,
    start: usize,
) -> Option<(BindingName, &str, usize)> {
    let (keyword, after_keyword) = declaration_keyword_at(source, start)?;
    let mut cursor = skip_ws(source.as_bytes(), start + after_keyword);
    let (binding, after_binding) = parse_identifier(source, cursor)?;
    cursor = skip_ws(source.as_bytes(), after_binding);
    if source.as_bytes().get(cursor) != Some(&b'=') {
        return None;
    }
    let value_start = skip_ws(source.as_bytes(), cursor + 1);
    let stmt_end = find_statement_end(source, value_start)?;
    let _ = keyword;
    Some((
        BindingName::new(binding),
        source[value_start..stmt_end].trim(),
        stmt_end + usize::from(source.as_bytes().get(stmt_end) == Some(&b';')),
    ))
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedRuntimeLazyInitializer<'a> {
    binding: BindingName,
    body: &'a str,
    body_span: (usize, usize),
    span: (usize, usize),
}

pub(crate) fn try_parse_runtime_lazy_initializer_declaration(
    source: &str,
    start: usize,
) -> Option<(ParsedRuntimeLazyInitializer<'_>, usize)> {
    let (keyword, after_keyword) = declaration_keyword_at(source, start)?;
    let bytes = source.as_bytes();
    let mut cursor = skip_ws(bytes, start + after_keyword);
    let (binding, after_binding) = parse_identifier(source, cursor)?;
    cursor = skip_ws(bytes, after_binding);
    if bytes.get(cursor) != Some(&b'=') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    let prefix = "lazyValue(";
    if !source[cursor..].starts_with(prefix) {
        return None;
    }
    cursor += prefix.len();
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'(') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    if bytes.get(cursor) != Some(&b')') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    cursor = expect_arrow(bytes, cursor)?;
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'{') {
        return None;
    }
    let body_start = cursor + 1;
    let body_end = find_matching_brace(source, cursor)?;
    let body = &source[body_start..body_end];
    let after_body = skip_ws(bytes, body_end + 1);
    if bytes.get(after_body) != Some(&b')') {
        return None;
    }
    let after_paren = skip_ws(bytes, after_body + 1);
    if bytes.get(after_paren) != Some(&b';') {
        return None;
    }
    let _ = keyword;
    let stmt_end = after_paren + 1;
    Some((
        ParsedRuntimeLazyInitializer {
            binding: BindingName::new(binding),
            body,
            body_span: (body_start, body_end),
            span: (start, stmt_end),
        },
        stmt_end,
    ))
}

pub(crate) fn find_statement_end(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut cursor = start;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        match bytes[cursor] {
            b'(' => paren_depth += 1,
            b')' => paren_depth = paren_depth.saturating_sub(1),
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b'{' => brace_depth += 1,
            b'}' => brace_depth = brace_depth.saturating_sub(1),
            b';' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => {
                return Some(cursor);
            }
            _ => {}
        }
        cursor += 1;
    }
    None
}

pub(crate) fn pure_runtime_lazy_body_assignments(
    body: &str,
    writable_helpers: &BTreeSet<BindingName>,
    pure_value_bindings: &BTreeSet<BindingName>,
) -> Option<Vec<(BindingName, String)>> {
    let mut assignments = Vec::new();
    let mut written = BTreeSet::new();
    for statement in top_level_statement_slices(body) {
        let statement = statement.trim().trim_end_matches(';').trim();
        if statement.is_empty() {
            continue;
        }
        let (target, after_target) = parse_identifier(statement, 0)?;
        let target = BindingName::new(target);
        if !writable_helpers.contains(&target) || !written.insert(target.clone()) {
            return None;
        }
        let equals = skip_ws(statement.as_bytes(), after_target);
        if statement.as_bytes().get(equals) != Some(&b'=')
            || statement.as_bytes().get(equals + 1) == Some(&b'=')
            || statement.as_bytes().get(equals + 1) == Some(&b'>')
        {
            return None;
        }
        let value_start = skip_ws(statement.as_bytes(), equals + 1);
        let value = statement[value_start..].trim();
        if !is_pure_runtime_assignment_value(value, pure_value_bindings) {
            return None;
        }
        assignments.push((target, value.to_string()));
    }
    Some(assignments)
}

pub(crate) fn is_pure_runtime_assignment_value(
    value: &str,
    pure_value_bindings: &BTreeSet<BindingName>,
) -> bool {
    if is_pure_initializer_expression(value) {
        return true;
    }
    let value = value.trim();
    let Some((identifier, end)) = parse_identifier(value, 0) else {
        return false;
    };
    if end != value.len() {
        return false;
    }
    is_runtime_global_identifier(identifier)
        || pure_value_bindings.contains(&BindingName::new(identifier))
}

pub(crate) fn pure_lazy_initializer_replacement(
    statement: &str,
    writable_helpers: &BTreeSet<BindingName>,
) -> Option<String> {
    let initializer = parse_lowered_lazy_initializer_statement(statement)?;
    let assignments = pure_lazy_body_assignments(initializer.body, writable_helpers)?;
    if assignments.is_empty() {
        return None;
    }
    let mut lines = assignments
        .into_iter()
        .map(|(target, value)| format!("{target} = {value};"))
        .collect::<Vec<_>>();
    lines.push(format!(
        "var {} = () => {{}};",
        initializer.binding.as_str()
    ));
    Some(lines.join("\n"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedLoweredLazyInitializer<'a> {
    binding: BindingName,
    body: &'a str,
}

pub(crate) fn parse_lowered_lazy_initializer_statement(
    statement: &str,
) -> Option<ParsedLoweredLazyInitializer<'_>> {
    let statement = statement.trim().trim_end_matches(';').trim();
    let (_keyword, after_keyword) = declaration_keyword_at_start(statement)?;
    let cursor = skip_ws(statement.as_bytes(), after_keyword);
    let (binding, after_binding) = parse_identifier(statement, cursor)?;
    let equals = skip_ws(statement.as_bytes(), after_binding);
    if statement.as_bytes().get(equals) != Some(&b'=') {
        return None;
    }
    let initializer_start = skip_ws(statement.as_bytes(), equals + 1);
    let body = parse_lowered_lazy_initializer_body(&statement[initializer_start..])?;
    Some(ParsedLoweredLazyInitializer {
        binding: BindingName::new(binding),
        body,
    })
}

pub(crate) fn parse_lowered_lazy_initializer_body(initializer: &str) -> Option<&str> {
    if !looks_like_lowered_lazy_initializer(initializer) {
        return None;
    }
    // Expected shape: `lazyValue(() => { BODY })`.
    let prefix = "lazyValue(";
    if !initializer.starts_with(prefix) {
        return None;
    }
    let mut cursor = prefix.len();
    cursor = skip_ws(initializer.as_bytes(), cursor);
    if initializer.as_bytes().get(cursor) != Some(&b'(') {
        return None;
    }
    cursor = skip_ws(initializer.as_bytes(), cursor + 1);
    if initializer.as_bytes().get(cursor) != Some(&b')') {
        return None;
    }
    cursor = skip_ws(initializer.as_bytes(), cursor + 1);
    cursor = expect_arrow(initializer.as_bytes(), cursor)?;
    cursor = skip_ws(initializer.as_bytes(), cursor);
    if initializer.as_bytes().get(cursor) != Some(&b'{') {
        return None;
    }
    let body_end = find_matching_brace(initializer, cursor)?;
    let after_body = skip_ws(initializer.as_bytes(), body_end + 1);
    if initializer.as_bytes().get(after_body) != Some(&b')') {
        return None;
    }
    Some(initializer[cursor + 1..body_end].trim())
}

pub(crate) fn pure_lazy_body_assignments(
    body: &str,
    writable_helpers: &BTreeSet<BindingName>,
) -> Option<Vec<(BindingName, String)>> {
    let mut assignments = Vec::new();
    let mut written = BTreeSet::new();
    for statement in top_level_statement_slices(body) {
        let statement = statement.trim().trim_end_matches(';').trim();
        if statement.is_empty() {
            continue;
        }
        let (target, after_target) = parse_identifier(statement, 0)?;
        let target = BindingName::new(target);
        if !writable_helpers.contains(&target) || !written.insert(target.clone()) {
            return None;
        }
        let equals = skip_ws(statement.as_bytes(), after_target);
        if statement.as_bytes().get(equals) != Some(&b'=')
            || statement.as_bytes().get(equals + 1) == Some(&b'=')
            || statement.as_bytes().get(equals + 1) == Some(&b'>')
        {
            return None;
        }
        let value_start = skip_ws(statement.as_bytes(), equals + 1);
        let value = statement[value_start..].trim();
        if !is_pure_initializer_expression(value) {
            return None;
        }
        assignments.push((target, value.to_string()));
    }
    Some(assignments)
}

pub(crate) fn is_pure_initializer_expression(source: &str) -> bool {
    let source = source.trim();
    if source.is_empty() {
        return false;
    }
    if matches!(source, "void 0") {
        return true;
    }
    if is_literal_expression(source) {
        return true;
    }
    if source.as_bytes().first() == Some(&b'{') {
        return pure_object_literal(source);
    }
    if source.as_bytes().first() == Some(&b'[') {
        return pure_array_literal(source);
    }
    if keyword_at(source, 0, "class") {
        return pure_class_expression(source);
    }
    if keyword_at(source, 0, "function") || looks_like_arrow_function_expression(source) {
        return true;
    }
    false
}

pub(crate) fn is_literal_expression(source: &str) -> bool {
    if matches!(
        source,
        "true" | "false" | "null" | "undefined" | "NaN" | "Infinity"
    ) {
        return true;
    }
    if quoted_literal_covers_source(source) {
        return true;
    }
    if matches!(source, "!0" | "!1") {
        return true;
    }
    if source.starts_with('`') && source.ends_with('`') && !source.contains("${") {
        return true;
    }
    let number = source.strip_prefix(['+', '-']).unwrap_or(source);
    !number.is_empty()
        && number
            .chars()
            .all(|character| character.is_ascii_digit() || matches!(character, '.' | '_' | 'n'))
        && number.chars().any(|character| character.is_ascii_digit())
}

pub(crate) fn quoted_literal_covers_source(source: &str) -> bool {
    let bytes = source.as_bytes();
    let Some(quote @ (b'\'' | b'"')) = bytes.first().copied() else {
        return false;
    };
    skip_quoted(bytes, 0, quote) == bytes.len()
}

pub(crate) fn pure_object_literal(source: &str) -> bool {
    let Some(close) = find_matching_brace(source, 0) else {
        return false;
    };
    if skip_ws(source.as_bytes(), close + 1) != source.len() {
        return false;
    }
    split_top_level_properties(&source[1..close])
        .into_iter()
        .all(pure_object_property)
}

pub(crate) fn pure_object_property(property: &str) -> bool {
    let property = property.trim();
    if property.is_empty() || property.starts_with("...") {
        return false;
    }
    if let Some(colon) = find_top_level_byte(property, b':') {
        if !pure_object_property_key(property[..colon].trim()) {
            return false;
        }
        let value = property[colon + 1..].trim();
        return is_pure_initializer_expression(value);
    }
    pure_object_method_property(property)
}

pub(crate) fn pure_object_property_key(source: &str) -> bool {
    let source = source.trim();
    if source.is_empty() {
        return false;
    }
    if source.as_bytes().first() == Some(&b'[') {
        let Some(close) = find_matching_bracket(source, 0) else {
            return false;
        };
        return skip_ws(source.as_bytes(), close + 1) == source.len()
            && is_literal_expression(&source[1..close]);
    }
    if quoted_literal_covers_source(source) {
        return true;
    }
    let key = source.strip_prefix(['+', '-']).unwrap_or(source);
    key.as_bytes()
        .first()
        .is_some_and(|byte| is_identifier_start(*byte) || byte.is_ascii_digit())
        && key
            .as_bytes()
            .iter()
            .all(|byte| is_identifier_continue(*byte) || *byte == b'.')
}

pub(crate) fn pure_object_method_property(source: &str) -> bool {
    let source = source.trim();
    let method_source = source
        .strip_prefix("async ")
        .or_else(|| source.strip_prefix("get "))
        .or_else(|| source.strip_prefix("set "))
        .unwrap_or(source)
        .trim_start();
    let Some(open_paren) = find_top_level_byte(method_source, b'(') else {
        return false;
    };
    if !pure_object_property_key(method_source[..open_paren].trim()) {
        return false;
    }
    let Some(close_paren) = find_matching_paren(method_source, open_paren) else {
        return false;
    };
    let open_body = skip_ws(method_source.as_bytes(), close_paren + 1);
    if method_source.as_bytes().get(open_body) != Some(&b'{') {
        return false;
    }
    let Some(close_body) = find_matching_brace(method_source, open_body) else {
        return false;
    };
    skip_ws(method_source.as_bytes(), close_body + 1) == method_source.len()
}

pub(crate) fn pure_array_literal(source: &str) -> bool {
    let Some(close) = find_matching_bracket(source, 0) else {
        return false;
    };
    if skip_ws(source.as_bytes(), close + 1) != source.len() {
        return false;
    }
    split_top_level_properties(&source[1..close])
        .into_iter()
        .all(|element| {
            let element = element.trim();
            !element.is_empty()
                && !element.starts_with("...")
                && is_pure_initializer_expression(element)
        })
}

pub(crate) fn pure_class_expression(source: &str) -> bool {
    let Some(open) = find_top_level_byte(source, b'{') else {
        return false;
    };
    let header = source[..open].trim();
    if header.contains('[') {
        return false;
    }
    if let Some(extends) = header
        .split_once("extends")
        .map(|(_, extends)| extends.trim())
    {
        let (base, end) = match parse_identifier(extends, 0) {
            Some(parsed) => parsed,
            None => return false,
        };
        if end != extends.len() || !is_runtime_global_identifier(base) {
            return false;
        }
    }
    let Some(close) = find_matching_brace(source, open) else {
        return false;
    };
    if skip_ws(source.as_bytes(), close + 1) != source.len() {
        return false;
    }
    !source[open + 1..close]
        .split(|character: char| !is_identifier_continue(character as u8))
        .any(|word| word == "static")
}

pub(crate) fn looks_like_arrow_function_expression(source: &str) -> bool {
    find_top_level_arrow(source).is_some()
}

pub(crate) fn find_top_level_arrow(source: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    while cursor + 1 < bytes.len() {
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
            b'=' if paren_depth == 0
                && bracket_depth == 0
                && brace_depth == 0
                && bytes.get(cursor + 1) == Some(&b'>') =>
            {
                return Some(cursor);
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
    None
}

pub(crate) fn find_top_level_byte(source: &str, target: u8) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
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
            byte if byte == target
                && paren_depth == 0
                && bracket_depth == 0
                && brace_depth == 0 =>
            {
                return Some(cursor);
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
    None
}

pub(crate) fn top_level_statement_slices(source: &str) -> Vec<&str> {
    top_level_statement_spans(source)
        .into_iter()
        .map(|(start, end)| &source[start..end])
        .collect()
}

pub(crate) fn top_level_statement_spans(source: &str) -> Vec<(usize, usize)> {
    collect_top_level_statement_facts(source, None, ParseGoal::TypeScript)
        .expect("planner top-level statement facts require parseable generated TypeScript source")
        .into_iter()
        .map(|fact| (fact.byte_start as usize, fact.byte_end as usize))
        .collect()
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

pub(crate) fn contains_top_level_initializer_operator(source: &str, mut cursor: usize) -> bool {
    let bytes = source.as_bytes();
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
            b'=' if paren_depth == 0
                && bracket_depth == 0
                && brace_depth == 0
                && bytes.get(cursor + 1) != Some(&b'=')
                && bytes.get(cursor + 1) != Some(&b'>') =>
            {
                return true;
            }
            _ => cursor += 1,
        }
    }
    false
}

pub(crate) fn runtime_entrypoint(
    program: &EnrichedProgram,
) -> Option<(&RuntimePrelude, &RuntimeEntrypoint)> {
    program
        .model()
        .graph()
        .runtime_preludes()
        .values()
        .find_map(|prelude| {
            prelude
                .entrypoint
                .as_ref()
                .map(|entrypoint| (prelude, entrypoint))
        })
}

pub(crate) fn runtime_entrypoint_root_bindings(
    prelude: &RuntimePrelude,
    entrypoint: &RuntimeEntrypoint,
) -> BTreeSet<BindingName> {
    let side_effects = runtime_entrypoint_side_effects(prelude, entrypoint);
    let mut source_bindings = BTreeSet::from([entrypoint.callee.clone()]);
    for side_effect in &side_effects {
        for identifier in identifiers_in_source(side_effect.source.as_str()) {
            let binding = BindingName::new(identifier);
            if prelude.defines(&binding) {
                source_bindings.insert(binding);
            }
        }
    }
    source_bindings
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClosedRuntimeHelperSource {
    pub(crate) emitted_bindings: BTreeSet<BindingName>,
    pub(crate) source: String,
}

pub(crate) fn close_runtime_helper_source(
    prelude: &RuntimePrelude,
    root_bindings: &BTreeSet<BindingName>,
    entrypoint: Option<&RuntimeEntrypoint>,
    folded_chunks: &[RuntimeFoldedSourceChunk],
) -> ClosedRuntimeHelperSource {
    close_runtime_helper_source_excluding(
        prelude,
        root_bindings,
        entrypoint,
        folded_chunks,
        &BTreeSet::new(),
    )
}

pub(crate) fn close_runtime_helper_source_excluding(
    prelude: &RuntimePrelude,
    root_bindings: &BTreeSet<BindingName>,
    entrypoint: Option<&RuntimeEntrypoint>,
    folded_chunks: &[RuntimeFoldedSourceChunk],
    excluded_bindings: &BTreeSet<BindingName>,
) -> ClosedRuntimeHelperSource {
    let mut source_bindings =
        runtime_required_bindings_excluding(prelude, root_bindings.iter(), excluded_bindings);

    loop {
        let namespace_exports = runtime_namespace_exports_for_helpers(prelude, &source_bindings);
        let mut roots = source_bindings.clone();
        for namespace_export in &namespace_exports {
            roots.extend(namespace_export.exports.values().cloned());
        }

        let source_bindings_with_namespaces =
            runtime_required_bindings_excluding(prelude, roots.iter(), excluded_bindings);
        let helper_source = runtime_helper_source(
            prelude,
            &source_bindings_with_namespaces,
            &namespace_exports,
            entrypoint,
            folded_chunks,
        );
        let mut next_roots = source_bindings_with_namespaces.clone();
        for identifier in runtime_import_identifiers_in_source(helper_source.as_str()) {
            let binding = BindingName::new(identifier);
            if prelude.defines(&binding) && !excluded_bindings.contains(&binding) {
                next_roots.insert(binding);
            }
        }
        let next_source_bindings =
            runtime_required_bindings_excluding(prelude, next_roots.iter(), excluded_bindings);
        if next_source_bindings == source_bindings_with_namespaces {
            let namespace_exports =
                runtime_namespace_exports_for_helpers(prelude, &next_source_bindings);
            let source = runtime_helper_source(
                prelude,
                &next_source_bindings,
                &namespace_exports,
                entrypoint,
                folded_chunks,
            );
            return ClosedRuntimeHelperSource {
                emitted_bindings: emitted_runtime_helper_bindings(
                    prelude,
                    &next_source_bindings,
                    folded_chunks,
                ),
                source,
            };
        }

        source_bindings = next_source_bindings;
    }
}

pub(crate) fn runtime_required_bindings_excluding<'a>(
    prelude: &RuntimePrelude,
    roots: impl Iterator<Item = &'a BindingName>,
    excluded_bindings: &BTreeSet<BindingName>,
) -> BTreeSet<BindingName> {
    prelude
        .required_bindings_for(roots)
        .difference(excluded_bindings)
        .cloned()
        .collect()
}

pub(crate) fn emitted_runtime_helper_bindings(
    prelude: &RuntimePrelude,
    bindings: &BTreeSet<BindingName>,
    folded_chunks: &[RuntimeFoldedSourceChunk],
) -> BTreeSet<BindingName> {
    let mut emitted = bindings
        .iter()
        .filter(|binding| prelude.snippets.contains_key(*binding))
        .cloned()
        .collect::<BTreeSet<_>>();
    for folded_chunk in folded_chunks {
        emitted.extend(top_level_definitions_in_source(
            folded_chunk.source.as_str(),
        ));
    }
    emitted
}

pub(crate) fn runtime_helper_source(
    prelude: &RuntimePrelude,
    source_bindings: &BTreeSet<BindingName>,
    namespace_exports: &[RuntimeNamespaceExport],
    entrypoint: Option<&RuntimeEntrypoint>,
    folded_chunks: &[RuntimeFoldedSourceChunk],
) -> String {
    let mut snippets = BTreeMap::<u32, String>::new();
    for binding in source_bindings {
        let Some(snippet) = prelude.snippets.get(binding) else {
            continue;
        };
        snippets
            .entry(snippet.byte_start)
            .or_insert_with(|| snippet.source.clone());
    }
    let mut chunks = snippets
        .into_iter()
        .map(|(byte_start, source)| (byte_start, 0u8, source))
        .collect::<Vec<_>>();
    for namespace_export in namespace_exports {
        chunks.push((
            namespace_export.byte_start,
            1,
            runtime_namespace_export_statement(namespace_export),
        ));
    }
    for folded_chunk in folded_chunks {
        chunks.push((folded_chunk.byte_start, 2, folded_chunk.source.clone()));
    }
    if let Some(entrypoint) = entrypoint {
        for side_effect in runtime_entrypoint_side_effects(prelude, entrypoint) {
            chunks.push((side_effect.byte_start, 3, side_effect.source));
        }
    }
    chunks.sort_by_key(|(byte_start, kind, _source)| (*byte_start, *kind));
    chunks
        .into_iter()
        .map(|(_, _, source)| source)
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn runtime_entrypoint_side_effects(
    prelude: &RuntimePrelude,
    entrypoint: &RuntimeEntrypoint,
) -> Vec<reverts_graph::RuntimePreludeSideEffect> {
    let mut side_effects = entrypoint
        .side_effects
        .iter()
        .filter(|side_effect| !is_noop_runtime_side_effect(prelude, side_effect.source.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    side_effects.sort_by_key(|side_effect| side_effect.byte_start);
    side_effects
}

pub(crate) fn is_noop_runtime_side_effect(prelude: &RuntimePrelude, source: &str) -> bool {
    let Some(binding) = simple_call_statement_binding(source) else {
        return false;
    };
    let Some(snippet) = prelude.snippets.get(&binding) else {
        return false;
    };
    runtime_prelude_snippet_is_noop(binding.as_str(), snippet.source.as_str())
}

pub(crate) fn simple_call_statement_binding(source: &str) -> Option<BindingName> {
    let source = source.trim().trim_end_matches(';').trim();
    let (identifier, after_identifier) = parse_identifier(source, 0)?;
    let open = skip_ws(source.as_bytes(), after_identifier);
    if source.as_bytes().get(open) != Some(&b'(') {
        return None;
    }
    let close = find_matching_paren(source, open)?;
    if !source[open + 1..close].trim().is_empty() {
        return None;
    }
    (skip_ws(source.as_bytes(), close + 1) == source.len()).then(|| BindingName::new(identifier))
}

pub(crate) fn runtime_prelude_snippet_is_noop(binding: &str, source: &str) -> bool {
    let compact = compact_js_source(source);
    let candidates = [
        format!("var{binding}=()=>{{}};"),
        format!("let{binding}=()=>{{}};"),
        format!("const{binding}=()=>{{}};"),
        format!("function{binding}(){{}}"),
    ];
    candidates.contains(&compact)
}

pub(crate) fn sanitize_identifier_fragment(value: &str) -> String {
    let mut out = String::new();
    for character in value.chars() {
        if character == '$' || character == '_' || character.is_ascii_alphanumeric() {
            out.push(character);
        } else {
            out.push('_');
        }
    }
    out
}

pub(crate) fn previous_token_is_keyword(source: &str, before: usize, keyword: &str) -> bool {
    let bytes = source.as_bytes();
    let Some(last) = previous_non_ws(bytes, before) else {
        return false;
    };
    if !is_identifier_continue(bytes[last]) {
        return false;
    }
    let mut start = last;
    while start > 0 && is_identifier_continue(bytes[start - 1]) {
        start -= 1;
    }
    &source[start..=last] == keyword
}

pub(crate) fn compact_js_source(source: &str) -> String {
    source
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct RuntimeExternalizedBindingScan {
    source_module_imports: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    package_init_shims: BTreeSet<BindingName>,
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

pub(crate) fn runtime_import_identifiers_in_source(source: &str) -> BTreeSet<String> {
    let scan_source = runtime_dependency_scan_source(source);
    let source = scan_source.as_deref().unwrap_or(source);
    let local_bindings = local_bindings_in_source(source);
    value_identifiers_in_source(source)
        .into_iter()
        .filter(|identifier| !is_runtime_global_identifier(identifier))
        .filter(|identifier| !local_bindings.contains(identifier))
        .collect()
}

pub(crate) fn runtime_dependency_scan_source(source: &str) -> Option<String> {
    let body_open = function_body_open(source)?;
    let body_close = find_matching_brace(source, body_open)?;
    let body = &source[body_open + 1..body_close];
    let (return_start, return_end) = top_level_return_statement_span(body)?;
    if body[return_end..].trim().is_empty() {
        return None;
    }
    let tail_function_names = top_level_function_declaration_names(&body[return_end..]);
    if tail_function_names
        .iter()
        .any(|name| source_contains_identifier_token(&body[..return_end], name))
    {
        return None;
    }

    let absolute_return_end = body_open + 1 + return_end;
    let mut stripped = String::new();
    stripped.push_str(&source[..absolute_return_end]);
    stripped.push_str(&source[body_close..]);
    debug_assert!(stripped.len() < source.len());
    debug_assert!(return_start < return_end);
    Some(stripped)
}

pub(crate) fn function_body_open(source: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut cursor = skip_ws(bytes, 0);
    if keyword_at(source, cursor, "async") {
        cursor = skip_ws(bytes, cursor + "async".len());
    }
    if !keyword_at(source, cursor, "function") {
        return None;
    }
    cursor += "function".len();
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
            b'(' => {
                let params_end = find_matching_paren(source, cursor)?;
                let body_open = skip_ws(bytes, params_end + 1);
                return (bytes.get(body_open) == Some(&b'{')).then_some(body_open);
            }
            b';' | b'=' | b'{' => return None,
            _ => cursor += 1,
        }
    }
    None
}

pub(crate) fn top_level_return_statement_span(body: &str) -> Option<(usize, usize)> {
    let bytes = body.as_bytes();
    let mut cursor = 0usize;
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
                && keyword_starts_statement_declaration(body, cursor)
                && keyword_at(body, cursor, "return") =>
            {
                return Some((cursor, top_level_statement_end(body, cursor)));
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
    None
}

pub(crate) fn top_level_statement_end(source: &str, start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut cursor = start;
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
            b';' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => {
                return cursor + 1;
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
    source.len()
}

pub(crate) fn top_level_function_declaration_names(source: &str) -> BTreeSet<String> {
    let bytes = source.as_bytes();
    let mut names = BTreeSet::new();
    let mut cursor = 0usize;
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
                && keyword_at(source, cursor, "function") =>
            {
                if let Some((name, next)) = parse_identifier_after_function_keyword(source, cursor)
                {
                    names.insert(name.to_string());
                    cursor = next;
                } else {
                    cursor += "function".len();
                }
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
    names
}

pub(crate) fn source_contains_identifier_token(source: &str, identifier: &str) -> bool {
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
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
            byte if is_identifier_start(byte) => {
                let start = cursor;
                cursor += 1;
                while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
                    cursor += 1;
                }
                if &source[start..cursor] == identifier {
                    return true;
                }
            }
            _ => cursor += 1,
        }
    }
    false
}

pub(crate) fn call_identifiers_in_source(source: &str) -> BTreeSet<String> {
    identifier_read_facts_in_source(source)
        .into_iter()
        .filter(|fact| fact.is_call_callee)
        .map(|fact| fact.name)
        .filter(|identifier| !is_runtime_global_identifier(identifier))
        .collect()
}

pub(crate) fn value_identifiers_in_source(source: &str) -> BTreeSet<String> {
    identifier_read_facts_in_source(source)
        .into_iter()
        .map(|fact| fact.name)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdentifierReadUsage {
    pub(crate) name: String,
    pub(crate) byte_start: usize,
    pub(crate) byte_end: usize,
    pub(crate) is_call_callee: bool,
}

pub(crate) fn identifier_read_facts_in_source(source: &str) -> Vec<IdentifierReadUsage> {
    try_identifier_read_facts_in_source(source).unwrap_or_default()
}

pub(crate) fn try_identifier_read_facts_in_source(
    source: &str,
) -> Option<Vec<IdentifierReadUsage>> {
    collect_identifier_read_facts(source, None, ParseGoal::TypeScript)
        .ok()
        .map(|facts| {
            facts
                .into_iter()
                .map(|fact| IdentifierReadUsage {
                    name: fact.name,
                    byte_start: fact.byte_start as usize,
                    byte_end: fact.byte_end as usize,
                    is_call_callee: fact.is_call_callee,
                })
                .collect()
        })
}

pub(crate) fn identifier_read_rename_sites_are_safe(source: &str, binding: &BindingName) -> bool {
    // When the generated source isn't parseable we can't enumerate reads,
    // so we must declare the rename unsafe rather than admitting it on an
    // empty fact set (which would silently miss usages).
    let Some(facts) = try_identifier_read_facts_in_source(source) else {
        return false;
    };
    facts
        .into_iter()
        .filter(|fact| fact.name == binding.as_str())
        .all(|fact| identifier_read_rename_site_is_safe(source, fact.byte_start, fact.byte_end))
}

pub(crate) fn identifier_read_rename_site_is_safe(source: &str, start: usize, end: usize) -> bool {
    let bytes = source.as_bytes();
    if previous_non_ws(bytes, start)
        .and_then(|index| bytes.get(index))
        .is_some_and(|byte| matches!(*byte, b'.' | b'#'))
    {
        return false;
    }
    let next = skip_ws(bytes, end);
    if bytes.get(next).is_some_and(|byte| *byte == b':') {
        return false;
    }
    let prev = previous_non_ws(bytes, start).and_then(|index| bytes.get(index).copied());
    let next_byte = bytes.get(next).copied();
    if matches!(prev, Some(b'{') | Some(b',')) && matches!(next_byte, Some(b'}') | Some(b',')) {
        return false;
    }
    true
}

pub(crate) fn rename_identifier_reads_in_source(
    source: &str,
    aliases: &BTreeMap<BindingName, BindingName>,
) -> String {
    if aliases.is_empty() {
        return source.to_string();
    }
    let mut edits = identifier_read_facts_in_source(source)
        .into_iter()
        .filter_map(|fact| {
            let alias = aliases.get(&BindingName::new(fact.name.as_str()))?;
            if !identifier_read_rename_site_is_safe(source, fact.byte_start, fact.byte_end) {
                return None;
            }
            Some((fact.byte_start, fact.byte_end, alias.as_str().to_string()))
        })
        .collect::<Vec<_>>();
    if edits.is_empty() {
        return source.to_string();
    }
    edits.sort_by(|left, right| right.0.cmp(&left.0));
    let mut rewritten = source.to_string();
    for (start, end, replacement) in edits {
        rewritten.replace_range(start..end, replacement.as_str());
    }
    rewritten
}

pub(crate) fn identifier_occurrence_is_value_reference(
    source: &str,
    start: usize,
    end: usize,
) -> bool {
    let bytes = source.as_bytes();
    if previous_non_ws(bytes, start)
        .and_then(|index| bytes.get(index))
        .is_some_and(|byte| *byte == b'#')
    {
        return false;
    }
    if previous_non_ws(bytes, start)
        .and_then(|index| bytes.get(index).map(|byte| (index, *byte)))
        .is_some_and(|(index, byte)| {
            byte == b'.' && index.checked_sub(1).and_then(|prev| bytes.get(prev)) != Some(&b'.')
        })
    {
        return false;
    }
    if identifier_is_declaration_name_after_keyword(source, start, "class")
        || identifier_is_declaration_name_after_keyword(source, start, "function")
    {
        return false;
    }

    let after = skip_ws(bytes, end);
    let before = previous_non_ws(bytes, start).and_then(|index| bytes.get(index));
    if bytes.get(after) == Some(&b'=') && bytes.get(after + 1) == Some(&b'>') {
        return false;
    }
    if bytes.get(after) == Some(&b':')
        && previous_non_ws(bytes, start)
            .and_then(|index| bytes.get(index))
            .is_some_and(|byte| matches!(*byte, b'{' | b',' | b'('))
    {
        return false;
    }

    if bytes.get(after) == Some(&b'=')
        && before.is_none_or(|byte| matches!(*byte, b'{' | b'}' | b';' | b',' | b'('))
    {
        return false;
    }

    if bytes.get(after) == Some(&b'(')
        && let Some(close) = find_matching_paren(source, after)
        && bytes.get(skip_ws(bytes, close + 1)) == Some(&b'{')
        && before.is_none_or(|byte| !matches!(*byte, b'.' | b')' | b']'))
    {
        return false;
    }

    true
}

pub(crate) fn identifier_is_declaration_name_after_keyword(
    source: &str,
    start: usize,
    keyword: &str,
) -> bool {
    let bytes = source.as_bytes();
    let Some(keyword_end) = previous_non_ws(bytes, start) else {
        return false;
    };
    let Some(keyword_start) = keyword_end
        .checked_add(1)
        .and_then(|end| end.checked_sub(keyword.len()))
    else {
        return false;
    };
    if bytes.get(keyword_start..keyword_end + 1) != Some(keyword.as_bytes()) {
        return false;
    }
    let before = keyword_start
        .checked_sub(1)
        .and_then(|index| bytes.get(index))
        .copied();
    let after = bytes.get(keyword_end + 1).copied();
    before.is_none_or(|byte| !is_identifier_continue(byte))
        && after.is_some_and(|byte| byte.is_ascii_whitespace())
}

pub(crate) fn control_flow_keyword_before_paren(source: &str, open_paren: usize) -> bool {
    match keyword_before_paren(source, open_paren) {
        Some("if" | "while" | "switch" | "for" | "catch" | "with") => true,
        Some("await") => for_keyword_before_await(source, open_paren),
        _ => false,
    }
}

pub(crate) fn keyword_before_paren(source: &str, open_paren: usize) -> Option<&str> {
    let bytes = source.as_bytes();
    let before = previous_non_ws(bytes, open_paren)?;
    // `previous_non_ws` walks raw bytes, so `before` can land inside a
    // multi-byte UTF-8 sequence. Identifier keywords are ASCII; bail
    // immediately when the preceding byte isn't ASCII so we never slice
    // across a char boundary.
    if !bytes[before].is_ascii() {
        return None;
    }
    let mut start = before;
    while start > 0 && is_identifier_continue(bytes[start - 1]) {
        start -= 1;
    }
    if start
        .checked_sub(1)
        .and_then(|index| bytes.get(index))
        .is_some_and(|byte| matches!(*byte, b'.' | b'#'))
    {
        return None;
    }
    Some(&source[start..=before])
}

pub(crate) fn for_keyword_before_await(source: &str, open_paren: usize) -> bool {
    let bytes = source.as_bytes();
    let await_end = match previous_non_ws(bytes, open_paren) {
        Some(index) => index,
        None => return false,
    };
    if !bytes[await_end].is_ascii() {
        return false;
    }
    let mut await_start = await_end;
    while await_start > 0 && is_identifier_continue(bytes[await_start - 1]) {
        await_start -= 1;
    }
    if &source[await_start..=await_end] != "await" {
        return false;
    }
    let Some(for_end) = previous_non_ws(bytes, await_start) else {
        return false;
    };
    if !bytes[for_end].is_ascii() {
        return false;
    }
    let mut for_start = for_end;
    while for_start > 0 && is_identifier_continue(bytes[for_start - 1]) {
        for_start -= 1;
    }
    &source[for_start..=for_end] == "for"
}

pub(crate) fn class_field_bindings_in_source(source: &str) -> BTreeMap<usize, String> {
    let mut bindings = BTreeMap::new();
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
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
            _ if keyword_at(source, cursor, "class") => {
                let Some(open) = find_class_body_open(source, cursor + "class".len()) else {
                    cursor += "class".len();
                    continue;
                };
                let Some(close) = find_matching_brace(source, open) else {
                    cursor = bytes.len();
                    continue;
                };
                collect_class_field_bindings(source, open + 1, close, &mut bindings);
                cursor = close + 1;
            }
            _ => cursor += 1,
        }
    }
    bindings
}

pub(crate) fn find_class_body_open(source: &str, mut cursor: usize) -> Option<usize> {
    let bytes = source.as_bytes();
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
            b'{' => return Some(cursor),
            b';' => return None,
            _ => cursor += 1,
        }
    }
    None
}

pub(crate) fn collect_class_field_bindings(
    source: &str,
    body_start: usize,
    body_end: usize,
    bindings: &mut BTreeMap<usize, String>,
) {
    let bytes = source.as_bytes();
    let mut cursor = body_start;
    let mut nested_braces = 0usize;
    let mut nested_brackets = 0usize;
    let mut nested_parens = 0usize;
    while cursor < body_end && cursor < bytes.len() {
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
            b'{' => {
                nested_braces += 1;
                cursor += 1;
            }
            b'}' => {
                nested_braces = nested_braces.saturating_sub(1);
                cursor += 1;
            }
            b'[' => {
                nested_brackets += 1;
                cursor += 1;
            }
            b']' => {
                nested_brackets = nested_brackets.saturating_sub(1);
                cursor += 1;
            }
            b'(' => {
                nested_parens += 1;
                cursor += 1;
            }
            b')' => {
                nested_parens = nested_parens.saturating_sub(1);
                cursor += 1;
            }
            byte if nested_braces == 0
                && nested_brackets == 0
                && nested_parens == 0
                && is_identifier_start(byte) =>
            {
                let start = cursor;
                cursor += 1;
                while cursor < body_end
                    && cursor < bytes.len()
                    && is_identifier_continue(bytes[cursor])
                {
                    cursor += 1;
                }
                let after = skip_ws(bytes, cursor);
                if bytes
                    .get(after)
                    .is_some_and(|byte| matches!(*byte, b';' | b'='))
                {
                    bindings.insert(start, source[start..cursor].to_string());
                }
            }
            _ => cursor += 1,
        }
    }
}

pub(crate) fn local_bindings_in_source(source: &str) -> BTreeSet<String> {
    let mut bindings = BTreeSet::new();
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'`' => {
                cursor = collect_template_expression_local_bindings(source, cursor, &mut bindings)
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            b'/' if looks_like_regex_literal(bytes, cursor) => {
                cursor = skip_regex_literal(bytes, cursor);
            }
            _ if keyword_at(source, cursor, "function") => {
                bindings.insert("arguments".to_string());
                if let Some((binding, next)) =
                    parse_identifier_after_function_keyword(source, cursor)
                {
                    bindings.insert(binding.to_string());
                    cursor = next;
                } else {
                    cursor += "function".len();
                }
            }
            _ if keyword_at(source, cursor, "class") => {
                if let Some((binding, next)) =
                    parse_identifier_after_keyword(source, cursor, "class")
                {
                    bindings.insert(binding.to_string());
                    cursor = next;
                } else {
                    cursor += "class".len();
                }
            }
            _ if keyword_at(source, cursor, "var") => {
                cursor =
                    collect_local_variable_bindings(source, cursor + "var".len(), &mut bindings);
            }
            _ if keyword_at(source, cursor, "let") => {
                cursor =
                    collect_local_variable_bindings(source, cursor + "let".len(), &mut bindings);
            }
            _ if keyword_at(source, cursor, "const") => {
                cursor =
                    collect_local_variable_bindings(source, cursor + "const".len(), &mut bindings);
            }
            b'(' => {
                let Some(close) = find_matching_paren(source, cursor) else {
                    cursor += 1;
                    continue;
                };
                let after = skip_ws(bytes, close + 1);
                if keyword_before_paren(source, cursor) == Some("catch") {
                    collect_binding_pattern_identifiers(&source[cursor + 1..close], &mut bindings);
                    cursor = close + 1;
                    continue;
                }
                if !control_flow_keyword_before_paren(source, cursor)
                    && (bytes.get(after) == Some(&b'{')
                        || (bytes.get(after) == Some(&b'=') && bytes.get(after + 1) == Some(&b'>')))
                {
                    collect_binding_pattern_identifiers(&source[cursor + 1..close], &mut bindings);
                }
                cursor += 1;
            }
            byte if is_identifier_start(byte) => {
                let start = cursor;
                cursor += 1;
                while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
                    cursor += 1;
                }
                if bytes.get(skip_ws(bytes, cursor)) == Some(&b'=')
                    && bytes.get(skip_ws(bytes, cursor) + 1) == Some(&b'>')
                {
                    bindings.insert(source[start..cursor].to_string());
                }
            }
            _ => cursor += 1,
        }
    }
    bindings
}

pub(crate) fn collect_template_expression_local_bindings(
    source: &str,
    start: usize,
    bindings: &mut BTreeSet<String>,
) -> usize {
    let bytes = source.as_bytes();
    let mut cursor = start + 1;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor = (cursor + 2).min(bytes.len()),
            b'`' => return cursor + 1,
            b'$' if bytes.get(cursor + 1) == Some(&b'{') => {
                let open = cursor + 1;
                let Some(close) = find_matching_brace(source, open) else {
                    return skip_quoted(bytes, start, b'`');
                };
                bindings.extend(local_bindings_in_source(&source[open + 1..close]));
                cursor = close + 1;
            }
            _ => cursor += 1,
        }
    }
    bytes.len()
}

pub(crate) fn collect_local_variable_bindings(
    source: &str,
    mut cursor: usize,
    bindings: &mut BTreeSet<String>,
) -> usize {
    let bytes = source.as_bytes();
    loop {
        cursor = skip_ws(bytes, cursor);
        if let Some((binding, next)) = parse_identifier(source, cursor) {
            bindings.insert(binding.to_string());
            cursor = next;
        } else if bytes.get(cursor) == Some(&b'{') {
            let Some(end) = find_matching_brace(source, cursor) else {
                return bytes.len();
            };
            collect_binding_pattern_identifiers(&source[cursor + 1..end], bindings);
            cursor = end + 1;
        } else if bytes.get(cursor) == Some(&b'[') {
            let Some(end) = find_matching_bracket(source, cursor) else {
                return bytes.len();
            };
            collect_binding_pattern_identifiers(&source[cursor + 1..end], bindings);
            cursor = end + 1;
        }

        let mut nested = 0usize;
        while cursor < bytes.len() {
            match bytes[cursor] {
                b'\'' | b'"' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
                b'`' => {
                    cursor = collect_template_expression_local_bindings(source, cursor, bindings)
                }
                b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                    cursor = skip_line_comment(bytes, cursor + 2);
                }
                b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                    cursor = skip_block_comment(bytes, cursor + 2);
                }
                b'/' if looks_like_regex_literal(bytes, cursor) => {
                    cursor = skip_regex_literal(bytes, cursor);
                }
                _ if keyword_at(source, cursor, "function")
                    && keyword_starts_statement_declaration(source, cursor) =>
                {
                    bindings.insert("arguments".to_string());
                    if let Some((binding, next)) =
                        parse_identifier_after_function_keyword(source, cursor)
                    {
                        bindings.insert(binding.to_string());
                        cursor = next;
                    } else {
                        cursor += "function".len();
                    }
                }
                _ if keyword_at(source, cursor, "class")
                    && keyword_starts_statement_declaration(source, cursor) =>
                {
                    if let Some((binding, next)) =
                        parse_identifier_after_keyword(source, cursor, "class")
                    {
                        bindings.insert(binding.to_string());
                        cursor = next;
                    } else {
                        cursor += "class".len();
                    }
                }
                _ if keyword_at(source, cursor, "var") => {
                    cursor =
                        collect_local_variable_bindings(source, cursor + "var".len(), bindings);
                }
                _ if keyword_at(source, cursor, "let") => {
                    cursor =
                        collect_local_variable_bindings(source, cursor + "let".len(), bindings);
                }
                _ if keyword_at(source, cursor, "const") => {
                    cursor =
                        collect_local_variable_bindings(source, cursor + "const".len(), bindings);
                }
                b'(' => {
                    if let Some(close) = find_matching_paren(source, cursor) {
                        let after = skip_ws(bytes, close + 1);
                        let captures_binding_pattern = keyword_before_paren(source, cursor)
                            == Some("catch")
                            || (!control_flow_keyword_before_paren(source, cursor)
                                && (bytes.get(after) == Some(&b'{')
                                    || (bytes.get(after) == Some(&b'=')
                                        && bytes.get(after + 1) == Some(&b'>'))));
                        if captures_binding_pattern {
                            collect_binding_pattern_identifiers(
                                &source[cursor + 1..close],
                                bindings,
                            );
                        }
                    }
                    nested += 1;
                    cursor += 1;
                }
                b'[' | b'{' => {
                    nested += 1;
                    cursor += 1;
                }
                b')' | b']' | b'}' => {
                    if nested == 0 {
                        return cursor;
                    }
                    nested -= 1;
                    cursor += 1;
                }
                b',' if nested == 0 => {
                    cursor += 1;
                    break;
                }
                b';' if nested == 0 => return cursor + 1,
                byte if is_identifier_start(byte) => {
                    let start = cursor;
                    cursor += 1;
                    while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
                        cursor += 1;
                    }
                    let after = skip_ws(bytes, cursor);
                    if bytes.get(after) == Some(&b'=') && bytes.get(after + 1) == Some(&b'>') {
                        bindings.insert(source[start..cursor].to_string());
                    }
                }
                _ => cursor += 1,
            }
        }
        if cursor >= bytes.len() {
            return cursor;
        }
    }
}

pub(crate) fn collect_binding_pattern_identifiers(source: &str, bindings: &mut BTreeSet<String>) {
    let mut segment_start = 0usize;
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    let mut depth = 0usize;
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
            b'(' | b'[' | b'{' => {
                depth += 1;
                cursor += 1;
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                cursor += 1;
            }
            b',' if depth == 0 => {
                collect_binding_pattern_segment_identifiers(
                    &source[segment_start..cursor],
                    bindings,
                );
                cursor += 1;
                segment_start = cursor;
            }
            _ => cursor += 1,
        }
    }
    collect_binding_pattern_segment_identifiers(&source[segment_start..], bindings);
}

pub(crate) fn collect_binding_pattern_segment_identifiers(
    source: &str,
    bindings: &mut BTreeSet<String>,
) {
    let pattern_end = top_level_binding_initializer_start(source).unwrap_or(source.len());
    let source = &source[..pattern_end];
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
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
            b'{' => {
                let Some(end) = find_matching_brace(source, cursor) else {
                    return;
                };
                collect_binding_pattern_identifiers(&source[cursor + 1..end], bindings);
                cursor = end + 1;
            }
            b'[' => {
                let Some(end) = find_matching_bracket(source, cursor) else {
                    return;
                };
                collect_binding_pattern_identifiers(&source[cursor + 1..end], bindings);
                cursor = end + 1;
            }
            byte if is_identifier_start(byte) => {
                let start = cursor;
                cursor += 1;
                while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
                    cursor += 1;
                }
                let identifier = &source[start..cursor];
                if !is_js_keyword(identifier) && bytes.get(skip_ws(bytes, cursor)) != Some(&b':') {
                    bindings.insert(identifier.to_string());
                }
            }
            _ => cursor += 1,
        }
    }
}

pub(crate) fn top_level_binding_initializer_start(source: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    let mut depth = 0usize;
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
            b'(' | b'[' | b'{' => {
                depth += 1;
                cursor += 1;
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                cursor += 1;
            }
            b'=' if depth == 0
                && bytes.get(cursor + 1) != Some(&b'>')
                && cursor
                    .checked_sub(1)
                    .and_then(|index| bytes.get(index))
                    .is_none_or(|byte| !matches!(*byte, b'=' | b'!' | b'<' | b'>')) =>
            {
                return Some(cursor);
            }
            _ => cursor += 1,
        }
    }
    None
}

pub(crate) fn keyword_starts_statement_declaration(source: &str, cursor: usize) -> bool {
    let bytes = source.as_bytes();
    previous_non_ws(bytes, cursor).is_none_or(|index| matches!(bytes[index], b'{' | b'}' | b';'))
}

pub(crate) fn is_runtime_global_identifier(identifier: &str) -> bool {
    matches!(
        identifier,
        "AbortController"
            | "AbortSignal"
            | "AggregateError"
            | "Array"
            | "ArrayBuffer"
            | "BigInt"
            | "BigInt64Array"
            | "BigUint64Array"
            | "Blob"
            | "Boolean"
            | "Buffer"
            | "Bun"
            | "ByteLengthQueuingStrategy"
            | "CompressionStream"
            | "CountQueuingStrategy"
            | "CustomEvent"
            | "DataView"
            | "Date"
            | "DecompressionStream"
            | "DOMException"
            | "Error"
            | "Event"
            | "EventTarget"
            | "EvalError"
            | "File"
            | "FinalizationRegistry"
            | "Float32Array"
            | "Float64Array"
            | "FormData"
            | "Function"
            | "Headers"
            | "Infinity"
            | "Int16Array"
            | "Int32Array"
            | "Int8Array"
            | "Intl"
            | "JSON"
            | "Map"
            | "Math"
            | "NaN"
            | "Number"
            | "Object"
            | "Promise"
            | "Proxy"
            | "RangeError"
            | "ReadableStream"
            | "ReferenceError"
            | "Reflect"
            | "RegExp"
            | "Request"
            | "Response"
            | "Screen"
            | "Set"
            | "String"
            | "Symbol"
            | "SyntaxError"
            | "TextDecoder"
            | "TextDecoderStream"
            | "TextEncoder"
            | "TextEncoderStream"
            | "TransformStream"
            | "TypeError"
            | "URIError"
            | "URL"
            | "URLSearchParams"
            | "Uint16Array"
            | "Uint32Array"
            | "Uint8Array"
            | "Uint8ClampedArray"
            | "WeakMap"
            | "WeakRef"
            | "WeakSet"
            | "WebSocket"
            | "WritableStream"
            | "__dirname"
            | "__filename"
            | "atob"
            | "browser"
            | "btoa"
            | "chrome"
            | "clearImmediate"
            | "clearInterval"
            | "clearTimeout"
            | "console"
            | "crypto"
            | "decodeURI"
            | "decodeURIComponent"
            | "document"
            | "encodeURI"
            | "encodeURIComponent"
            | "eval"
            | "exports"
            | "fetch"
            | "global"
            | "globalThis"
            | "isFinite"
            | "isNaN"
            | "localStorage"
            | "location"
            | "module"
            | "navigator"
            | "parseFloat"
            | "parseInt"
            | "performance"
            | "process"
            | "queueMicrotask"
            | "require"
            | "self"
            | "setImmediate"
            | "setInterval"
            | "setTimeout"
            | "structuredClone"
            | "undefined"
            | "unescape"
            | "window"
            | "XMLHttpRequest"
    )
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

pub(crate) fn previous_non_ws(bytes: &[u8], before: usize) -> Option<usize> {
    let mut cursor = before.checked_sub(1)?;
    while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
        cursor = cursor.checked_sub(1)?;
    }
    Some(cursor)
}

pub(crate) fn source_module_wiring(
    program: &EnrichedProgram,
    externalized_packages: &BTreeSet<ModuleId>,
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
        let Some(target_bindings) = exportable_bindings_by_module.get(&target_module_id) else {
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
        bindings.extend(implicit_global_writes_in_source(source.source));
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NamedReexportSpecifier {
    exported: String,
    is_aliased: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NamedImportSpecifier {
    local: String,
    is_aliased: bool,
}

pub(crate) fn source_statements(source: &str) -> Vec<&str> {
    source
        .split(';')
        .map(str::trim)
        .filter(|statement| !statement.is_empty())
        .collect()
}

pub(crate) fn named_reexport_specifiers(statement: &str) -> Option<Vec<NamedReexportSpecifier>> {
    let rest = statement.strip_prefix("export {")?;
    let (inner, after) = rest.split_once('}')?;
    if !after.trim_start().starts_with("from ") {
        return None;
    }
    let mut specifiers = Vec::new();
    for raw in inner.split(',') {
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with("type ") {
            return None;
        }
        let (imported, exported, is_aliased) = raw
            .split_once(" as ")
            .map_or((raw, raw, false), |(imported, exported)| {
                (imported.trim(), exported.trim(), true)
            });
        if !is_identifier_like(imported) || !is_identifier_like(exported) {
            return None;
        }
        specifiers.push(NamedReexportSpecifier {
            exported: exported.to_string(),
            is_aliased,
        });
    }
    Some(specifiers)
}

pub(crate) fn local_named_export_specifiers(
    statement: &str,
) -> Option<Vec<NamedReexportSpecifier>> {
    let rest = statement.strip_prefix("export {")?;
    let (inner, after) = rest.split_once('}')?;
    if !after.trim().is_empty() {
        return None;
    }
    parse_named_export_inner(inner)
}

pub(crate) fn parse_named_export_inner(inner: &str) -> Option<Vec<NamedReexportSpecifier>> {
    let mut specifiers = Vec::new();
    for raw in inner.split(',') {
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with("type ") {
            return None;
        }
        let (imported, exported, is_aliased) = raw
            .split_once(" as ")
            .map_or((raw, raw, false), |(imported, exported)| {
                (imported.trim(), exported.trim(), true)
            });
        if !is_identifier_like(imported) || !is_identifier_like(exported) {
            return None;
        }
        specifiers.push(NamedReexportSpecifier {
            exported: exported.to_string(),
            is_aliased,
        });
    }
    Some(specifiers)
}

pub(crate) fn named_import_specifiers(statement: &str) -> Option<Vec<NamedImportSpecifier>> {
    let rest = statement.strip_prefix("import ")?.trim();
    if rest.starts_with("type ") || rest.contains(" with ") || rest.contains(" assert ") {
        return None;
    }
    let (clause, _specifier) = split_import_clause_and_specifier(rest)?;
    if !clause.starts_with('{') {
        return None;
    }
    Some(
        parse_named_import_clause(clause)?
            .into_iter()
            .map(|(imported, local)| NamedImportSpecifier {
                is_aliased: imported != local,
                local,
            })
            .collect(),
    )
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
    Some(
        program
            .semantic_names()
            .module_path(module.id)
            .unwrap_or(module.semantic_path.as_str())
            .to_string(),
    )
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

pub(crate) fn inline_remaining_lazy_value_wrappers(source: &str) -> (String, bool) {
    inline_remaining_lazy_value_wrappers_with_options(source, false)
}

pub(crate) fn inline_remaining_lazy_value_wrappers_allowing_assignments(
    source: &str,
) -> (String, bool) {
    inline_remaining_lazy_value_wrappers_with_options(source, true)
}

pub(crate) fn localize_lazy_value_source(source: &str) -> Option<String> {
    let (localized, changed) = inline_remaining_lazy_value_wrappers_allowing_assignments(source);
    (changed && !source_contains_top_level_call(localized.as_str(), "lazyValue"))
        .then_some(localized)
}

pub(crate) fn inline_remaining_lazy_value_wrappers_with_options(
    source: &str,
    allow_assignment_factories: bool,
) -> (String, bool) {
    let bytes = source.as_bytes();
    let mut edits = Vec::<(usize, usize, String)>::new();
    let helper_name = local_lazy_value_helper_name(source);
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
            && let Some((decl, after)) =
                try_parse_lazy_value_wrapper_declaration(source, cursor, allow_assignment_factories)
        {
            edits.push((decl.callee_span.0, decl.callee_span.1, helper_name.clone()));
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
        return (source.to_string(), false);
    }
    let source = apply_text_edits(source, &edits);
    (
        format!("{}\n{source}", local_lazy_value_helper_source(&helper_name)),
        true,
    )
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedLazyValueWrapper {
    callee_span: (usize, usize),
}

pub(crate) fn try_parse_lazy_value_wrapper_declaration(
    source: &str,
    start: usize,
    allow_assignment_factory: bool,
) -> Option<(ParsedLazyValueWrapper, usize)> {
    let (_keyword, after_keyword) = declaration_keyword_at(source, start)?;
    let bytes = source.as_bytes();
    let mut cursor = skip_ws(bytes, start + after_keyword);
    let (_binding, after_binding) = parse_identifier(source, cursor)?;
    cursor = skip_ws(bytes, after_binding);
    if bytes.get(cursor) != Some(&b'=') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    let callee_start = cursor;
    if !keyword_at(source, cursor, "lazyValue") {
        return None;
    }
    cursor += "lazyValue".len();
    let callee_end = cursor;
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'(') {
        return None;
    }
    let call_open = cursor;
    let call_close = find_matching_paren(source, call_open)?;
    let factory = source[call_open + 1..call_close].trim();
    if factory.is_empty() {
        return None;
    }
    if !lazy_value_factory_is_zero_arg_arrow(factory) {
        return None;
    }
    // Keep assignment-heavy lazy initializers in their canonical
    // `lazyValue(...)` shape for the runtime-folding pass. Those bodies are
    // often the writer side of runtime mutable bindings; inlining them before
    // fold planning would hide the `X = ...` writes and prevent the safer
    // runtime relocation/folding machinery from seeing them.
    if !allow_assignment_factory && lazy_value_factory_contains_assignment(factory) {
        return None;
    }
    let after_call = skip_ws(bytes, call_close + 1);
    if bytes.get(after_call) != Some(&b';') {
        return None;
    }
    let stmt_end = after_call + 1;
    Some((
        ParsedLazyValueWrapper {
            callee_span: (callee_start, callee_end),
        },
        stmt_end,
    ))
}

pub(crate) fn lazy_value_factory_contains_assignment(factory: &str) -> bool {
    let Some(arrow) = factory.find("=>") else {
        return true;
    };
    let body = &factory[arrow + 2..];
    let bytes = body.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(body, cursor) {
            cursor = next;
            continue;
        }
        if bytes[cursor] != b'=' {
            cursor += 1;
            continue;
        }
        let prev = previous_non_ws(bytes, cursor);
        let next = skip_ws(bytes, cursor + 1);
        let prev_is_operator =
            prev.is_some_and(|idx| matches!(bytes[idx], b'=' | b'!' | b'<' | b'>'));
        let next_is_operator = matches!(bytes.get(next), Some(b'=') | Some(b'>'));
        if !prev_is_operator && !next_is_operator {
            return true;
        }
        cursor += 1;
    }
    false
}

pub(crate) fn lazy_value_factory_is_zero_arg_arrow(factory: &str) -> bool {
    let bytes = factory.as_bytes();
    let mut cursor = skip_ws(bytes, 0);
    if bytes.get(cursor) != Some(&b'(') {
        return false;
    }
    let Some(params_end) = find_matching_paren(factory, cursor) else {
        return false;
    };
    if !factory[cursor + 1..params_end].trim().is_empty() {
        return false;
    }
    cursor = skip_ws(bytes, params_end + 1);
    expect_arrow(bytes, cursor).is_some()
}

pub(crate) fn local_lazy_value_helper_name(source: &str) -> String {
    let mut name = "_$l".to_string();
    let mut suffix = 0usize;
    let local_bindings = local_bindings_in_source(source);
    while local_bindings.contains(name.as_str())
        || contains_identifier_reference(source, name.as_str())
    {
        suffix += 1;
        name = format!("_$l{suffix}");
    }
    name
}

pub(crate) fn local_lazy_value_helper_source(helper_name: &str) -> String {
    format!("var {helper_name}=(_$f,_$v)=>()=>(_$f&&(_$v=_$f(_$f=0)),_$v);")
}

pub(crate) fn inline_remaining_lazy_module_wrappers(source: &str) -> (String, bool) {
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
            && let Some((decl, after)) = try_parse_lazy_module_wrapper_declaration(source, cursor)
        {
            edits.push((
                decl.span.0,
                decl.span.1,
                inline_lazy_module_wrapper_replacement(&decl),
            ));
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
        return (source.to_string(), false);
    }
    (apply_text_edits(source, &edits), true)
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedLazyModuleWrapper<'a> {
    binding: BindingName,
    factory: &'a str,
    span: (usize, usize),
}

pub(crate) fn try_parse_lazy_module_wrapper_declaration(
    source: &str,
    start: usize,
) -> Option<(ParsedLazyModuleWrapper<'_>, usize)> {
    let (_keyword, after_keyword) = declaration_keyword_at(source, start)?;
    let bytes = source.as_bytes();
    let mut cursor = skip_ws(bytes, start + after_keyword);
    let (binding, after_binding) = parse_identifier(source, cursor)?;
    cursor = skip_ws(bytes, after_binding);
    if bytes.get(cursor) != Some(&b'=') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    if !keyword_at(source, cursor, "lazyModule") {
        return None;
    }
    cursor += "lazyModule".len();
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'(') {
        return None;
    }
    let call_open = cursor;
    let call_close = find_matching_paren(source, call_open)?;
    let factory = source[call_open + 1..call_close].trim();
    if factory.is_empty() {
        return None;
    }
    let after_call = skip_ws(bytes, call_close + 1);
    if bytes.get(after_call) != Some(&b';') {
        return None;
    }
    let stmt_end = after_call + 1;
    Some((
        ParsedLazyModuleWrapper {
            binding: BindingName::new(binding),
            factory,
            span: (start, stmt_end),
        },
        stmt_end,
    ))
}

pub(crate) fn inline_lazy_module_wrapper_replacement(decl: &ParsedLazyModuleWrapper<'_>) -> String {
    let binding = decl.binding.as_str();
    format!(
        "var {binding} = (() => {{ let _$cached; return () => {{ if (_$cached) return _$cached.exports; var _$module = _$cached = {{ exports: {{}} }}; ({factory})(_$module.exports, _$module); return _$module.exports; }}; }})();",
        factory = decl.factory,
    )
}

pub(crate) fn lower_commonjs_wrapper_helper(source: &str, helper: &str) -> Option<String> {
    lower_helper_declarations(source, helper, HelperDeclarationKind::CommonJsWrapper)
}

pub(crate) fn lower_lazy_initializer_helper(source: &str, helper: &str) -> Option<String> {
    lower_helper_declarations(source, helper, HelperDeclarationKind::LazyInitializer)
}

#[derive(Debug, Clone)]
pub(crate) struct DelazifyCandidate {
    binding: BindingName,
    /// Byte range covering the full `var X = lazyValue(() => { ... });` statement.
    declaration_span: (usize, usize),
    /// The pure expression extracted from the lazy body's `return EXPR;`.
    value_expr: String,
    /// Spans of every `X()` call site whose `(` `)` get dropped when X is delazified.
    call_sites: Vec<(usize, usize)>,
}

/// Replace `var X = lazyValue(() => { return EXPR; });` with `var X = EXPR;`
/// — and rewrite every `X()` call site to plain `X` — whenever the binding is
/// used only as an immediate-call thunk and EXPR is a pure literal-shape
/// expression (literal, object literal, array literal, class expression, or
/// function expression). Returns the rewritten source unchanged when no
/// candidates qualify, so callers can apply the pass unconditionally.
pub(crate) fn delazify_pure_value_bindings(
    source: &str,
    exported_bindings: &BTreeSet<BindingName>,
    eager_safe_call_targets: &BTreeSet<String>,
) -> (String, BTreeSet<BindingName>) {
    let mut candidates = collect_delazify_candidates(source, eager_safe_call_targets);
    // Cross-module gate: any binding listed in `exported_bindings` is
    // observed by some other module that still calls `X()` against the
    // unlowered surface. Collapsing it to a value here would crash those
    // call sites. The local `collect_safe_call_sites` check only sees uses
    // within this module's bundle slice — explicit export plumbing is the
    // only signal that catches cross-module usage.
    candidates.retain(|cand| !exported_bindings.contains(&cand.binding));
    if candidates.is_empty() {
        return (source.to_string(), BTreeSet::new());
    }
    let changed = candidates.iter().map(|cand| cand.binding.clone()).collect();
    (apply_delazify_rewrites(source, &candidates), changed)
}

pub(crate) fn collect_delazify_candidates(
    source: &str,
    eager_safe_call_targets: &BTreeSet<String>,
) -> Vec<DelazifyCandidate> {
    let bytes = source.as_bytes();
    let mut candidates = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        let parsed = try_parse_lazy_value_declaration(source, cursor, eager_safe_call_targets)
            .or_else(|| try_parse_lazy_module_declaration(source, cursor, eager_safe_call_targets));
        let Some((declaration, after)) = parsed else {
            cursor += 1;
            continue;
        };
        // Verify the binding is referenced only as immediate `X()` calls; any
        // other reference shape (used as value, exported, passed as argument,
        // typeof, etc.) makes delazification unsafe.
        if let Some(call_sites) =
            collect_safe_call_sites(source, declaration.binding.as_str(), declaration.span)
        {
            candidates.push(DelazifyCandidate {
                binding: declaration.binding,
                declaration_span: declaration.span,
                value_expr: declaration.value_expr,
                call_sites,
            });
        }
        cursor = after;
    }
    candidates
}

pub(crate) struct ParsedDelazifiableDeclaration {
    binding: BindingName,
    span: (usize, usize),
    value_expr: String,
}

pub(crate) fn try_parse_lazy_value_declaration(
    source: &str,
    start: usize,
    eager_safe_call_targets: &BTreeSet<String>,
) -> Option<(ParsedDelazifiableDeclaration, usize)> {
    let bytes = source.as_bytes();
    let keyword = ["var", "let", "const"]
        .into_iter()
        .find(|kw| keyword_at(source, start, kw))?;
    let mut cursor = start + keyword.len();
    cursor = skip_ws(bytes, cursor);
    let (binding_name, after_binding) = parse_identifier(source, cursor)?;
    cursor = skip_ws(bytes, after_binding);
    if bytes.get(cursor) != Some(&b'=') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    let prefix = "lazyValue(";
    if !source[cursor..].starts_with(prefix) {
        return None;
    }
    cursor += prefix.len();
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'(') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    if bytes.get(cursor) != Some(&b')') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    cursor = expect_arrow(bytes, cursor)?;
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'{') {
        return None;
    }
    let body_end = find_matching_brace(source, cursor)?;
    let body = &source[cursor + 1..body_end];
    let value_expr = reverts_js::extract_lazy_module_eager_value_with_safe_deps(
        body,
        "",
        None,
        None,
        ParseGoal::TypeScript,
        eager_safe_call_targets,
    )?;
    let after_body = skip_ws(bytes, body_end + 1);
    if bytes.get(after_body) != Some(&b')') {
        return None;
    }
    let after_paren = skip_ws(bytes, after_body + 1);
    if bytes.get(after_paren) != Some(&b';') {
        return None;
    }
    let stmt_end = after_paren + 1;
    Some((
        ParsedDelazifiableDeclaration {
            binding: BindingName::new(binding_name),
            span: (start, stmt_end),
            value_expr,
        },
        stmt_end,
    ))
}

/// Match `var X = lazyModule((EXPORTS, MODULE?) => { ... });` and return the
/// pure value the binding can collapse to. Supports two body shapes:
///   1. Single statement `MODULE.exports = PURE_EXPR;` — covers a CommonJS
///      module that re-exports a single pure value (literal, object/array,
///      class, function).
///   2. Series of `EXPORTS.k = PURE_EXPR_k;` statements — translated to an
///      inline object literal `{ k1: expr1, k2: expr2, ... }`. Covers the
///      `module.exports.foo = ...; module.exports.bar = ...;` style of
///      CommonJS multi-key exports.
///
/// Any other body shape (mixed assignments, control flow, `require` calls,
/// helper declarations) leaves the lazy module intact for a later, more
/// invasive pass to extract to a sibling file.
pub(crate) fn try_parse_lazy_module_declaration(
    source: &str,
    start: usize,
    eager_safe_call_targets: &BTreeSet<String>,
) -> Option<(ParsedDelazifiableDeclaration, usize)> {
    let bytes = source.as_bytes();
    let keyword = ["var", "let", "const"]
        .into_iter()
        .find(|kw| keyword_at(source, start, kw))?;
    let mut cursor = start + keyword.len();
    cursor = skip_ws(bytes, cursor);
    let (binding_name, after_binding) = parse_identifier(source, cursor)?;
    cursor = skip_ws(bytes, after_binding);
    if bytes.get(cursor) != Some(&b'=') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    let prefix = "lazyModule(";
    if !source[cursor..].starts_with(prefix) {
        return None;
    }
    cursor += prefix.len();
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'(') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    let (exports_param, after_first) = parse_identifier(source, cursor)?;
    cursor = skip_ws(bytes, after_first);
    let module_param = if bytes.get(cursor) == Some(&b',') {
        cursor = skip_ws(bytes, cursor + 1);
        let (name, after) = parse_identifier(source, cursor)?;
        cursor = skip_ws(bytes, after);
        Some(name)
    } else {
        None
    };
    if bytes.get(cursor) != Some(&b')') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    cursor = expect_arrow(bytes, cursor)?;
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'{') {
        return None;
    }
    let body_end = find_matching_brace(source, cursor)?;
    let body = &source[cursor + 1..body_end];
    let value_expr = reverts_js::extract_lazy_module_eager_value_with_safe_deps(
        body,
        exports_param,
        module_param,
        None,
        ParseGoal::TypeScript,
        eager_safe_call_targets,
    )?;
    let after_body = skip_ws(bytes, body_end + 1);
    if bytes.get(after_body) != Some(&b')') {
        return None;
    }
    let after_paren = skip_ws(bytes, after_body + 1);
    if bytes.get(after_paren) != Some(&b';') {
        return None;
    }
    let stmt_end = after_paren + 1;
    Some((
        ParsedDelazifiableDeclaration {
            binding: BindingName::new(binding_name),
            span: (start, stmt_end),
            value_expr,
        },
        stmt_end,
    ))
}

/// Walk `source` looking for every identifier reference that equals `binding`,
/// excluding the byte range `exclude_span` (which covers the declaration we are
/// considering rewriting). Each reference must be an immediate-call thunk
/// invocation `X()` — anything else (used as a value, exported, passed as an
/// argument, captured in a closure, etc.) is unsafe to rewrite. Returns
/// `Some(call_sites)` when every reference qualifies; otherwise `None`.
pub(crate) fn collect_safe_call_sites(
    source: &str,
    binding: &str,
    exclude_span: (usize, usize),
) -> Option<Vec<(usize, usize)>> {
    let bytes = source.as_bytes();
    let mut call_sites = Vec::new();
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
        let ident = &source[start..cursor];
        if ident != binding {
            continue;
        }
        // Skip occurrences inside the declaration being rewritten.
        if start >= exclude_span.0 && cursor <= exclude_span.1 {
            continue;
        }
        // Property access `obj.X` / `obj#X` — `X` is a key, not a reference to
        // our binding. Safe to ignore.
        if let Some(prev) = previous_non_ws(bytes, start)
            && matches!(bytes[prev], b'.' | b'#')
        {
            continue;
        }
        // Any other context where `X` is not a value reference (object property
        // key, parameter declaration, assignment target on lhs) makes
        // delazification unsafe.
        if !identifier_occurrence_is_value_reference(source, start, cursor) {
            return None;
        }
        // The reference must be immediately followed by `()` and nothing else
        // that captures `X` as a function (e.g., `X.bind`, `X(arg)` is also
        // unsafe — the lazy thunk takes no args).
        let after = skip_ws(bytes, cursor);
        if bytes.get(after) != Some(&b'(') {
            return None;
        }
        let inner = skip_ws(bytes, after + 1);
        if bytes.get(inner) != Some(&b')') {
            return None;
        }
        call_sites.push((start, inner + 1));
        cursor = inner + 1;
    }
    Some(call_sites)
}

pub(crate) fn apply_delazify_rewrites(source: &str, candidates: &[DelazifyCandidate]) -> String {
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for candidate in candidates {
        edits.push((
            candidate.declaration_span.0,
            candidate.declaration_span.1,
            format!(
                "var {} = {};",
                candidate.binding.as_str(),
                candidate.value_expr
            ),
        ));
        for (start, end) in &candidate.call_sites {
            edits.push((*start, *end, candidate.binding.as_str().to_string()));
        }
    }
    edits.sort_by_key(|(start, _, _)| *start);
    let mut output = String::with_capacity(source.len());
    let mut cursor = 0;
    for (start, end, replacement) in &edits {
        debug_assert!(*start >= cursor, "delazify edits must be non-overlapping");
        output.push_str(&source[cursor..*start]);
        output.push_str(replacement);
        cursor = *end;
    }
    output.push_str(&source[cursor..]);
    output
}

/// Skip any non-code prefix at `cursor`: quoted strings, template literals,
/// line/block comments, or regex literals. Returns the byte position just past
/// the non-code span when something was skipped, or `None` when the current
/// byte begins a code token.
#[derive(Debug, Clone)]
pub(crate) struct DecomposeCandidate {
    /// Byte range covering the full `var X = { ... };` declaration.
    declaration_span: (usize, usize),
    /// Object properties, in their source order. Every value is guaranteed
    /// to be a function expression, arrow expression, or class expression.
    properties: Vec<(String, String)>,
    /// Byte spans of every `X.<key>` access site, with the key that's
    /// being accessed. The full span (from the start of `X` to the end of
    /// the key identifier) gets replaced by the bare key.
    access_sites: Vec<(usize, usize, String)>,
}

/// Decompose `var X = { K1: FN1, K2: FN2, ... };` (where every value is a
/// function/class/arrow expression and X is accessed only as `X.<Ki>`) into
/// individual top-level bindings `var K1 = FN1; var K2 = FN2; ...` and
/// rewrite each `X.Ki` to bare `Ki`. This restores the ESM `export function`
/// shape that a CommonJS module of the form `exports.foo = fn; exports.bar = fn;`
/// would have been written as in idiomatic TypeScript.
///
/// Restricted to value bindings whose entries are *all* function-shape (so the
/// object is really an API namespace rather than a data record); primitive- or
/// mixed-valued objects keep their grouped form because the grouping is the
/// pre-bundle programmer's intent.
pub(crate) fn decompose_function_namespace_objects(
    source: &str,
    exported_bindings: &BTreeSet<BindingName>,
) -> (String, BTreeSet<BindingName>) {
    let mut raw = scan_function_namespace_declarations(source);
    // Cross-module gate (same reasoning as delazify): if the namespace object
    // binding is exported, consumers in other modules still address it as
    // `X.foo` against the pre-lowering namespace shape. Decomposing here
    // would orphan those consumers.
    raw.retain(|cand| !exported_bindings.contains(&cand.binding));
    if raw.is_empty() {
        return (source.to_string(), BTreeSet::new());
    }
    let existing_top_level = top_level_definitions_in_source(source);
    let candidates = filter_decompose_candidates(source, raw, &existing_top_level);
    if candidates.is_empty() {
        return (source.to_string(), BTreeSet::new());
    }
    let mut changed = BTreeSet::<BindingName>::new();
    for cand in &candidates {
        // The namespace object's binding is being removed entirely; its shape
        // changes from "Callable thunk" (IR view) to "doesn't exist".
        // Downstream callers treat the binding as reshaped to clear its
        // IR-inferred shape.
        for (key, _) in &cand.properties {
            changed.insert(BindingName::new(key));
        }
    }
    (apply_decompose_rewrites(source, &candidates), changed)
}

#[derive(Debug, Clone)]
pub(crate) struct RawDecomposeCandidate {
    binding: BindingName,
    declaration_span: (usize, usize),
    properties: Vec<(String, String)>,
}

/// Walk the source at module top level (depth-tracked so we don't pick up
/// `var X = { ... }` declarations nested inside functions or blocks) and
/// collect every `var X = { ... };` whose object literal has only
/// function-shape values.
pub(crate) fn scan_function_namespace_declarations(source: &str) -> Vec<RawDecomposeCandidate> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut cursor = 0;
    let mut brace_depth: i32 = 0;
    let mut paren_depth: i32 = 0;
    let mut bracket_depth: i32 = 0;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        match bytes[cursor] {
            b'{' => {
                brace_depth += 1;
                cursor += 1;
                continue;
            }
            b'}' => {
                brace_depth -= 1;
                cursor += 1;
                continue;
            }
            b'(' => {
                paren_depth += 1;
                cursor += 1;
                continue;
            }
            b')' => {
                paren_depth -= 1;
                cursor += 1;
                continue;
            }
            b'[' => {
                bracket_depth += 1;
                cursor += 1;
                continue;
            }
            b']' => {
                bracket_depth -= 1;
                cursor += 1;
                continue;
            }
            _ => {}
        }
        if brace_depth > 0 || paren_depth > 0 || bracket_depth > 0 {
            cursor += 1;
            continue;
        }
        if let Some((cand, after)) = try_parse_function_namespace_declaration(source, cursor) {
            out.push(cand);
            cursor = after;
        } else {
            cursor += 1;
        }
    }
    out
}

pub(crate) fn try_parse_function_namespace_declaration(
    source: &str,
    start: usize,
) -> Option<(RawDecomposeCandidate, usize)> {
    let bytes = source.as_bytes();
    let keyword = ["var", "let", "const"]
        .into_iter()
        .find(|kw| keyword_at(source, start, kw))?;
    let mut cursor = start + keyword.len();
    cursor = skip_ws(bytes, cursor);
    let (binding_name, after_binding) = parse_identifier(source, cursor)?;
    cursor = skip_ws(bytes, after_binding);
    if bytes.get(cursor) != Some(&b'=') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    if bytes.get(cursor) != Some(&b'{') {
        return None;
    }
    let object_open = cursor;
    let object_close = find_matching_brace(source, object_open)?;
    let inner = &source[object_open + 1..object_close];
    let after_object = skip_ws(bytes, object_close + 1);
    if bytes.get(after_object) != Some(&b';') {
        return None;
    }
    let stmt_end = after_object + 1;

    let properties = parse_function_namespace_properties(inner)?;
    Some((
        RawDecomposeCandidate {
            binding: BindingName::new(binding_name),
            declaration_span: (start, stmt_end),
            properties,
        },
        stmt_end,
    ))
}

/// Parse the inside of an object literal into a list of `(key, value)` pairs
/// — but only succeed when every entry is a `KEY: FUNCTION_SHAPE` pair with a
/// bare identifier key. Rejects:
///   * computed keys (`['x']: ...`),
///   * shorthand (`{ x }`),
///   * spread elements,
///   * primitive or other non-function values,
///   * any value that is not a function/class/arrow expression.
pub(crate) fn parse_function_namespace_properties(inner: &str) -> Option<Vec<(String, String)>> {
    let entries = split_top_level_commas(inner);
    let mut properties = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in entries {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let entry_bytes = entry.as_bytes();
        if !is_identifier_start(entry_bytes[0]) {
            return None;
        }
        let mut key_end = 1;
        while key_end < entry_bytes.len() && is_identifier_continue(entry_bytes[key_end]) {
            key_end += 1;
        }
        let key = &entry[..key_end];
        if !seen.insert(key.to_string()) {
            return None;
        }
        let after_key = entry[key_end..].trim_start();
        let after_colon = after_key.strip_prefix(':')?;
        let value = after_colon.trim();
        if value.is_empty() || !is_function_or_class_value(value) {
            return None;
        }
        properties.push((key.to_string(), value.to_string()));
    }
    if properties.is_empty() {
        return None;
    }
    Some(properties)
}

/// True iff `source` (after trimming) is a function expression, class
/// expression, or arrow function expression. Used to detect "API surface"
/// values for the namespace decomposition pass — only those decompose.
pub(crate) fn is_function_or_class_value(source: &str) -> bool {
    let trimmed = source.trim();
    if keyword_at(trimmed, 0, "class") {
        return pure_class_expression(trimmed);
    }
    if keyword_at(trimmed, 0, "function") {
        return true;
    }
    looks_like_arrow_function_expression(trimmed)
}

/// Split `source` on top-level `,` separators (those not nested inside
/// `(...)`, `[...]`, `{...}`, strings, templates, regex, or comments).
pub(crate) fn split_top_level_commas(source: &str) -> Vec<&str> {
    let bytes = source.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut cursor = 0;
    let mut paren: i32 = 0;
    let mut bracket: i32 = 0;
    let mut brace: i32 = 0;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        match bytes[cursor] {
            b'(' => {
                paren += 1;
                cursor += 1;
            }
            b')' => {
                paren -= 1;
                cursor += 1;
            }
            b'[' => {
                bracket += 1;
                cursor += 1;
            }
            b']' => {
                bracket -= 1;
                cursor += 1;
            }
            b'{' => {
                brace += 1;
                cursor += 1;
            }
            b'}' => {
                brace -= 1;
                cursor += 1;
            }
            b',' if paren == 0 && bracket == 0 && brace == 0 => {
                parts.push(&source[start..cursor]);
                cursor += 1;
                start = cursor;
            }
            _ => cursor += 1,
        }
    }
    parts.push(&source[start..]);
    parts
}

/// Filter raw candidates by the two cross-cutting safety checks:
///   * X must be referenced only as `X.<known_key>` (no other usage shape).
///   * None of the property keys may collide with an existing top-level
///     binding (other than the X about to be removed) or with a key already
///     introduced by a prior accepted candidate.
pub(crate) fn filter_decompose_candidates(
    source: &str,
    raw: Vec<RawDecomposeCandidate>,
    existing_top_level: &BTreeSet<BindingName>,
) -> Vec<DecomposeCandidate> {
    let mut introduced = BTreeSet::<BindingName>::new();
    let mut accepted = Vec::new();
    for cand in raw {
        let mut would_introduce = Vec::new();
        let mut safe = true;
        for (key, _) in &cand.properties {
            let key_binding = BindingName::new(key);
            // The binding X disappears after rewrite, so a key matching X is
            // harmless on its own — but skip it to keep things simple.
            if key_binding == cand.binding {
                continue;
            }
            if existing_top_level.contains(&key_binding) || introduced.contains(&key_binding) {
                safe = false;
                break;
            }
            would_introduce.push(key_binding);
        }
        if !safe {
            continue;
        }
        let Some(access_sites) = collect_member_access_only(
            source,
            cand.binding.as_str(),
            cand.declaration_span,
            &cand.properties,
        ) else {
            continue;
        };
        // Refuse no-op decompositions: if X is declared but never accessed,
        // dropping the declaration would discard the function/class
        // definitions entirely (they might still be observable through
        // side effects if the binding is exported, but the export check
        // happens via `collect_member_access_only` returning None for any
        // non-`X.<key>` reference, including export specifiers).
        if access_sites.is_empty() {
            continue;
        }
        introduced.extend(would_introduce);
        accepted.push(DecomposeCandidate {
            declaration_span: cand.declaration_span,
            properties: cand.properties,
            access_sites,
        });
    }
    accepted
}

/// Walk `source` and verify every reference to `binding` (outside the
/// candidate's own declaration span) is a `binding.<key>` member access whose
/// key is one of `properties`' keys. Returns the list of access-site spans
/// when every reference qualifies, or `None` when any reference is unsafe
/// (export specifier, value passed as argument, used as `typeof`, accessed
/// with a different key, etc.).
pub(crate) fn collect_member_access_only(
    source: &str,
    binding: &str,
    exclude_span: (usize, usize),
    properties: &[(String, String)],
) -> Option<Vec<(usize, usize, String)>> {
    let valid_keys: BTreeSet<&str> = properties.iter().map(|(k, _)| k.as_str()).collect();
    let bytes = source.as_bytes();
    let mut sites = Vec::new();
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
        let ident = &source[start..cursor];
        if ident != binding {
            continue;
        }
        if start >= exclude_span.0 && cursor <= exclude_span.1 {
            continue;
        }
        // Property access `obj.X` / `obj#X` — X here is a property key, not
        // our binding. Skip.
        if let Some(prev) = previous_non_ws(bytes, start)
            && matches!(bytes[prev], b'.' | b'#')
        {
            continue;
        }
        if !identifier_occurrence_is_value_reference(source, start, cursor) {
            return None;
        }
        let after = skip_ws(bytes, cursor);
        if bytes.get(after) != Some(&b'.') {
            return None;
        }
        let key_start = skip_ws(bytes, after + 1);
        if key_start >= bytes.len() || !is_identifier_start(bytes[key_start]) {
            return None;
        }
        let mut key_end = key_start + 1;
        while key_end < bytes.len() && is_identifier_continue(bytes[key_end]) {
            key_end += 1;
        }
        let accessed_key = &source[key_start..key_end];
        if !valid_keys.contains(accessed_key) {
            return None;
        }
        sites.push((start, key_end, accessed_key.to_string()));
        cursor = key_end;
    }
    Some(sites)
}

pub(crate) fn apply_decompose_rewrites(source: &str, candidates: &[DecomposeCandidate]) -> String {
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for candidate in candidates {
        let mut new_decl = String::new();
        for (i, (key, value)) in candidate.properties.iter().enumerate() {
            if i > 0 {
                new_decl.push('\n');
            }
            new_decl.push_str(&format!("var {key} = {value};"));
        }
        edits.push((
            candidate.declaration_span.0,
            candidate.declaration_span.1,
            new_decl,
        ));
        for (start, end, key) in &candidate.access_sites {
            edits.push((*start, *end, key.clone()));
        }
    }
    edits.sort_by_key(|(start, _, _)| *start);
    let mut output = String::with_capacity(source.len());
    let mut cursor = 0;
    for (start, end, replacement) in &edits {
        debug_assert!(*start >= cursor, "decompose edits must not overlap");
        output.push_str(&source[cursor..*start]);
        output.push_str(replacement);
        cursor = *end;
    }
    output.push_str(&source[cursor..]);
    output
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HelperDeclarationKind {
    CommonJsWrapper,
    LazyInitializer,
}

pub(crate) fn lower_helper_declarations(
    source: &str,
    helper: &str,
    kind: HelperDeclarationKind,
) -> Option<String> {
    let mut output = String::new();
    let mut index = 0;
    let mut changed = false;
    while let Some(declaration) = find_helper_declaration(source, index, helper, kind) {
        output.push_str(&source[index..declaration.start]);
        output.push_str(declaration.replacement.as_str());
        index = declaration.end;
        changed = true;
    }
    if changed {
        output.push_str(&source[index..]);
        Some(output)
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HelperDeclaration {
    start: usize,
    end: usize,
    replacement: String,
}

pub(crate) fn find_helper_declaration(
    source: &str,
    from: usize,
    helper: &str,
    kind: HelperDeclarationKind,
) -> Option<HelperDeclaration> {
    let bytes = source.as_bytes();
    let mut index = from;
    while index < bytes.len() {
        let (start, keyword) = find_declaration_keyword(source, index)?;
        let mut cursor = start + keyword.len();
        cursor = skip_ws(bytes, cursor);
        let Some((binding, after_binding)) = parse_identifier(source, cursor) else {
            index = start + keyword.len();
            continue;
        };
        cursor = skip_ws(bytes, after_binding);
        if bytes.get(cursor) != Some(&b'=') {
            index = start + keyword.len();
            continue;
        }
        cursor = skip_ws(bytes, cursor + 1);
        let Some((callee, after_callee)) = parse_identifier(source, cursor) else {
            index = cursor.saturating_add(1);
            continue;
        };
        if callee != helper {
            index = after_callee;
            continue;
        }
        cursor = skip_ws(bytes, after_callee);
        if bytes.get(cursor) != Some(&b'(') {
            index = after_callee;
            continue;
        }
        let parsed = match kind {
            HelperDeclarationKind::CommonJsWrapper => {
                parse_commonjs_wrapper_replacement(source, cursor + 1, binding)
            }
            HelperDeclarationKind::LazyInitializer => {
                parse_lazy_initializer_replacement(source, cursor + 1, binding)
            }
        };
        let Some((replacement, end)) = parsed else {
            index = after_callee;
            continue;
        };
        return Some(HelperDeclaration {
            start,
            end,
            replacement,
        });
    }
    None
}

pub(crate) fn parse_commonjs_wrapper_replacement(
    source: &str,
    mut cursor: usize,
    binding: &str,
) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'(') {
        return None;
    }
    let params_end = find_byte(bytes, cursor + 1, b')')?;
    let params = source[cursor + 1..params_end]
        .split(',')
        .map(str::trim)
        .filter(|param| !param.is_empty())
        .collect::<Vec<_>>();
    if params.is_empty() {
        return None;
    }
    cursor = skip_ws(bytes, params_end + 1);
    cursor = expect_arrow(bytes, cursor)?;
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'{') {
        return None;
    }
    let body_end = find_matching_brace(source, cursor)?;
    let body = source[cursor + 1..body_end].trim();
    let end = parse_helper_call_end(bytes, body_end + 1)?;
    let exports = params[0];
    let module_alias = params.get(1).copied();
    let parameter_list = match module_alias {
        Some(module) => format!("({exports}, {module})"),
        None => format!("({exports})"),
    };
    Some((
        format!("var {binding} = lazyModule({parameter_list} => {{\n{body}\n}});"),
        end,
    ))
}

pub(crate) fn parse_lazy_initializer_replacement(
    source: &str,
    mut cursor: usize,
    binding: &str,
) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'(') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    if bytes.get(cursor) != Some(&b')') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    cursor = expect_arrow(bytes, cursor)?;
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b'{') {
        return None;
    }
    let body_end = find_matching_brace(source, cursor)?;
    let body = source[cursor + 1..body_end].trim();
    let end = parse_helper_call_end(bytes, body_end + 1)?;
    Some((
        format!("var {binding} = lazyValue(() => {{\n{body}\n}});"),
        end,
    ))
}

pub(crate) fn parse_helper_call_end(bytes: &[u8], mut cursor: usize) -> Option<usize> {
    cursor = skip_ws(bytes, cursor);
    if bytes.get(cursor) != Some(&b')') {
        return None;
    }
    cursor = skip_ws(bytes, cursor + 1);
    if bytes.get(cursor) == Some(&b';') {
        cursor += 1;
    }
    Some(cursor)
}

pub(crate) fn identifiers_in_source(source: &str) -> BTreeSet<String> {
    value_identifiers_in_source(source)
}

pub(crate) fn top_level_definitions_in_source(source: &str) -> BTreeSet<BindingName> {
    let mut definitions = BTreeSet::new();
    let bytes = source.as_bytes();
    let mut cursor = 0;
    let mut depth = 0usize;
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
            b'{' => {
                depth += 1;
                cursor += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                cursor += 1;
            }
            _ if depth == 0 && keyword_at(source, cursor, "function") => {
                if let Some((binding, next)) =
                    parse_identifier_after_function_keyword(source, cursor)
                {
                    definitions.insert(BindingName::new(binding));
                    cursor = next;
                } else {
                    cursor += "function".len();
                }
            }
            _ if depth == 0 && keyword_at(source, cursor, "class") => {
                if let Some((binding, next)) =
                    parse_identifier_after_keyword(source, cursor, "class")
                {
                    definitions.insert(BindingName::new(binding));
                    cursor = next;
                } else {
                    cursor += "class".len();
                }
            }
            _ if depth == 0 && keyword_at(source, cursor, "var") => {
                cursor = collect_variable_declaration_definitions(
                    source,
                    cursor + "var".len(),
                    &mut definitions,
                );
            }
            _ if depth == 0 && keyword_at(source, cursor, "let") => {
                cursor = collect_variable_declaration_definitions(
                    source,
                    cursor + "let".len(),
                    &mut definitions,
                );
            }
            _ if depth == 0 && keyword_at(source, cursor, "const") => {
                cursor = collect_variable_declaration_definitions(
                    source,
                    cursor + "const".len(),
                    &mut definitions,
                );
            }
            _ => cursor += 1,
        }
    }
    definitions
}

pub(crate) fn implicit_global_declarations_for_module(
    source: &str,
    source_definitions: &BTreeSet<BindingName>,
    source_imports: &BTreeSet<BindingName>,
    planned_bindings: &BTreeSet<BindingName>,
) -> BTreeSet<BindingName> {
    let top_level_definitions = top_level_definitions_in_source(source);
    implicit_global_writes_in_source(source)
        .into_iter()
        .filter(|binding| !top_level_definitions.contains(binding))
        .filter(|binding| !source_definitions.contains(binding))
        .filter(|binding| !source_imports.contains(binding))
        .filter(|binding| !planned_bindings.contains(binding))
        .filter(|binding| !is_planner_synthetic_binding(binding.as_str()))
        .collect()
}

pub(crate) fn implicit_global_writes_in_source(source: &str) -> BTreeSet<BindingName> {
    let mut writes = BTreeSet::new();
    let declaration_bindings = variable_declaration_binding_starts(source);
    let class_field_bindings = class_field_bindings_in_source(source);
    let bytes = source.as_bytes();
    let mut cursor = 0;
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
            b'+' | b'-' if update_operator_at(bytes, cursor).is_some() => {
                let target_start = skip_ws(bytes, cursor + 2);
                if let Some((identifier, target_end)) = parse_identifier(source, target_start)
                    && is_simple_update_target(source, target_start, target_end)
                {
                    writes.insert(BindingName::new(identifier));
                }
                cursor += 1;
            }
            b'{' => {
                if let Some((end, bindings)) =
                    object_destructuring_assignment_writes(source, cursor)
                {
                    writes.extend(bindings);
                    cursor = end;
                } else {
                    cursor += 1;
                }
            }
            b'[' => {
                if let Some((end, bindings)) = array_destructuring_assignment_writes(source, cursor)
                {
                    writes.extend(bindings);
                    cursor = end;
                } else {
                    cursor += 1;
                }
            }
            byte if is_identifier_start(byte) => {
                let start = cursor;
                cursor += 1;
                while cursor < bytes.len() && is_identifier_continue(bytes[cursor]) {
                    cursor += 1;
                }
                if declaration_bindings.contains(&start) {
                    continue;
                }
                let identifier = &source[start..cursor];
                if !is_js_keyword(identifier)
                    && start
                        .checked_sub(1)
                        .and_then(|index| bytes.get(index))
                        .is_none_or(|byte| !matches!(*byte, b'.' | b'#'))
                    && !class_field_bindings.contains_key(&start)
                {
                    let after = skip_ws(bytes, cursor);
                    if (bytes.get(after) == Some(&b'=')
                        && bytes.get(after + 1) != Some(&b'=')
                        && bytes.get(after + 1) != Some(&b'>'))
                        || update_operator_at(bytes, after).is_some()
                    {
                        writes.insert(BindingName::new(identifier));
                    }
                }
            }
            _ => cursor += 1,
        }
    }
    writes
}

pub(crate) fn collect_variable_declaration_definitions(
    source: &str,
    mut cursor: usize,
    definitions: &mut BTreeSet<BindingName>,
) -> usize {
    let bytes = source.as_bytes();
    cursor = skip_ws(bytes, cursor);
    if let Some((binding, next)) = parse_identifier(source, cursor) {
        definitions.insert(BindingName::new(binding));
        cursor = next;
    }

    let mut nested = 0usize;
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
            b'(' | b'[' | b'{' => {
                nested += 1;
                cursor += 1;
            }
            b')' | b']' | b'}' => {
                nested = nested.saturating_sub(1);
                cursor += 1;
            }
            b',' if nested == 0 => {
                cursor = skip_ws(bytes, cursor + 1);
                if let Some((binding, next)) = parse_identifier(source, cursor) {
                    definitions.insert(BindingName::new(binding));
                    cursor = next;
                }
            }
            b';' if nested == 0 => return cursor + 1,
            _ => cursor += 1,
        }
    }
    cursor
}

pub(crate) fn variable_declaration_binding_starts(source: &str) -> BTreeSet<usize> {
    let mut starts = BTreeSet::new();
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
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
            _ if keyword_at(source, cursor, "var") => {
                cursor = collect_variable_declaration_binding_starts(
                    source,
                    cursor + "var".len(),
                    &mut starts,
                );
            }
            _ if keyword_at(source, cursor, "let") => {
                cursor = collect_variable_declaration_binding_starts(
                    source,
                    cursor + "let".len(),
                    &mut starts,
                );
            }
            _ if keyword_at(source, cursor, "const") => {
                cursor = collect_variable_declaration_binding_starts(
                    source,
                    cursor + "const".len(),
                    &mut starts,
                );
            }
            _ => cursor += 1,
        }
    }
    starts
}

pub(crate) fn collect_variable_declaration_binding_starts(
    source: &str,
    mut cursor: usize,
    starts: &mut BTreeSet<usize>,
) -> usize {
    let bytes = source.as_bytes();
    cursor = skip_ws(bytes, cursor);
    if parse_identifier(source, cursor).is_some() {
        starts.insert(cursor);
    }

    let mut nested = 0usize;
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
            b'(' | b'[' | b'{' => {
                nested += 1;
                cursor += 1;
            }
            b')' if nested == 0 => return cursor + 1,
            b')' | b']' | b'}' => {
                nested = nested.saturating_sub(1);
                cursor += 1;
            }
            b',' if nested == 0 => {
                cursor = skip_ws(bytes, cursor + 1);
                if parse_identifier(source, cursor).is_some() {
                    starts.insert(cursor);
                }
            }
            b';' if nested == 0 => return cursor + 1,
            _ => cursor += 1,
        }
    }
    cursor
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
