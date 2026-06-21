//! Inline singleton-consumer runtime helper snippets into their unique consumer.
//!
//! When a runtime helper binding has exactly one consumer module and the
//! prelude itself doesn't depend on it for anything else, the consumer
//! can lift the snippet's source body directly into its own file —
//! removing the cross-module import (and the corresponding runtime
//! export) entirely. `runtime_singleton_inline_plan` computes which
//! bindings qualify; `emit_runtime_singleton_inline_helpers` actually
//! splices the inlined snippets into the consumer's planned file.
//!
//! Conservative gating:
//!
//! 1. Exactly one consumer module imports the binding.
//! 2. The binding is not already migrated by
//!    `runtime_var_migration` (that owner takes precedence).
//! 3. The binding is not in the excluded set (synthetic setters,
//!    `lazyModule`, `lazyValue`, `__reverts_*`).
//! 4. The consumer doesn't already declare a same-name binding (no
//!    silent shadowing).
//! 5. The transitive closure of free identifiers in the snippet stays
//!    inside the bindings the consumer can name (either via existing
//!    source imports, direct prelude imports, or other inlined
//!    singletons).
//!
//! `resolve_runtime_singleton_inline_snippet` does the closure walk;
//! `partition_runtime_singleton_inline_bindings` splits a "remaining
//! runtime helpers" set into "stays as a runtime import" vs.
//! "inlined into this consumer".

use std::collections::{BTreeMap, BTreeSet};

use reverts_graph::RuntimePrelude;
use reverts_ir::{BindingName, BindingShape, ModuleId};
use reverts_model::EnrichedProgram;

use crate::identifiers::is_planner_synthetic_binding;
use crate::runtime_source_read::{
    RuntimeSourceReadIndex, runtime_readers_for_binding, runtime_source_read_index,
};
use crate::{
    LoweredRuntimeModuleSource, PlannedBinding, PlannedFile, RuntimeLazyFoldPlan,
    RuntimePreludeDirectImport, RuntimeVarMigrationPlan, SourceModuleWiring,
    emit_direct_owner_imports, emit_direct_prelude_imports, implicit_global_writes_in_source,
    is_migratable_reader_function_snippet, is_pure_initializer_expression,
    module_dependency_modules_by_owner, module_dependency_path_exists,
    runtime_binding_has_blocking_non_snippet_use, runtime_owner_definition_modules,
    strip_top_level_named_exports,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeSingletonInlineSnippet {
    pub(crate) consumer_module: ModuleId,
    pub(crate) byte_start: u32,
    pub(crate) snippets: Vec<RuntimeSingletonInlineSnippetSource>,
    pub(crate) direct_prelude_imports: BTreeMap<BindingName, RuntimePreludeDirectImport>,
    pub(crate) source_imports: BTreeMap<ModuleId, BTreeSet<BindingName>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeSingletonInlineSnippetSource {
    pub(crate) binding: BindingName,
    pub(crate) byte_start: u32,
    pub(crate) source: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeSingletonInlinePlan {
    pub(crate) snippets_by_binding: BTreeMap<(u32, BindingName), RuntimeSingletonInlineSnippet>,
}

pub(crate) fn runtime_singleton_inline_plan(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
    lowered_runtime_sources: &BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
    runtime_lazy_folds: &RuntimeLazyFoldPlan,
    runtime_var_migrations: &RuntimeVarMigrationPlan,
    direct_prelude_imports: &BTreeMap<u32, BTreeMap<BindingName, RuntimePreludeDirectImport>>,
    externalized_packages: &BTreeSet<ModuleId>,
) -> RuntimeSingletonInlinePlan {
    let source_definition_modules =
        runtime_owner_definition_modules(program, externalized_packages);
    let source_exported_bindings_by_module =
        source_exported_bindings_by_module(program, source_module_wiring);
    let module_dependencies_by_owner = module_dependency_modules_by_owner(program);
    let mut consumers_by_binding = BTreeMap::<(u32, BindingName), BTreeSet<ModuleId>>::new();
    let mut blocked_bindings = BTreeSet::<(u32, BindingName)>::new();

    for module in program.model().modules() {
        if externalized_packages.contains(&module.id) {
            continue;
        }
        let mut used_by_source = BTreeMap::<u32, BTreeSet<BindingName>>::new();
        for import in program.model().graph().runtime_imports_for(module.id) {
            used_by_source
                .entry(import.source_file_id)
                .or_default()
                .insert(import.binding);
        }
        if let Some(source) = lowered_runtime_sources.get(&module.id) {
            used_by_source
                .entry(source.source_file_id)
                .or_default()
                .extend(source.remaining_helpers.iter().cloned());
            for written in &source.written_helpers {
                blocked_bindings.insert((source.source_file_id, written.clone()));
            }
        }
        for (source_file_id, bindings) in used_by_source {
            for binding in bindings {
                let key = (source_file_id, binding.clone());
                if is_runtime_singleton_inline_excluded_binding(&binding)
                    || runtime_var_migrations
                        .migrated_owner(source_file_id, &binding)
                        .is_some()
                {
                    blocked_bindings.insert(key);
                    continue;
                }
                consumers_by_binding
                    .entry(key)
                    .or_default()
                    .insert(module.id);
            }
        }
    }

    let mut plan = RuntimeSingletonInlinePlan::default();
    let mut read_indices_by_source = BTreeMap::<u32, RuntimeSourceReadIndex>::new();
    for ((source_file_id, binding), consumers) in &consumers_by_binding {
        if consumers.len() != 1 || blocked_bindings.contains(&(*source_file_id, binding.clone())) {
            continue;
        }
        let consumer_module = *consumers
            .iter()
            .next()
            .expect("singleton consumer set must contain one module");
        if externalized_packages.contains(&consumer_module) {
            continue;
        }
        if runtime_singleton_inline_consumer_has_name_conflict(
            program,
            lowered_runtime_sources,
            consumer_module,
            binding,
        ) {
            continue;
        }
        let Some(prelude) = program.model().graph().runtime_prelude(*source_file_id) else {
            continue;
        };
        read_indices_by_source
            .entry(*source_file_id)
            .or_insert_with(|| {
                let folded_chunks = runtime_lazy_folds
                    .chunks_by_source_file
                    .get(source_file_id)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                runtime_source_read_index(prelude, folded_chunks)
            });
        let read_index = read_indices_by_source
            .get(source_file_id)
            .expect("runtime read index should be cached");
        let singleton_ctx = RuntimeSingletonInlineContext {
            program,
            lowered_runtime_sources,
            runtime_var_migrations,
            prelude,
            read_index,
            source_file_id: *source_file_id,
            consumers_by_binding: &consumers_by_binding,
            blocked_bindings: &blocked_bindings,
            direct_prelude_imports: direct_prelude_imports.get(source_file_id),
            source_definition_modules: &source_definition_modules,
            source_exported_bindings_by_module: &source_exported_bindings_by_module,
            module_dependencies_by_owner: &module_dependencies_by_owner,
        };
        let Some(inline_snippet) =
            resolve_runtime_singleton_inline_snippet(&singleton_ctx, binding, consumer_module)
        else {
            continue;
        };
        plan.snippets_by_binding
            .insert((*source_file_id, binding.clone()), inline_snippet);
    }

    plan
}

pub(crate) struct RuntimeSingletonInlineContext<'a> {
    pub(crate) program: &'a EnrichedProgram,
    pub(crate) lowered_runtime_sources: &'a BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
    pub(crate) runtime_var_migrations: &'a RuntimeVarMigrationPlan,
    pub(crate) prelude: &'a RuntimePrelude,
    pub(crate) read_index: &'a RuntimeSourceReadIndex,
    pub(crate) source_file_id: u32,
    pub(crate) consumers_by_binding: &'a BTreeMap<(u32, BindingName), BTreeSet<ModuleId>>,
    pub(crate) blocked_bindings: &'a BTreeSet<(u32, BindingName)>,
    pub(crate) direct_prelude_imports:
        Option<&'a BTreeMap<BindingName, RuntimePreludeDirectImport>>,
    pub(crate) source_definition_modules: &'a BTreeMap<BindingName, Option<ModuleId>>,
    pub(crate) source_exported_bindings_by_module: &'a BTreeMap<ModuleId, BTreeSet<BindingName>>,
    pub(crate) module_dependencies_by_owner: &'a BTreeMap<ModuleId, BTreeSet<ModuleId>>,
}

pub(crate) fn resolve_runtime_singleton_inline_snippet(
    ctx: &RuntimeSingletonInlineContext<'_>,
    root_binding: &BindingName,
    consumer_module: ModuleId,
) -> Option<RuntimeSingletonInlineSnippet> {
    let mut cluster_bindings = BTreeSet::<BindingName>::new();
    let mut direct_imports = BTreeMap::<BindingName, RuntimePreludeDirectImport>::new();
    let mut source_imports = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
    let mut queue = vec![root_binding.clone()];

    while let Some(binding) = queue.pop() {
        if !cluster_bindings.insert(binding.clone()) {
            continue;
        }
        let key = (ctx.source_file_id, binding.clone());
        if ctx.blocked_bindings.contains(&key)
            || ctx
                .runtime_var_migrations
                .migrated_owner(ctx.source_file_id, &binding)
                .is_some()
            || is_runtime_singleton_inline_excluded_binding(&binding)
        {
            return None;
        }
        if ctx.consumers_by_binding.get(&key).is_some_and(|consumers| {
            consumers
                .iter()
                .any(|consumer| *consumer != consumer_module)
        }) {
            return None;
        }
        if runtime_singleton_inline_consumer_has_name_conflict(
            ctx.program,
            ctx.lowered_runtime_sources,
            consumer_module,
            &binding,
        ) {
            return None;
        }
        if runtime_binding_has_blocking_non_snippet_use(ctx.read_index, &binding)
            || ctx
                .read_index
                .namespace_exports_by_namespace
                .contains_key(&binding)
        {
            return None;
        }
        let snippet = ctx.prelude.snippets.get(&binding)?;
        if !is_runtime_singleton_inline_snippet(&binding, snippet.source.as_str()) {
            return None;
        }
        if implicit_global_writes_in_source(snippet.source.as_str())
            .into_iter()
            .any(|write| ctx.prelude.defines(&write))
        {
            return None;
        }
        let free_bindings = ctx.read_index.free_bindings_by_snippet.get(&binding)?;
        for free_binding in free_bindings {
            if cluster_bindings.contains(free_binding) {
                continue;
            }
            if let Some(import) = ctx
                .direct_prelude_imports
                .and_then(|imports| imports.get(free_binding))
            {
                if runtime_singleton_inline_consumer_has_name_conflict(
                    ctx.program,
                    ctx.lowered_runtime_sources,
                    consumer_module,
                    free_binding,
                ) {
                    return None;
                }
                direct_imports.insert(free_binding.clone(), import.clone());
                continue;
            }
            if ctx.prelude.defines(free_binding) {
                queue.push(free_binding.clone());
                continue;
            }
            if let Some(dep_module) = ctx
                .source_definition_modules
                .get(free_binding)
                .and_then(|module_id| *module_id)
            {
                if dep_module == consumer_module
                    || !ctx
                        .source_exported_bindings_by_module
                        .get(&dep_module)
                        .is_some_and(|exports| exports.contains(free_binding))
                    || module_dependency_path_exists(
                        ctx.module_dependencies_by_owner,
                        dep_module,
                        consumer_module,
                    )
                    || runtime_singleton_inline_consumer_has_name_conflict(
                        ctx.program,
                        ctx.lowered_runtime_sources,
                        consumer_module,
                        free_binding,
                    )
                {
                    return None;
                }
                source_imports
                    .entry(dep_module)
                    .or_default()
                    .insert(free_binding.clone());
                continue;
            }
            return None;
        }
    }

    if !cluster_bindings.iter().all(|binding| {
        runtime_readers_for_binding(ctx.read_index, binding)
            .into_iter()
            .all(|reader| reader == *binding || cluster_bindings.contains(&reader))
    }) {
        return None;
    }
    if cluster_bindings.iter().any(|binding| {
        ctx.consumers_by_binding
            .get(&(ctx.source_file_id, binding.clone()))
            .is_some_and(|consumers| {
                consumers
                    .iter()
                    .any(|consumer| *consumer != consumer_module)
            })
    }) {
        return None;
    }

    let mut snippets = cluster_bindings
        .iter()
        .filter_map(|binding| {
            let snippet = ctx.prelude.snippets.get(binding)?;
            Some(RuntimeSingletonInlineSnippetSource {
                binding: binding.clone(),
                byte_start: snippet.byte_start,
                source: snippet.source.clone(),
            })
        })
        .collect::<Vec<_>>();
    snippets.sort_by_key(|snippet| (snippet.byte_start, snippet.binding.clone()));
    let byte_start = snippets
        .iter()
        .map(|snippet| snippet.byte_start)
        .min()
        .unwrap_or_default();

    Some(RuntimeSingletonInlineSnippet {
        consumer_module,
        byte_start,
        snippets,
        direct_prelude_imports: direct_imports,
        source_imports,
    })
}

pub(crate) fn source_exported_bindings_by_module(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
) -> BTreeMap<ModuleId, BTreeSet<BindingName>> {
    program
        .model()
        .modules()
        .iter()
        .map(|module| {
            let mut exports = program
                .model()
                .graph()
                .import_export()
                .exports_for(module.id)
                .into_iter()
                .collect::<BTreeSet<_>>();
            if let Some(wiring_exports) = source_module_wiring.exports_by_module.get(&module.id) {
                exports.extend(wiring_exports.iter().cloned());
            }
            (module.id, exports)
        })
        .collect()
}

pub(crate) fn module_exported_bindings(
    program: &EnrichedProgram,
    module_id: ModuleId,
    wired_exports: Option<&BTreeSet<BindingName>>,
    source: &str,
) -> BTreeSet<BindingName> {
    let mut exports = program
        .model()
        .graph()
        .import_export()
        .exports_for(module_id)
        .into_iter()
        .collect::<BTreeSet<_>>();
    if let Some(wired_exports) = wired_exports {
        exports.extend(wired_exports.iter().cloned());
    }
    if let Some((_stripped, named_exports)) = strip_top_level_named_exports(source) {
        exports.extend(named_exports);
    }
    exports
}

pub(crate) fn is_runtime_singleton_inline_excluded_binding(binding: &BindingName) -> bool {
    let name = binding.as_str();
    name == "lazyModule"
        || name == "lazyValue"
        || name.starts_with("__reverts_set_")
        || is_planner_synthetic_binding(name)
}

pub(crate) fn is_runtime_singleton_inline_snippet(binding: &BindingName, source: &str) -> bool {
    is_migratable_reader_function_snippet(binding, source)
        || classify_runtime_singleton_var_declaration(source, binding.as_str()).is_some()
}

pub(crate) fn classify_runtime_singleton_var_declaration<'a>(
    snippet: &'a str,
    binding: &str,
) -> Option<Option<&'a str>> {
    let trimmed = snippet.trim();
    for keyword in ["var", "let", "const"] {
        if let Some(rest) = trimmed.strip_prefix(keyword)
            && rest.starts_with(|c: char| c.is_ascii_whitespace())
        {
            let rest = rest.trim_start();
            let Some(rest) = rest.strip_suffix(';') else {
                continue;
            };
            let rest = rest.trim();
            if rest == binding && keyword != "const" {
                return Some(None);
            }
            let mut splitter = rest.splitn(2, '=');
            let lhs = splitter.next()?.trim();
            let rhs = splitter.next()?.trim();
            if lhs == binding && is_pure_initializer_expression(rhs) {
                return Some(Some(rhs));
            }
        }
    }
    None
}

pub(crate) fn runtime_singleton_inline_consumer_has_name_conflict(
    program: &EnrichedProgram,
    lowered_runtime_sources: &BTreeMap<ModuleId, LoweredRuntimeModuleSource>,
    consumer_module: ModuleId,
    binding: &BindingName,
) -> bool {
    program
        .model()
        .graph()
        .ast_imports_for(consumer_module)
        .contains(binding)
        || program
            .model()
            .graph()
            .ast_definitions_for(consumer_module)
            .contains(binding)
        || lowered_runtime_sources
            .get(&consumer_module)
            .is_some_and(|source| source.local_definitions.contains(binding))
}

pub(crate) fn partition_runtime_singleton_inline_bindings(
    plan: &RuntimeSingletonInlinePlan,
    current_module: ModuleId,
    source_file_id: u32,
    bindings: &BTreeSet<BindingName>,
) -> (BTreeSet<BindingName>, BTreeSet<BindingName>) {
    let mut inline_bindings = BTreeSet::<BindingName>::new();
    let mut runtime_bindings = BTreeSet::<BindingName>::new();
    for binding in bindings {
        if plan
            .snippets_by_binding
            .get(&(source_file_id, binding.clone()))
            .is_some_and(|snippet| snippet.consumer_module == current_module)
        {
            inline_bindings.insert(binding.clone());
        } else {
            runtime_bindings.insert(binding.clone());
        }
    }
    (inline_bindings, runtime_bindings)
}

pub(crate) struct RuntimeSingletonInlineEmitContext<'a> {
    pub(crate) program: &'a EnrichedProgram,
    pub(crate) module_id: ModuleId,
    pub(crate) module_path: &'a str,
}

pub(crate) fn emit_runtime_singleton_inline_helpers(
    ctx: RuntimeSingletonInlineEmitContext<'_>,
    plan: &RuntimeSingletonInlinePlan,
    file: &mut PlannedFile,
    planned_bindings: &mut BTreeSet<BindingName>,
    emitted_inline_runtime_helpers: &mut BTreeSet<(u32, BindingName)>,
    source_file_id: u32,
    bindings: &BTreeSet<BindingName>,
) {
    let mut snippets = bindings
        .iter()
        .filter_map(|binding| {
            let key = (source_file_id, binding.clone());
            let snippet = plan.snippets_by_binding.get(&key)?;
            Some((key, snippet))
        })
        .collect::<Vec<_>>();
    snippets.sort_by_key(|((_, binding), snippet)| (snippet.byte_start, binding.clone()));
    for ((source_file_id, _binding), snippet) in snippets {
        if !snippet.snippets.iter().any(|part| {
            !planned_bindings.contains(&part.binding)
                && !emitted_inline_runtime_helpers.contains(&(source_file_id, part.binding.clone()))
        }) {
            continue;
        }
        emit_direct_owner_imports(
            ctx.program,
            ctx.module_id,
            ctx.module_path,
            file,
            planned_bindings,
            &snippet.source_imports,
        );
        emit_direct_prelude_imports(file, planned_bindings, &snippet.direct_prelude_imports);
        for part in &snippet.snippets {
            if planned_bindings.contains(&part.binding)
                || !emitted_inline_runtime_helpers.insert((source_file_id, part.binding.clone()))
            {
                continue;
            }
            file.push_source(part.source.clone());
            planned_bindings.insert(part.binding.clone());
            file.add_binding(PlannedBinding::new(
                part.binding.clone(),
                part.binding.clone(),
                BindingShape::Unknown,
                true,
            ));
        }
    }
}
