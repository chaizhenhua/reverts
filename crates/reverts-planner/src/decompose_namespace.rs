use std::collections::BTreeSet;

use reverts_ir::BindingName;
use reverts_js::{
    is_ascii_identifier_continue as is_identifier_continue,
    is_ascii_identifier_start as is_identifier_start,
};

use crate::byte_lexer::{find_matching_brace, skip_non_code_at, skip_ws};
use crate::identifiers::{keyword_at, parse_identifier};
use crate::{
    identifier_occurrence_is_value_reference, looks_like_arrow_function_expression,
    previous_non_ws, pure_class_expression, top_level_definitions_in_source,
};

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
