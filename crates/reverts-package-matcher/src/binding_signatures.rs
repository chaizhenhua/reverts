//! Text-based extraction of binding-string-literal signatures from a
//! JavaScript source body. Walks declarations/assignments without a full
//! parser and records the set of string literals appearing in each binding's
//! initializer. Used by the export-member proof and dependency-edge scorer
//! to compare cross-module shapes without re-parsing each source.

use std::collections::{BTreeMap, BTreeSet};

use crate::source::exported_members::is_identifier_name;
use crate::source::source_text::{
    previous_non_ascii_ws, read_identifier_with_end_at, skip_ascii_ws,
};

#[must_use]
pub(crate) fn binding_string_signatures_from_source(
    source: &str,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut signatures = BTreeMap::<String, BTreeSet<String>>::new();
    let bytes = source.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_for_signature(source, cursor) {
            cursor = next;
            continue;
        }
        let Some((identifier, after_identifier)) = read_identifier_with_end_at(source, cursor)
        else {
            cursor += 1;
            continue;
        };

        if matches!(identifier, "var" | "let" | "const") {
            if let Some((binding, expression_start)) =
                variable_initializer_start_for_signature(source, after_identifier)
            {
                let end = signature_expression_end(source, expression_start);
                let initializer = &source[expression_start..end];
                if !initializer_is_lazy_wrapper_for_signature(initializer) {
                    insert_binding_string_signature(&mut signatures, binding, initializer);
                    cursor = end;
                } else {
                    cursor = expression_start;
                }
                continue;
            }
        } else if identifier == "class" {
            if let Some((binding, class_start)) =
                class_declaration_start_for_signature(source, cursor, after_identifier)
            {
                let end = signature_expression_end(source, class_start);
                insert_binding_string_signature(
                    &mut signatures,
                    binding,
                    &source[class_start..end],
                );
                cursor = end;
                continue;
            }
        } else if identifier == "function" {
            if let Some((binding, function_start)) =
                function_declaration_start_for_signature(source, cursor, after_identifier)
            {
                let end = signature_expression_end(source, function_start);
                insert_binding_string_signature(
                    &mut signatures,
                    binding,
                    &source[function_start..end],
                );
                cursor = end;
                continue;
            }
        } else if let Some((binding, expression_start)) =
            commonjs_export_initializer_start_for_signature(source, cursor)
        {
            let end = signature_expression_end(source, expression_start);
            insert_binding_string_signature(
                &mut signatures,
                binding,
                &source[expression_start..end],
            );
            cursor = end;
            continue;
        } else if assignment_lhs_is_standalone_identifier(source, cursor) {
            let after_ws = skip_ascii_ws(bytes, after_identifier);
            if bytes.get(after_ws) == Some(&b'=')
                && bytes.get(after_ws + 1) != Some(&b'=')
                && bytes.get(after_ws + 1) != Some(&b'>')
            {
                let expression_start = skip_ascii_ws(bytes, after_ws + 1);
                let end = signature_expression_end(source, expression_start);
                insert_binding_string_signature(
                    &mut signatures,
                    identifier,
                    &source[expression_start..end],
                );
                cursor = end;
                continue;
            }
        }
        cursor = after_identifier;
    }
    signatures
}

fn initializer_is_lazy_wrapper_for_signature(initializer: &str) -> bool {
    let trimmed = initializer.trim_start();
    trimmed.starts_with("E(")
        || trimmed.starts_with("lazyValue(")
        || trimmed.starts_with("lazyModule(")
        || trimmed.starts_with("__commonJS(")
}

fn insert_binding_string_signature(
    signatures: &mut BTreeMap<String, BTreeSet<String>>,
    binding: &str,
    source: &str,
) {
    if !is_identifier_name(binding) {
        return;
    }
    let strings = quoted_string_literals_from_source(source);
    if strings.is_empty() {
        return;
    }
    signatures
        .entry(binding.to_string())
        .or_default()
        .extend(strings);
}

fn variable_initializer_start_for_signature(source: &str, start: usize) -> Option<(&str, usize)> {
    let bytes = source.as_bytes();
    let binding_start = skip_ascii_ws(bytes, start);
    let (binding, after_binding) = read_identifier_with_end_at(source, binding_start)?;
    let equals = skip_ascii_ws(bytes, after_binding);
    if bytes.get(equals) != Some(&b'=') {
        return None;
    }
    Some((binding, skip_ascii_ws(bytes, equals + 1)))
}

fn class_declaration_start_for_signature(
    source: &str,
    class_start: usize,
    after_keyword: usize,
) -> Option<(&str, usize)> {
    let binding_start = skip_ascii_ws(source.as_bytes(), after_keyword);
    let (binding, _after_binding) = read_identifier_with_end_at(source, binding_start)?;
    Some((binding, class_start))
}

fn function_declaration_start_for_signature(
    source: &str,
    function_start: usize,
    after_keyword: usize,
) -> Option<(&str, usize)> {
    let binding_start = skip_ascii_ws(source.as_bytes(), after_keyword);
    let (binding, _after_binding) = read_identifier_with_end_at(source, binding_start)?;
    Some((binding, function_start))
}

fn commonjs_export_initializer_start_for_signature(
    source: &str,
    start: usize,
) -> Option<(&str, usize)> {
    let bytes = source.as_bytes();
    let (object, after_object) = read_identifier_with_end_at(source, start)?;
    let member_start = if object == "exports" {
        if bytes.get(after_object) != Some(&b'.') {
            return None;
        }
        after_object + 1
    } else if object == "module" {
        let dot = after_object;
        if bytes.get(dot) != Some(&b'.') {
            return None;
        }
        let (exports, after_exports) = read_identifier_with_end_at(source, dot + 1)?;
        if exports != "exports" || bytes.get(after_exports) != Some(&b'.') {
            return None;
        }
        after_exports + 1
    } else {
        return None;
    };
    let (member, after_member) = read_identifier_with_end_at(source, member_start)?;
    let equals = skip_ascii_ws(bytes, after_member);
    if bytes.get(equals) != Some(&b'=')
        || bytes.get(equals + 1) == Some(&b'=')
        || bytes.get(equals + 1) == Some(&b'>')
    {
        return None;
    }
    Some((member, skip_ascii_ws(bytes, equals + 1)))
}

fn assignment_lhs_is_standalone_identifier(source: &str, start: usize) -> bool {
    previous_non_ascii_ws(source.as_bytes(), start).is_none_or(|byte| !matches!(byte, b'.' | b'#'))
}

fn signature_expression_end(source: &str, start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut cursor = start;
    let mut paren_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut bracket_depth = 0usize;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_for_signature(source, cursor) {
            cursor = next;
            continue;
        }
        match bytes[cursor] {
            b'(' => paren_depth += 1,
            b')' => paren_depth = paren_depth.saturating_sub(1),
            b'{' => brace_depth += 1,
            b'}' => {
                brace_depth = brace_depth.saturating_sub(1);
                if paren_depth == 0 && brace_depth == 0 && bracket_depth == 0 {
                    return cursor + 1;
                }
            }
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b';' if paren_depth == 0 && brace_depth == 0 && bracket_depth == 0 => {
                return cursor + 1;
            }
            _ => {}
        }
        cursor += 1;
    }
    source.len()
}

fn quoted_string_literals_from_source(source: &str) -> BTreeSet<String> {
    let bytes = source.as_bytes();
    let mut strings = BTreeSet::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => {
                let quote = bytes[cursor];
                let (value, next) = read_quoted_string_literal_for_signature(source, cursor, quote);
                let trimmed = value.trim();
                if (3..=128).contains(&trimmed.len()) {
                    strings.insert(trimmed.to_string());
                }
                cursor = next;
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment_for_signature(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment_for_signature(bytes, cursor + 2);
            }
            _ => cursor += 1,
        }
    }
    strings
}

fn read_quoted_string_literal_for_signature(
    source: &str,
    start: usize,
    quote: u8,
) -> (String, usize) {
    let mut escaped = false;
    let mut out = String::new();
    for (offset, ch) in source[start + 1..].char_indices() {
        if escaped {
            out.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch as u8 == quote {
            return (out, start + 1 + offset + ch.len_utf8());
        }
        out.push(ch);
    }
    (out, source.len())
}

fn skip_non_code_for_signature(source: &str, cursor: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    match bytes.get(cursor).copied()? {
        b'\'' | b'"' | b'`' => {
            Some(read_quoted_string_literal_for_signature(source, cursor, bytes[cursor]).1)
        }
        b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
            Some(skip_line_comment_for_signature(bytes, cursor + 2))
        }
        b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
            Some(skip_block_comment_for_signature(bytes, cursor + 2))
        }
        _ => None,
    }
}

fn skip_line_comment_for_signature(bytes: &[u8], mut cursor: usize) -> usize {
    while cursor < bytes.len() && bytes[cursor] != b'\n' {
        cursor += 1;
    }
    cursor
}

fn skip_block_comment_for_signature(bytes: &[u8], mut cursor: usize) -> usize {
    while cursor + 1 < bytes.len() {
        if bytes[cursor] == b'*' && bytes[cursor + 1] == b'/' {
            return cursor + 2;
        }
        cursor += 1;
    }
    bytes.len()
}
