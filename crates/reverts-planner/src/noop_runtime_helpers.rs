//! Runtime no-op helper localization and pruning.
//!
//! Source surgery note: this pass removes or rewrites exact top-level helper
//! statements after byte-lexer scans prove the referenced helpers are no-ops.
//! AST codegen would not preserve the raw runtime snippets/trivia this pass is
//! intentionally compacting, so edits are range-based and applied through
//! `source_surgery::apply_text_edits`.

use std::collections::BTreeSet;

use reverts_graph::RuntimePrelude;
use reverts_ir::BindingName;
use reverts_js::{ParseGoal, collect_void_zero_expression_statements};

use crate::byte_lexer::{find_matching_paren, skip_ws};
use crate::{
    IdentifierReadUsage, apply_text_edits, contains_identifier_reference,
    expand_line_removal_edits, identifier_read_facts_in_source,
    identifier_read_rename_site_is_safe, implicit_global_writes_in_source,
    localize_lazy_value_source, lowered_lazy_initializer_statement_binding,
    previous_token_is_keyword, runtime_prelude_snippet_is_noop, top_level_definitions_in_source,
    top_level_statement_slices, top_level_statement_spans,
};

pub(crate) fn localizable_noop_runtime_helpers(
    prelude: &RuntimePrelude,
    source: &str,
    candidates: &BTreeSet<BindingName>,
    exported_bindings: &BTreeSet<BindingName>,
    uses_lazy_value: bool,
) -> BTreeSet<BindingName> {
    let localizable = candidates
        .iter()
        .filter(|binding| !exported_bindings.contains(*binding))
        .filter(|binding| {
            prelude.snippets.get(*binding).is_some_and(|snippet| {
                runtime_prelude_snippet_is_noop(binding.as_str(), snippet.source.as_str())
            })
        })
        .filter(|binding| source_reads_binding_only_as_erasable_noop_calls(source, binding))
        .cloned()
        .collect::<BTreeSet<_>>();
    if localizable.is_empty() {
        return localizable;
    }
    if uses_lazy_value && localize_lazy_value_source(source).is_none() {
        return localizable
            .into_iter()
            .filter(|binding| {
                !source_contains_top_level_lazy_value_call_referencing_binding(source, binding)
            })
            .collect();
    }
    localizable
}

pub(crate) fn source_contains_top_level_lazy_value_call_referencing_binding(
    source: &str,
    binding: &BindingName,
) -> bool {
    top_level_statement_slices(source)
        .into_iter()
        .filter(|statement| lowered_lazy_initializer_statement_binding(statement).is_some())
        .any(|statement| contains_identifier_reference(statement, binding.as_str()))
}

pub(crate) fn private_noop_runtime_helpers_in_source(
    source: &str,
    public_bindings: &BTreeSet<BindingName>,
) -> BTreeSet<BindingName> {
    noop_runtime_helpers_in_source(source)
        .into_iter()
        .filter(|binding| !public_bindings.contains(binding))
        .filter(|binding| source_reads_binding_only_as_erasable_noop_calls(source, binding))
        .collect()
}

pub(crate) fn noop_runtime_helpers_in_source(source: &str) -> BTreeSet<BindingName> {
    top_level_statement_slices(source)
        .into_iter()
        .flat_map(|statement| {
            top_level_definitions_in_source(statement)
                .into_iter()
                .filter(move |binding| runtime_prelude_snippet_is_noop(binding.as_str(), statement))
        })
        .collect()
}

pub(crate) fn strip_runtime_noop_declarations(
    source: &str,
    bindings: &BTreeSet<BindingName>,
) -> String {
    if bindings.is_empty() {
        return source.to_string();
    }
    let edits = top_level_statement_spans(source)
        .into_iter()
        .filter_map(|(start, end)| {
            let statement = &source[start..end];
            let matched = top_level_definitions_in_source(statement)
                .into_iter()
                .any(|binding| {
                    bindings.contains(&binding)
                        && runtime_prelude_snippet_is_noop(binding.as_str(), statement)
                });
            matched.then_some((start, end, String::new()))
        })
        .collect::<Vec<_>>();
    if edits.is_empty() {
        return source.to_string();
    }
    apply_text_edits(source, &expand_line_removal_edits(source, &edits))
}

pub(crate) fn drop_bare_void_zero_top_level_statements(source: &str) -> String {
    let edits = top_level_statement_spans(source)
        .into_iter()
        .filter_map(|(start, end)| {
            let statement = &source[start..end];
            matches!(statement.trim(), "void 0;" | "void 0").then_some((start, end, String::new()))
        })
        .collect::<Vec<_>>();
    if edits.is_empty() {
        source.to_string()
    } else {
        apply_text_edits(source, &expand_line_removal_edits(source, &edits))
    }
}

pub(crate) fn compact_bare_void_zero_expression_statements(source: &str) -> String {
    let Ok(statements) =
        collect_void_zero_expression_statements(source, None, ParseGoal::TypeScript)
    else {
        return source.to_string();
    };
    if statements.is_empty() {
        return source.to_string();
    }
    let edits = statements
        .into_iter()
        .map(|statement| {
            (
                statement.byte_start as usize,
                statement.byte_end as usize,
                ";".to_string(),
            )
        })
        .collect::<Vec<_>>();
    apply_text_edits(source, &edits)
}

pub(crate) fn source_reads_binding_only_as_erasable_noop_calls(
    source: &str,
    binding: &BindingName,
) -> bool {
    if implicit_global_writes_in_source(source).contains(binding) {
        return false;
    }
    let reads = identifier_read_facts_in_source(source)
        .into_iter()
        .filter(|fact| fact.name == binding.as_str())
        .collect::<Vec<_>>();
    !reads.is_empty()
        && reads
            .iter()
            .all(|fact| noop_call_replacement_span(source, fact).is_some())
}

pub(crate) fn rewrite_noop_runtime_helper_calls(
    source: &str,
    helpers: &BTreeSet<BindingName>,
) -> String {
    if helpers.is_empty() {
        return source.to_string();
    }
    let mut edits = identifier_read_facts_in_source(source)
        .into_iter()
        .filter(|fact| helpers.contains(&BindingName::new(fact.name.as_str())))
        .filter_map(|fact| {
            let (start, end) = noop_call_replacement_span(source, &fact)?;
            Some((start, end, "void 0".to_string()))
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

pub(crate) fn noop_call_replacement_span(
    source: &str,
    fact: &IdentifierReadUsage,
) -> Option<(usize, usize)> {
    if !fact.is_call_callee
        || !identifier_read_rename_site_is_safe(source, fact.byte_start, fact.byte_end)
    {
        return None;
    }
    let bytes = source.as_bytes();
    if previous_token_is_keyword(source, fact.byte_start, "new") {
        return None;
    }
    let open = skip_ws(bytes, fact.byte_end);
    if bytes.get(open) != Some(&b'(') {
        return None;
    }
    let close = find_matching_paren(source, open)?;
    if !source[open + 1..close].trim().is_empty() {
        return None;
    }
    Some((fact.byte_start, close + 1))
}
