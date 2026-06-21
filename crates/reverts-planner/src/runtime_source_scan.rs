//! Runtime source scanning helpers extracted from `lib.rs`.
//!
//! These passes parse generated runtime helper bodies to determine which
//! free identifiers the helper still depends on, after subtracting locally
//! introduced bindings and JS globals. Used by the helper-source closure
//! and the externalized-binding scan to know which extra prelude bindings
//! must be transitively pulled in.

use std::collections::BTreeSet;

use crate::byte_lexer::{
    find_matching_brace, find_matching_paren, looks_like_regex_literal, skip_quoted,
    skip_regex_literal, skip_ws,
};
use crate::identifier_facts::identifier_read_facts_in_source;
use crate::identifiers::{keyword_at, parse_identifier, parse_identifier_after_function_keyword};
use crate::is_runtime_global_identifier;
use crate::local_bindings::{keyword_starts_statement_declaration, local_bindings_in_source};
use reverts_js::{
    is_ascii_identifier_continue as is_identifier_continue,
    is_ascii_identifier_start as is_identifier_start, skip_block_comment, skip_line_comment,
};

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

/// Top-level names bound to a FUNCTION value: `function NAME(){}`,
/// `NAME = function …`, `NAME = (…) => …`, `NAME = x => …` (incl. names in a
/// comma-chained `const a = …, NAME = (…) => …`). Unlike
/// [`top_level_function_declaration_names`] this also captures arrow/function
/// expression assignments — the shape esbuild emits for many package helpers
/// (e.g. execa's `sut = (e, { verbose }) => { … }`). Member assignments
/// (`a.b = () => …`) are excluded: only a bare identifier in binding position
/// is recorded.
pub(crate) fn top_level_function_valued_names(source: &str) -> BTreeSet<String> {
    let bytes = source.as_bytes();
    let mut names = BTreeSet::new();
    let mut cursor = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    while cursor < bytes.len() {
        let top_level = paren_depth == 0 && bracket_depth == 0 && brace_depth == 0;
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
            _ if top_level && keyword_at(source, cursor, "function") => {
                if let Some((name, next)) = parse_identifier_after_function_keyword(source, cursor)
                {
                    names.insert(name.to_string());
                    cursor = next;
                } else {
                    cursor += "function".len();
                }
            }
            _ if top_level
                && is_identifier_start(bytes[cursor])
                && !preceded_by_member_dot(bytes, cursor) =>
            {
                if let Some((identifier, after)) = parse_identifier(source, cursor) {
                    let equals = skip_ws(bytes, after);
                    if bytes.get(equals) == Some(&b'=')
                        && !matches!(bytes.get(equals + 1), Some(&b'=') | Some(&b'>'))
                        && rhs_is_function_value(source, skip_ws(bytes, equals + 1))
                    {
                        names.insert(identifier.to_string());
                    }
                    cursor = after;
                } else {
                    cursor += 1;
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

/// True if the identifier starting at `cursor` is a property access (the
/// previous non-whitespace byte is `.`), so it is NOT a binding position.
fn preceded_by_member_dot(bytes: &[u8], cursor: usize) -> bool {
    let mut index = cursor;
    while index > 0 {
        index -= 1;
        match bytes[index] {
            b' ' | b'\t' | b'\r' | b'\n' => {}
            b'.' => return true,
            _ => return false,
        }
    }
    false
}

/// True if the expression starting at `pos` is a function value: a `function`
/// expression, an `async` function/arrow, a parenthesized arrow `(…) => …`, or
/// a single-parameter arrow `x => …`.
fn rhs_is_function_value(source: &str, pos: usize) -> bool {
    let bytes = source.as_bytes();
    if pos >= bytes.len() {
        return false;
    }
    if keyword_at(source, pos, "function") || keyword_at(source, pos, "async") {
        return true;
    }
    if bytes[pos] == b'(' {
        let Some(close) = find_matching_paren(source, pos) else {
            return false;
        };
        return bytes.get(skip_ws(bytes, close + 1)) == Some(&b'=')
            && bytes.get(skip_ws(bytes, close + 1) + 1) == Some(&b'>');
    }
    if is_identifier_start(bytes[pos])
        && let Some((_, after)) = parse_identifier(source, pos)
    {
        let arrow = skip_ws(bytes, after);
        return bytes.get(arrow) == Some(&b'=') && bytes.get(arrow + 1) == Some(&b'>');
    }
    false
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
