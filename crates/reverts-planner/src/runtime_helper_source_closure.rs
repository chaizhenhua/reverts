//! Runtime helper source closure pass extracted from `lib.rs`.
//!
//! Walks the prelude snippet graph starting from a root binding set,
//! emitting the fixed-point set of helper sources (including the
//! namespace export statements they require) plus the entrypoint
//! side-effects.

use std::collections::{BTreeMap, BTreeSet};

use reverts_graph::{RuntimeEntrypoint, RuntimeNamespaceExport, RuntimePrelude};
use reverts_ir::BindingName;
use reverts_model::EnrichedProgram;

use crate::byte_lexer::{find_matching_paren, skip_ws};
use crate::identifier_facts::compact_js_source;
use crate::identifiers::parse_identifier;
use crate::statements::runtime_namespace_export_statement;
use crate::{
    RuntimeFoldedSourceChunk, identifiers_in_source, runtime_import_identifiers_in_source,
    runtime_namespace_exports_for_helpers, top_level_definitions_in_source,
};

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
        // Re-emit each binding-augmenting statement at its ORIGINAL byte offset
        // (a member assignment may eagerly read a binding declared after the
        // augmented one; emitting in source order avoids a TDZ access).
        for augmentation in &snippet.augmentations {
            snippets
                .entry(augmentation.byte_start)
                .or_insert_with(|| augmentation.source.clone());
        }
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
