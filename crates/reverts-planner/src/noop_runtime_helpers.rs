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
    localize_lazy_value_source, lowered_lazy_initializer_statement_binding, previous_non_ws,
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
    let noop_candidates = candidates
        .iter()
        .filter(|binding| !exported_bindings.contains(*binding))
        .filter(|binding| {
            prelude.snippets.get(*binding).is_some_and(|snippet| {
                runtime_prelude_snippet_is_noop(binding.as_str(), snippet.source.as_str())
            })
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    let localizable = source_reads_bindings_only_as_erasable_noop_calls(source, &noop_candidates);
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
                String::new(),
            )
        })
        .collect::<Vec<_>>();
    apply_text_edits(source, &expand_line_removal_edits(source, &edits))
}

pub(crate) fn source_reads_bindings_only_as_erasable_noop_calls(
    source: &str,
    bindings: &BTreeSet<BindingName>,
) -> BTreeSet<BindingName> {
    if bindings.is_empty() {
        return BTreeSet::new();
    }
    let runtime_writes = implicit_global_writes_in_source(source);
    let candidates = bindings
        .difference(&runtime_writes)
        .cloned()
        .collect::<BTreeSet<_>>();
    if candidates.is_empty() {
        return BTreeSet::new();
    }
    let mut has_read = BTreeSet::<BindingName>::new();
    let mut has_non_erasable_read = BTreeSet::<BindingName>::new();
    for fact in identifier_read_facts_in_source(source) {
        let binding = BindingName::new(fact.name.as_str());
        if !candidates.contains(&binding) {
            continue;
        }
        has_read.insert(binding.clone());
        if noop_call_replacement_span(source, &fact).is_none() {
            has_non_erasable_read.insert(binding);
        }
    }
    has_read
        .difference(&has_non_erasable_read)
        .cloned()
        .collect()
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
    // Only erase a helper call whose *result* is discarded. If the call is the
    // object of a member/index access, a further call, or a tagged template
    // (e.g. `helper().fetch`), its value is consumed, so the helper is not a
    // genuine no-op — replacing it with `void 0` would both break semantics and
    // emit invalid syntax (`void 0.fetch` lexes `0.` as a number). Keep it.
    let after_close = skip_ws(bytes, close + 1);
    if matches!(
        bytes.get(after_close),
        Some(b'.' | b'[' | b'(' | b'`' | b'?')
    ) {
        return None;
    }
    // Do not erase when the call is assigned to a binding that is later used as
    // an object (`X = helper(); … X.prop`). A pure init shim returns `undefined`,
    // so discarding or returning its result is fine — but a CommonJS-style init
    // whose *exports* are captured and member-accessed is not a no-op; erasing it
    // makes `X` undefined and crashes at `X.prop` (e.g. `OCA = sentryInit()` then
    // `OCA.SEMANTIC_ATTRIBUTE_PROFILE_ID`).
    if let Some(equals) = previous_non_ws(bytes, fact.byte_start).filter(|index| {
        bytes[*index] == b'='
            && !matches!(bytes.get(*index + 1), Some(b'=' | b'>'))
            && previous_non_ws(bytes, *index)
                .is_none_or(|prev| !is_assignment_operator_lead(bytes[prev]))
    }) && let Some(target) = simple_assignment_target_before(source, equals)
        && binding_used_as_object(source, target.as_str())
    {
        return None;
    }
    Some((fact.byte_start, close + 1))
}

/// Whether `byte` immediately before an `=` makes it a compound-assignment or
/// comparison operator (`+=`, `==`, `<=`, `!=`, …) rather than a plain `=`.
fn is_assignment_operator_lead(byte: u8) -> bool {
    matches!(
        byte,
        b'=' | b'!' | b'<' | b'>' | b'+' | b'-' | b'*' | b'/' | b'%' | b'&' | b'|' | b'^'
    )
}

fn is_identifier_byte(byte: u8) -> bool {
    byte == b'_' || byte == b'$' || byte.is_ascii_alphanumeric()
}

/// The simple identifier assignment target immediately before `equals_index`
/// (the `X` in `X = …`). Returns `None` for member targets (`X.y = …`) or
/// non-identifier left-hand sides.
fn simple_assignment_target_before(source: &str, equals_index: usize) -> Option<String> {
    let bytes = source.as_bytes();
    let end = previous_non_ws(bytes, equals_index)? + 1;
    let mut start = end;
    while start > 0 && is_identifier_byte(bytes[start - 1]) {
        start -= 1;
    }
    if start == end {
        return None;
    }
    // A member target (`X.y = …`) or property-of (`a.X = …`) is not a plain
    // binding write, so leave it to normal handling.
    if start
        .checked_sub(1)
        .and_then(|index| bytes.get(index))
        .is_some_and(|byte| matches!(*byte, b'.' | b'?'))
    {
        return None;
    }
    Some(source[start..end].to_string())
}

/// Whether `binding` appears anywhere in `source` as the object of a member or
/// index access (`binding.x`, `binding[...]`, `binding?.x`) — i.e. its value is
/// consumed as an object, not merely held.
fn binding_used_as_object(source: &str, binding: &str) -> bool {
    if binding.is_empty() {
        return false;
    }
    let bytes = source.as_bytes();
    let mut from = 0;
    while let Some(found) = source[from..].find(binding) {
        let start = from + found;
        let end = start + binding.len();
        let before_ok = start
            .checked_sub(1)
            .and_then(|index| bytes.get(index))
            .is_none_or(|byte| !is_identifier_byte(*byte));
        let word_end = bytes.get(end).is_none_or(|byte| !is_identifier_byte(*byte));
        if before_ok && word_end && matches!(bytes.get(end), Some(b'.' | b'[' | b'?')) {
            return true;
        }
        from = end;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn helpers(names: &[&str]) -> BTreeSet<BindingName> {
        names.iter().map(|name| BindingName::new(*name)).collect()
    }

    #[test]
    fn erases_discarded_noop_call_but_keeps_consumed_result() {
        // A bare statement call is a discardable no-op and folds to `void 0`.
        // A call whose result is member-accessed is consumed, so it must stay:
        // folding it would emit `void 0.fetch` (invalid syntax, broken meaning).
        let source = "h();\nconst a = h().fetch;\n";
        let rewritten = rewrite_noop_runtime_helper_calls(source, &helpers(&["h"]));
        assert!(rewritten.contains("void 0"), "{rewritten}");
        assert!(!rewritten.contains("void 0.fetch"), "{rewritten}");
        assert!(rewritten.contains("h().fetch"), "{rewritten}");
    }

    #[test]
    fn keeps_noop_call_used_as_index_call_or_template_object() {
        for source in [
            "const a = h()[k];\n",
            "const a = h()(x);\n",
            "const a = h()`tpl`;\n",
            "const a = h()?.fetch;\n",
        ] {
            let rewritten = rewrite_noop_runtime_helper_calls(source, &helpers(&["h"]));
            assert_eq!(
                rewritten, source,
                "consumed call must be preserved: {source}"
            );
        }
    }

    #[test]
    fn keeps_noop_call_assigned_to_a_member_accessed_binding() {
        // `OCA = sentryInit(); … OCA.PROP` — the init's exports are captured and
        // member-accessed, so folding the call to `void 0` makes `OCA` undefined
        // and crashes at `OCA.PROP`. The assignment must be preserved.
        let source = "OCA = sentryInit();\nx.PROFILE = OCA.SEMANTIC_ATTRIBUTE_PROFILE_ID;\n";
        let rewritten = rewrite_noop_runtime_helper_calls(source, &helpers(&["sentryInit"]));
        assert_eq!(rewritten, source, "{rewritten}");
    }

    #[test]
    fn still_erases_noop_call_assigned_to_an_unused_or_value_only_binding() {
        // No member access on the target -> the result is effectively discarded,
        // so the pure-init no-op still folds.
        let source = "flag = sideEffectOnlyInit();\nuse(flag);\n";
        let rewritten =
            rewrite_noop_runtime_helper_calls(source, &helpers(&["sideEffectOnlyInit"]));
        assert!(rewritten.contains("flag = void 0;"), "{rewritten}");
    }
}
