//! Local-binding scope traversal extracted from `lib.rs`.
//!
//! Walks an emitted source slice and collects identifier names that are
//! locally bound within it: function/class declarations, `var`/`let`/`const`
//! destructuring patterns, arrow-function parameters, and catch clauses.
//! Used by sibling modules to determine which identifiers in a fragment
//! refer to in-scope locals versus free runtime/module references.

use std::collections::BTreeSet;

use reverts_js::{
    is_ascii_identifier_continue as is_identifier_continue,
    is_ascii_identifier_start as is_identifier_start, is_js_keyword, skip_block_comment,
    skip_line_comment,
};

use crate::byte_lexer::{
    find_matching_brace, find_matching_bracket, find_matching_paren, looks_like_regex_literal,
    skip_quoted, skip_regex_literal, skip_ws,
};
use crate::identifier_facts::{control_flow_keyword_before_paren, keyword_before_paren};
use crate::identifiers::{
    keyword_at, parse_identifier, parse_identifier_after_function_keyword,
    parse_identifier_after_keyword,
};
use crate::previous_non_ws;

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
