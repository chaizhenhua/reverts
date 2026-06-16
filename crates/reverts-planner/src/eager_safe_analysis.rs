//! Cross-module eager-safety analysis.
//!
//! Modules whose top-level evaluation has no observable side effect on
//! other modules — singleton SCCs in the import graph — can have their
//! `lazyValue(() => …)` / `lazyModule((exports, module) => …)` thunks
//! "eagerified" into direct values without changing semantics. The
//! cross-module flavour of that analysis lives here.
//!
//! `compute_eager_safe_analysis` produces an `EagerSafeAnalysis` with
//! two indexes:
//!
//! - `eager_safe_exports_by_module` — per producer, the bindings whose
//!   thunks the analysis cleared. Consumer call sites referencing these
//!   get rewritten from `X()` to `X`.
//! - `safe_call_targets_by_module` — per consumer, the names of imports
//!   whose producer-side eagerification means their bodies can also be
//!   extracted (the `EagerWithDeps` path).
//!
//! Construction pipeline:
//!
//! 1. `compute_consumer_call_forms` — every import a consumer makes is
//!    either always `X()` (zero-arg call) or it's not. Only the former
//!    can be safely rewritten.
//! 2. `compute_consumer_usage_scopes` — classify each import's usage
//!    site as top-level vs. inside a callable body, fed into the SCC
//!    detector so module-evaluation cycles are detected accurately.
//! 3. `singleton_scc_modules` — Tarjan SCC over the top-level-only
//!    dependency graph; only singletons (no self-loop) are eligible
//!    producers.
//! 4. `compute_thunk_wrapped_exports` — restrict candidates to bindings
//!    declared as `var X = HELPER((…) => …)` where HELPER is a lazy
//!    wrapper from the runtime prelude.
//! 5. `predict_delazifiable_exports` — per-body classifier + name-
//!    resolution + worklist fixpoint over zero-arg thunk-call deps.
//! 6. Final intersection: a candidate is eager-safe iff it passes the
//!    SCC test, is thunk-wrapped, its body is delazifiable, AND every
//!    consumer references it only as `X()`.
//!
//! `consumer_eagerified_imports` and `rewrite_eagerified_call_sites`
//! are the per-module rewrite step the `lowered_runtime_sources` pass
//! invokes after this analysis runs.

use std::collections::{BTreeMap, BTreeSet};

use reverts_graph::RuntimePreludeBindingKind;
use reverts_ir::{BindingName, ModuleId};
use reverts_js::{
    ImportUsageScope, ParseGoal, classify_import_usage_scope,
    is_ascii_identifier_continue as is_identifier_continue,
    is_ascii_identifier_start as is_identifier_start, verify_only_immediate_call_references,
};
use reverts_model::EnrichedProgram;

use crate::byte_lexer::{expect_arrow, find_matching_brace, skip_non_code_at, skip_ws};
use crate::identifiers::{keyword_at, parse_identifier};
use crate::{
    SourceModuleWiring, identifier_occurrence_is_value_reference, previous_non_ws,
    runtime_helper_kinds, runtime_helper_kinds_for_source,
};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct EagerSafeAnalysis {
    /// For each target module, the subset of its exported bindings that
    /// pass the cross-module eager-safety check.
    pub(crate) eager_safe_exports_by_module: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    /// For each consumer module, the set of import names that resolve
    /// to bindings the fixpoint marked eager-safe. The lowering pass
    /// passes this set to the body extractor so `EagerWithDeps` bodies
    /// (zero-arg calls to imported thunks) can extract their value
    /// when every dep would itself have been eagerified.
    pub(crate) safe_call_targets_by_module: BTreeMap<ModuleId, BTreeSet<String>>,
}

const CROSS_MODULE_EAGER_SAFE_ANALYSIS_MODULE_LIMIT: usize = 1024;

pub(crate) fn should_compute_cross_module_eager_safe_analysis(program: &EnrichedProgram) -> bool {
    program.model().modules().len() <= CROSS_MODULE_EAGER_SAFE_ANALYSIS_MODULE_LIMIT
        || std::env::var_os("REVERTS_CROSS_MODULE_EAGER_SAFE").is_some()
}

pub(crate) fn compute_eager_safe_analysis(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
) -> EagerSafeAnalysis {
    let usage_scopes = compute_consumer_usage_scopes(program, source_module_wiring);
    let call_forms = compute_consumer_call_forms(program, source_module_wiring);
    let singleton_modules = singleton_scc_modules(program, source_module_wiring, &usage_scopes);
    // Only bindings declared as `var X = <lazy_helper>(...)` in their
    // exporting module are eagerification candidates — a regular function
    // or value export is already "direct" and any consumer `X()` call is
    // already calling it correctly (not invoking a thunk).
    let thunk_wrapped_exports = compute_thunk_wrapped_exports(program);
    // Additional gate: only bindings whose BODY actually passes the
    // delazify-extraction check qualify for eagerification. The
    // prediction also reports per-consumer `safe_call_targets` so the
    // lowering extractor can accept `EagerWithDeps` bodies whose deps
    // would themselves eagerify.
    let prediction = predict_delazifiable_exports(program, source_module_wiring);
    let mut eager_safe_exports_by_module = BTreeMap::<ModuleId, BTreeSet<BindingName>>::new();
    for (target_id, exported_bindings) in &source_module_wiring.exports_by_module {
        if !singleton_modules.contains(target_id) {
            continue;
        }
        let Some(thunk_wrapped) = thunk_wrapped_exports.get(target_id) else {
            continue;
        };
        let Some(delazifiable) = prediction.delazifiable_exports_by_module.get(target_id) else {
            continue;
        };
        let mut safe = BTreeSet::<BindingName>::new();
        'each_export: for binding in exported_bindings {
            if !thunk_wrapped.contains(binding) {
                continue;
            }
            if !delazifiable.contains(binding) {
                continue;
            }
            // For every consumer that imports this binding, the consumer
            // must reference it exclusively in the zero-arg `X()` call
            // shape — the only pattern the cross-module rewriter knows
            // how to mechanically convert to a bare `X` once M emits the
            // direct value. Body-purity is checked separately inside
            // `lower_runtime_helpers` (we never delazify an impure RHS),
            // so call-form is the only additional gate Phase 8 introduces
            // on top of SCC clearance.
            for (consumer_id, imports_by_target) in &source_module_wiring.imports_by_module {
                let Some(imported_from_target) = imports_by_target.get(target_id) else {
                    continue;
                };
                if !imported_from_target.contains(binding) {
                    continue;
                }
                let Some(consumer_call_forms) = call_forms.get(consumer_id) else {
                    continue 'each_export;
                };
                if !consumer_call_forms
                    .get(binding.as_str())
                    .copied()
                    .unwrap_or(false)
                {
                    continue 'each_export;
                }
            }
            safe.insert(binding.clone());
        }
        if !safe.is_empty() {
            eager_safe_exports_by_module.insert(*target_id, safe);
        }
    }
    EagerSafeAnalysis {
        eager_safe_exports_by_module,
        safe_call_targets_by_module: prediction.safe_call_targets_by_module,
    }
}

/// For each consumer module, classify every imported binding by whether
/// its uses in that consumer are all the zero-arg call shape `X()`. A
/// binding can be eagerified only if every consumer that references it
/// passes this check — otherwise cross-module rewriting can't
/// mechanically convert `X()` (returning the thunk's value) into `X`
/// (the value directly).
fn compute_consumer_call_forms(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
) -> BTreeMap<ModuleId, BTreeMap<String, bool>> {
    let mut out = BTreeMap::new();
    for (consumer_id, imports_by_target) in &source_module_wiring.imports_by_module {
        let Some(source_slice) = program.model().input().module_source_slice(*consumer_id) else {
            continue;
        };
        let binding_names: BTreeSet<String> = imports_by_target
            .values()
            .flatten()
            .map(|binding| binding.as_str().to_string())
            .collect();
        if binding_names.is_empty() {
            continue;
        }
        let call_forms = verify_only_immediate_call_references(
            source_slice.source,
            &binding_names,
            Some(std::path::Path::new(source_slice.source_file_path)),
            ParseGoal::TypeScript,
        );
        out.insert(*consumer_id, call_forms);
    }
    out
}

/// Parse every consumer module's source slice once and classify the usage
/// scope of every binding it imports. Returns a map keyed by consumer
/// module id, with values keyed by the imported binding's identifier.
fn compute_consumer_usage_scopes(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
) -> BTreeMap<ModuleId, BTreeMap<String, ImportUsageScope>> {
    let mut out = BTreeMap::new();
    for (consumer_id, imports_by_target) in &source_module_wiring.imports_by_module {
        let Some(source_slice) = program.model().input().module_source_slice(*consumer_id) else {
            continue;
        };
        let binding_names: BTreeSet<String> = imports_by_target
            .values()
            .flatten()
            .map(|binding| binding.as_str().to_string())
            .collect();
        if binding_names.is_empty() {
            continue;
        }
        let scopes = classify_import_usage_scope(
            source_slice.source,
            &binding_names,
            Some(std::path::Path::new(source_slice.source_file_path)),
            ParseGoal::TypeScript,
        );
        out.insert(*consumer_id, scopes);
    }
    out
}

/// Compute the set of modules that are singleton SCCs in the
/// top-level-only module dependency graph: edges include only the
/// references a consumer makes at its own top-level (not the ones nested
/// inside fn/arrow/method bodies). Modules in singleton SCCs are not part
/// of any module-evaluation cycle and can therefore be eagerified without
/// reordering observable side effects.
fn singleton_scc_modules(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
    usage_scopes: &BTreeMap<ModuleId, BTreeMap<String, ImportUsageScope>>,
) -> BTreeSet<ModuleId> {
    use petgraph::algo::tarjan_scc;
    use petgraph::graph::{DiGraph, NodeIndex};
    let mut graph: DiGraph<ModuleId, ()> = DiGraph::new();
    let mut node_by_module = BTreeMap::<ModuleId, NodeIndex>::new();
    for module in program.model().modules() {
        let idx = graph.add_node(module.id);
        node_by_module.insert(module.id, idx);
    }
    for (consumer_id, imports_by_target) in &source_module_wiring.imports_by_module {
        let consumer_scopes = usage_scopes.get(consumer_id);
        let Some(&consumer_idx) = node_by_module.get(consumer_id) else {
            continue;
        };
        for (target_id, bindings) in imports_by_target {
            let Some(&target_idx) = node_by_module.get(target_id) else {
                continue;
            };
            let has_top_level_use = bindings.iter().any(|binding| {
                consumer_scopes
                    .and_then(|scopes| scopes.get(binding.as_str()))
                    .copied()
                    == Some(ImportUsageScope::TopLevel)
            });
            if has_top_level_use {
                graph.add_edge(consumer_idx, target_idx, ());
            }
        }
    }
    let sccs = tarjan_scc(&graph);
    let mut singleton = BTreeSet::new();
    for scc in &sccs {
        if scc.len() != 1 {
            continue;
        }
        let node = scc[0];
        // Reject if there's a self-loop — that's a (trivial) cycle.
        if graph.find_edge(node, node).is_some() {
            continue;
        }
        singleton.insert(graph[node]);
    }
    singleton
}

/// For each module, find every top-level binding declared as
/// `var X = HELPER(...)` where HELPER is a lazy-wrapping helper from
/// the bundle's runtime prelude (CommonJS wrapper or lazy initializer).
/// These are the only exports the cross-module eager-safety analysis
/// is allowed to consider as candidates — non-thunk exports (a literal,
/// a function declaration, a class) are already direct and their
/// consumer call sites are not zero-arg thunk invocations.
fn compute_thunk_wrapped_exports(
    program: &EnrichedProgram,
) -> BTreeMap<ModuleId, BTreeSet<BindingName>> {
    let mut out = BTreeMap::new();
    for module in program.model().modules() {
        let Some(source) = program.model().input().module_source_slice(module.id) else {
            continue;
        };
        let runtime_imports = program.model().graph().runtime_imports_for(module.id);
        let mut helper_kinds = runtime_helper_kinds(program.model().graph(), &runtime_imports);
        helper_kinds.extend(runtime_helper_kinds_for_source(
            program.model().graph(),
            source.source_file_id,
            source.source,
        ));
        let lazy_helpers: BTreeSet<&str> = helper_kinds
            .iter()
            .filter(|(_, kind)| {
                matches!(
                    kind,
                    RuntimePreludeBindingKind::CommonJsWrapper
                        | RuntimePreludeBindingKind::LazyInitializer
                )
            })
            .map(|(binding, _)| binding.as_str())
            .collect();
        if lazy_helpers.is_empty() {
            continue;
        }
        let thunk_bindings = scan_thunk_wrapped_bindings(source.source, &lazy_helpers);
        if !thunk_bindings.is_empty() {
            out.insert(module.id, thunk_bindings);
        }
    }
    out
}

/// Scan `source` for every `var/let/const X = HELPER(...)` declaration
/// where HELPER is one of `lazy_helpers`, and return the binding name X.
/// The scan is byte-level (skipping quoted strings, comments, regex
/// literals, templates) — same conventions as the existing
/// declaration scanners in this module.
fn scan_thunk_wrapped_bindings(
    source: &str,
    lazy_helpers: &BTreeSet<&str>,
) -> BTreeSet<BindingName> {
    let mut out = BTreeSet::new();
    let bytes = source.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        let Some(keyword) = ["var", "let", "const"]
            .into_iter()
            .find(|kw| keyword_at(source, cursor, kw))
        else {
            cursor += 1;
            continue;
        };
        let mut c = cursor + keyword.len();
        c = skip_ws(bytes, c);
        let Some((binding_name, after_binding)) = parse_identifier(source, c) else {
            cursor += 1;
            continue;
        };
        c = skip_ws(bytes, after_binding);
        if bytes.get(c) != Some(&b'=') {
            cursor = after_binding;
            continue;
        }
        c = skip_ws(bytes, c + 1);
        let Some((helper_name, after_helper)) = parse_identifier(source, c) else {
            cursor = after_binding;
            continue;
        };
        c = skip_ws(bytes, after_helper);
        if bytes.get(c) != Some(&b'(') {
            cursor = after_helper;
            continue;
        }
        if lazy_helpers.contains(helper_name) {
            out.insert(BindingName::new(binding_name));
        }
        cursor = after_helper;
    }
    out
}

/// For each module, predict the subset of its thunk-wrapped exports
/// whose body would actually delazify — including transitively, via
/// the inter-procedural fixpoint over zero-arg thunk-call dependencies.
///
/// Pipeline:
///   1. Enumerate every `var X = HELPER((params) => { BODY })` across
///      every module and classify each BODY via the AST-level
///      `classify_lazy_module_body`. Outcomes:
///         * `Eager` — body has a value with no calls; immediately safe.
///         * `EagerWithDeps` — body composes a value but invokes one or
///           more zero-arg bindings; safe iff those bindings are
///           themselves eager-safe.
///         * `Impure` — body has unrecognized side effects; never safe.
///   2. Build a per-module name-resolution table from
///      `source_module_wiring.imports_by_module` so each `call_deps`
///      identifier in step 1 can be mapped to a `(target_module, binding)`
///      pair. Local thunks (declared in the consumer module itself)
///      resolve to that same module's `(M, name)`.
///   3. Fixpoint: seed with all `Eager` bindings, then loop: add
///      `EagerWithDeps{deps}` bindings where every resolved dep already
///      lives in the safe set, until stable. Mutual recursion (cycles
///      in the dep graph) keep both sides unsafe — neither can be added
///      without the other already in the set.
fn predict_delazifiable_exports(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
) -> EagerSafetyPrediction {
    let classifications = enumerate_and_classify_lazy_bindings(program);
    let resolution = build_dep_resolution_map(program, source_module_wiring, &classifications);
    let safe_keys = compute_eager_safe_fixpoint(&classifications, &resolution);
    let mut delazifiable_exports_by_module: BTreeMap<ModuleId, BTreeSet<BindingName>> =
        BTreeMap::new();
    for (module_id, binding) in &safe_keys {
        delazifiable_exports_by_module
            .entry(*module_id)
            .or_default()
            .insert(binding.clone());
    }
    let mut safe_call_targets_by_module: BTreeMap<ModuleId, BTreeSet<String>> = BTreeMap::new();
    for module in program.model().modules() {
        let names = build_eager_safe_call_targets_for_module(module.id, &safe_keys, &resolution);
        if !names.is_empty() {
            safe_call_targets_by_module.insert(module.id, names);
        }
    }
    EagerSafetyPrediction {
        delazifiable_exports_by_module,
        safe_call_targets_by_module,
    }
}

/// Bundle of outputs from the inter-procedural fixpoint. Both fields
/// are needed by the lowering: the exports set tells the cross-module
/// rewriter which consumer `X()` calls to strip, and the call-targets
/// set tells the body extractor which thunk-call deps to treat as
/// "already handled by the producer's eagerification" — i.e., drop
/// from the consumer's prologue.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct EagerSafetyPrediction {
    delazifiable_exports_by_module: BTreeMap<ModuleId, BTreeSet<BindingName>>,
    pub(crate) safe_call_targets_by_module: BTreeMap<ModuleId, BTreeSet<String>>,
}

/// Walk every module, find each `var X = HELPER((params) => { BODY })`
/// declaration where HELPER is a lazy wrapper, and classify the BODY
/// via the AST classifier in `reverts_js`. Returns the classification
/// keyed by `(module_id, binding)`.
fn enumerate_and_classify_lazy_bindings(
    program: &EnrichedProgram,
) -> BTreeMap<(ModuleId, BindingName), reverts_js::LazyBodyClassification> {
    let mut classifications = BTreeMap::new();
    for module in program.model().modules() {
        let Some(source) = program.model().input().module_source_slice(module.id) else {
            continue;
        };
        let runtime_imports = program.model().graph().runtime_imports_for(module.id);
        let mut helper_kinds = runtime_helper_kinds(program.model().graph(), &runtime_imports);
        helper_kinds.extend(runtime_helper_kinds_for_source(
            program.model().graph(),
            source.source_file_id,
            source.source,
        ));
        let module_helpers: BTreeSet<&str> = helper_kinds
            .iter()
            .filter(|(_, kind)| matches!(kind, RuntimePreludeBindingKind::CommonJsWrapper))
            .map(|(binding, _)| binding.as_str())
            .collect();
        let value_helpers: BTreeSet<&str> = helper_kinds
            .iter()
            .filter(|(_, kind)| matches!(kind, RuntimePreludeBindingKind::LazyInitializer))
            .map(|(binding, _)| binding.as_str())
            .collect();
        if module_helpers.is_empty() && value_helpers.is_empty() {
            continue;
        }
        scan_and_classify_lazy_bindings_in_module(
            source.source,
            &module_helpers,
            &value_helpers,
            module.id,
            &mut classifications,
        );
    }
    classifications
}

/// Companion to `enumerate_and_classify_lazy_bindings`: for one module,
/// scan its source for lazy declarations and stash the body classification.
fn scan_and_classify_lazy_bindings_in_module(
    source: &str,
    commonjs_helpers: &BTreeSet<&str>,
    lazy_value_helpers: &BTreeSet<&str>,
    module_id: ModuleId,
    out: &mut BTreeMap<(ModuleId, BindingName), reverts_js::LazyBodyClassification>,
) {
    let bytes = source.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        let Some(keyword) = ["var", "let", "const"]
            .into_iter()
            .find(|kw| keyword_at(source, cursor, kw))
        else {
            cursor += 1;
            continue;
        };
        let mut c = cursor + keyword.len();
        c = skip_ws(bytes, c);
        let Some((binding_name, after_binding)) = parse_identifier(source, c) else {
            cursor += 1;
            continue;
        };
        c = skip_ws(bytes, after_binding);
        if bytes.get(c) != Some(&b'=') {
            cursor = after_binding;
            continue;
        }
        c = skip_ws(bytes, c + 1);
        let Some((helper_name, after_helper)) = parse_identifier(source, c) else {
            cursor = after_binding;
            continue;
        };
        c = skip_ws(bytes, after_helper);
        if bytes.get(c) != Some(&b'(') {
            cursor = after_helper;
            continue;
        }
        let is_commonjs = commonjs_helpers.contains(helper_name);
        let is_lazy_value = lazy_value_helpers.contains(helper_name);
        if !is_commonjs && !is_lazy_value {
            cursor = after_helper;
            continue;
        }
        c = skip_ws(bytes, c + 1);
        let (exports_param, module_param, body_start, body_end) =
            match parse_lazy_factory_signature(source, c, is_commonjs) {
                Some(parts) => parts,
                None => {
                    cursor = after_helper;
                    continue;
                }
            };
        let body = &source[body_start..body_end];
        let classification = reverts_js::classify_lazy_module_body(
            body,
            exports_param,
            module_param,
            None,
            ParseGoal::TypeScript,
        );
        if !matches!(classification, reverts_js::LazyBodyClassification::Impure) {
            out.insert((module_id, BindingName::new(binding_name)), classification);
        }
        cursor = body_end;
    }
}

/// For each consumer module, build a `name → (target_module, binding)`
/// table so dep names appearing in lazy bodies can be resolved.
/// Combines two sources:
///   * Cross-module imports recorded in `source_module_wiring` — every
///     imported binding is mapped to its target module.
///   * Local thunk bindings — if a body calls `localX()` and the same
///     module has `var localX = lazyValue(...)`, that resolves to
///     `(self, localX)`.
fn build_dep_resolution_map(
    program: &EnrichedProgram,
    source_module_wiring: &SourceModuleWiring,
    _classifications: &BTreeMap<(ModuleId, BindingName), reverts_js::LazyBodyClassification>,
) -> BTreeMap<ModuleId, BTreeMap<String, (ModuleId, BindingName)>> {
    // Only cross-module imports are resolved as eager-safe call deps —
    // local thunks within the same module would need source-order
    // verification (a thunk declared AFTER its consumer can't be
    // referenced before its declaration runs) which we don't yet
    // perform. Restricting to imports is the conservative direction:
    // local-only chains stay lazy; cross-module chains get the
    // fixpoint benefit.
    let mut out: BTreeMap<ModuleId, BTreeMap<String, (ModuleId, BindingName)>> = BTreeMap::new();
    for module in program.model().modules() {
        let entry = out.entry(module.id).or_default();
        if let Some(targets) = source_module_wiring.imports_by_module.get(&module.id) {
            for (target_id, bindings) in targets {
                for binding in bindings {
                    entry.insert(binding.as_str().to_string(), (*target_id, binding.clone()));
                }
            }
        }
    }
    out
}

/// Standard worklist fixpoint over the dep graph. Seeds with `Eager`
/// bindings (no deps), then iteratively adds `EagerWithDeps` bindings
/// whose dependencies all resolve to entries already in the safe set.
/// O(N × max-deps) per round; converges in a small number of rounds
/// in practice because most chains are shallow.
///
/// Note: the fixpoint result is used both to gate cross-module
/// rewriting (consumer `X()` → `X`) and to gate value extraction in
/// the lowering pass — the matching extractor
/// `extract_lazy_module_eager_value_with_safe_deps` accepts
/// `EagerWithDeps` bindings whose every dep is in the safe set, so
/// producer and consumer agree on whether the binding emits as a
/// direct value.
fn compute_eager_safe_fixpoint(
    classifications: &BTreeMap<(ModuleId, BindingName), reverts_js::LazyBodyClassification>,
    resolution: &BTreeMap<ModuleId, BTreeMap<String, (ModuleId, BindingName)>>,
) -> BTreeSet<(ModuleId, BindingName)> {
    let mut safe: BTreeSet<(ModuleId, BindingName)> = BTreeSet::new();
    for (key, classification) in classifications {
        if matches!(
            classification,
            reverts_js::LazyBodyClassification::Eager { .. }
        ) {
            safe.insert(key.clone());
        }
    }
    loop {
        let mut added = false;
        for (key, classification) in classifications {
            if safe.contains(key) {
                continue;
            }
            let reverts_js::LazyBodyClassification::EagerWithDeps { call_deps, .. } =
                classification
            else {
                continue;
            };
            let module_resolution = resolution.get(&key.0);
            let all_deps_safe = call_deps.iter().all(|name| {
                module_resolution
                    .and_then(|r| r.get(name))
                    .map(|resolved| safe.contains(resolved))
                    .unwrap_or(false)
            });
            if all_deps_safe {
                safe.insert(key.clone());
                added = true;
            }
        }
        if !added {
            break;
        }
    }
    safe
}

/// For a given module M, project the global fixpoint result onto the
/// names visible in M's scope. Returns the set of names X such that
/// `X()` (zero-arg call) in M's body resolves to a binding that the
/// fixpoint marked eager-safe — feeds into
/// `extract_lazy_module_eager_value_with_safe_deps` when lowering M.
fn build_eager_safe_call_targets_for_module(
    module_id: ModuleId,
    safe_keys: &BTreeSet<(ModuleId, BindingName)>,
    resolution: &BTreeMap<ModuleId, BTreeMap<String, (ModuleId, BindingName)>>,
) -> BTreeSet<String> {
    let Some(module_resolution) = resolution.get(&module_id) else {
        return BTreeSet::new();
    };
    module_resolution
        .iter()
        .filter_map(|(name, resolved)| {
            if safe_keys.contains(resolved) {
                Some(name.clone())
            } else {
                None
            }
        })
        .collect()
}

/// Parse the `((exports[, module]) => { body })` signature inside a
/// lazy-helper call, starting from the byte right after the helper's
/// opening `(`. Returns the parameter names and the byte range of the
/// arrow body (exclusive of the surrounding braces).
fn parse_lazy_factory_signature(
    source: &str,
    open_paren_after_helper: usize,
    expect_two_params: bool,
) -> Option<(&str, Option<&str>, usize, usize)> {
    let bytes = source.as_bytes();
    let mut c = open_paren_after_helper;
    if bytes.get(c) != Some(&b'(') {
        return None;
    }
    c = skip_ws(bytes, c + 1);
    let (exports_param, after_exports) = if expect_two_params {
        let (name, after) = parse_identifier(source, c)?;
        c = skip_ws(bytes, after);
        (name, after)
    } else {
        // lazyValue arrow: `() => { ... }`. Allow empty parameter list.
        if bytes.get(c) == Some(&b')') {
            ("", c)
        } else {
            return None;
        }
    };
    let _ = after_exports;
    let module_param = if expect_two_params && bytes.get(c) == Some(&b',') {
        c = skip_ws(bytes, c + 1);
        let (name, after) = parse_identifier(source, c)?;
        c = skip_ws(bytes, after);
        Some(name)
    } else {
        None
    };
    if bytes.get(c) != Some(&b')') {
        return None;
    }
    c = skip_ws(bytes, c + 1);
    let arrow_end = expect_arrow(bytes, c)?;
    c = skip_ws(bytes, arrow_end);
    if bytes.get(c) != Some(&b'{') {
        return None;
    }
    let body_end = find_matching_brace(source, c)?;
    Some((exports_param, module_param, c + 1, body_end))
}

/// For a given consumer module, gather every imported binding the
/// cross-module eager-safety analysis cleared. These are the call sites
/// `lowered_runtime_sources` must rewrite from `X()` to `X` so the
/// consumer can see the import's now-direct value rather than a missing
/// thunk function.
pub(crate) fn consumer_eagerified_imports(
    consumer_id: ModuleId,
    source_module_wiring: &SourceModuleWiring,
    eager_safe_analysis: &EagerSafeAnalysis,
) -> BTreeSet<BindingName> {
    let mut out = BTreeSet::new();
    let Some(imports_by_target) = source_module_wiring.imports_by_module.get(&consumer_id) else {
        return out;
    };
    for (target_id, bindings) in imports_by_target {
        let Some(eager_safe) = eager_safe_analysis
            .eager_safe_exports_by_module
            .get(target_id)
        else {
            continue;
        };
        for binding in bindings.intersection(eager_safe) {
            out.insert(binding.clone());
        }
    }
    out
}

/// Mechanically rewrite every `X()` zero-arg call site (where `X` is one
/// of `eagerified_imports`) to a bare `X`. The cross-module eager-safe
/// analysis has already verified upstream that every reference to each
/// binding is in this exact shape, so the rewrite cannot lose precision.
/// Property-access uses (`obj.X`) and non-value occurrences (import
/// specifiers, export specifiers) are correctly skipped via the same
/// classifier the local delazify pass uses.
pub(crate) fn rewrite_eagerified_call_sites(
    source: &str,
    eagerified_imports: &BTreeSet<BindingName>,
) -> String {
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let bytes = source.as_bytes();
    for binding in eagerified_imports {
        let target = binding.as_str();
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
            if &source[start..cursor] != target {
                continue;
            }
            // Skip property access (`obj.X` / `obj#X`).
            if let Some(prev) = previous_non_ws(bytes, start)
                && matches!(bytes[prev], b'.' | b'#')
            {
                continue;
            }
            // Skip non-value occurrences (import specifier, etc.).
            if !identifier_occurrence_is_value_reference(source, start, cursor) {
                continue;
            }
            // Require zero-arg call shape `X()` — verified by the
            // eager-safe analysis to be the only shape present.
            let after = skip_ws(bytes, cursor);
            if bytes.get(after) != Some(&b'(') {
                continue;
            }
            let inner = skip_ws(bytes, after + 1);
            if bytes.get(inner) != Some(&b')') {
                continue;
            }
            edits.push((start, inner + 1, target.to_string()));
            cursor = inner + 1;
        }
    }
    if edits.is_empty() {
        return source.to_string();
    }
    edits.sort_by_key(|(start, _, _)| *start);
    let mut output = String::with_capacity(source.len());
    let mut cursor = 0;
    for (start, end, replacement) in &edits {
        debug_assert!(*start >= cursor, "cross-module rewrites must not overlap");
        output.push_str(&source[cursor..*start]);
        output.push_str(replacement);
        cursor = *end;
    }
    output.push_str(&source[cursor..]);
    output
}
