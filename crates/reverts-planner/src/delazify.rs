use std::collections::BTreeSet;

use reverts_ir::BindingName;
use reverts_js::{
    ParseGoal, extract_lazy_module_eager_value_with_safe_deps,
    is_ascii_identifier_continue as is_identifier_continue,
    is_ascii_identifier_start as is_identifier_start,
};

use crate::byte_lexer::{expect_arrow, find_matching_brace, skip_non_code_at, skip_ws};
use crate::identifiers::{keyword_at, parse_identifier};
use crate::{identifier_occurrence_is_value_reference, previous_non_ws};

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
    let value_expr = extract_lazy_module_eager_value_with_safe_deps(
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
    let value_expr = extract_lazy_module_eager_value_with_safe_deps(
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
