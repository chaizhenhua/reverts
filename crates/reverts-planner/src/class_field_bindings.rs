//! Class field binding scan extracted from `lib.rs`.
//!
//! Locates top-level class declarations in an emitted source slice and
//! records the textual names of their declared fields (everything that
//! ends with `;` or `=`). Used by the local-bindings analysis to detect
//! identifiers that should not be treated as free references.

use std::collections::BTreeMap;

use reverts_js::{
    is_ascii_identifier_continue as is_identifier_continue,
    is_ascii_identifier_start as is_identifier_start, skip_block_comment, skip_line_comment,
};

use crate::byte_lexer::{
    find_matching_brace, looks_like_regex_literal, skip_quoted, skip_regex_literal, skip_ws,
};
use crate::identifiers::keyword_at;

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
