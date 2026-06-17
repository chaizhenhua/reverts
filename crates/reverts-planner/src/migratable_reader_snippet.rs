//! Migratable reader-snippet recognition extracted from `lib.rs`.
//!
//! Decides whether a runtime helper snippet (a `function`, `class`, or
//! `var`/`let`/`const` declaration in a prelude body) is safe to relocate
//! into a reader module's own source: classes with eager static state or
//! computed property keys are rejected because their body would execute at
//! class-definition time inside the reader module before the runtime
//! helpers it depends on have been initialised.

use reverts_ir::BindingName;

use crate::byte_lexer::{
    find_matching_brace, find_matching_paren, skip_quoted, skip_ws, skip_ws_and_comments,
};
use crate::identifiers::{
    keyword_at, parse_identifier, parse_identifier_after_function_keyword,
    parse_identifier_after_keyword,
};
use crate::is_runtime_global_identifier;
use crate::lazy_initializer_parse::find_statement_end;
use crate::pure_expression::{
    find_top_level_byte, is_pure_initializer_expression, looks_like_arrow_function_expression,
};
use reverts_js::{skip_block_comment, skip_line_comment};

pub(crate) fn is_migratable_private_runtime_function_dependency(
    binding: &BindingName,
    source: &str,
) -> bool {
    let source = source.trim();
    if let Some(rest) = source.strip_prefix("async")
        && rest.starts_with(|c: char| c.is_ascii_whitespace())
    {
        return function_declaration_names_binding(rest.trim_start(), binding);
    }
    function_declaration_names_binding(source, binding)
}

pub(crate) fn is_migratable_reader_function_snippet(binding: &BindingName, source: &str) -> bool {
    let source = source.trim();
    if let Some(rest) = source.strip_prefix("async")
        && rest.starts_with(|c: char| c.is_ascii_whitespace())
    {
        let rest = rest.trim_start();
        return function_declaration_names_binding(rest, binding);
    }
    class_declaration_names_binding(source, binding)
        || function_declaration_names_binding(source, binding)
        || variable_declaration_names_function_like_binding(source, binding)
}

pub(crate) fn is_migratable_namespace_reader_snippet(binding: &BindingName, source: &str) -> bool {
    let source = source.trim();
    for keyword in ["var", "let", "const"] {
        let Some(rest) = source.strip_prefix(keyword) else {
            continue;
        };
        if !rest.starts_with(|c: char| c.is_ascii_whitespace()) {
            continue;
        }
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_suffix(';') else {
            continue;
        };
        let mut splitter = rest.splitn(2, '=');
        let lhs = splitter.next().unwrap_or("").trim();
        let rhs = splitter.next().unwrap_or("").trim();
        if lhs != binding.as_str() {
            continue;
        }
        let compact_rhs = rhs
            .chars()
            .filter(|character| !character.is_ascii_whitespace())
            .collect::<String>();
        if compact_rhs == "{}" {
            return true;
        }
    }
    false
}

pub(crate) fn function_declaration_names_binding(source: &str, binding: &BindingName) -> bool {
    let function_offset = if keyword_at(source, 0, "function") {
        0
    } else if keyword_at(source, 0, "async") {
        let offset = skip_ws(source.as_bytes(), "async".len());
        if keyword_at(source, offset, "function") {
            offset
        } else {
            return false;
        }
    } else {
        return false;
    };
    parse_identifier_after_function_keyword(source, function_offset)
        .is_some_and(|(name, _)| name == binding.as_str())
}

pub(crate) fn class_declaration_names_binding(source: &str, binding: &BindingName) -> bool {
    if !keyword_at(source, 0, "class") {
        return false;
    }
    parse_identifier_after_keyword(source, 0, "class")
        .is_some_and(|(name, _)| name == binding.as_str())
        && is_migratable_reader_class(source)
}

pub(crate) fn is_migratable_reader_class(source: &str) -> bool {
    let Some(open) = find_top_level_byte(source, b'{') else {
        return false;
    };
    let header = source[..open].trim();
    if !migratable_reader_class_header(header) {
        return false;
    }
    let Some(close) = find_matching_brace(source, open) else {
        return false;
    };
    if skip_ws(source.as_bytes(), close + 1) != source.len() {
        return false;
    }
    !class_body_has_top_level_computed_key(source) && !class_body_has_eager_static_element(source)
}

pub(crate) fn migratable_reader_class_header(header: &str) -> bool {
    if header.contains('[') {
        return false;
    }
    if let Some(extends) = header
        .split_once("extends")
        .map(|(_, extends)| extends.trim())
    {
        let (base, end) = match parse_identifier(extends, 0) {
            Some(parsed) => parsed,
            None => return false,
        };
        if end != extends.len() || !is_runtime_global_identifier(base) {
            return false;
        }
    }
    true
}

pub(crate) fn class_body_has_top_level_computed_key(source: &str) -> bool {
    let Some(open) = find_top_level_byte(source, b'{') else {
        return true;
    };
    let Some(close) = find_matching_brace(source, open) else {
        return true;
    };
    let bytes = source.as_bytes();
    let mut cursor = open + 1;
    while cursor < close {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            // `field = [];` is an instance-field initializer, not a
            // computed property key. It is evaluated when instances are
            // constructed, so moving the class with its reader cluster does
            // not introduce class-definition-time reads.
            b'=' => {
                cursor =
                    find_statement_end(&source[..close], cursor + 1).map_or(close, |end| end + 1);
            }
            b'[' => return true,
            b'(' => {
                let Some(end) = find_matching_paren(source, cursor) else {
                    return true;
                };
                cursor = end + 1;
            }
            b'{' => {
                let Some(end) = find_matching_brace(source, cursor) else {
                    return true;
                };
                cursor = end + 1;
            }
            _ => cursor += 1,
        }
    }
    false
}

pub(crate) fn class_body_has_eager_static_element(source: &str) -> bool {
    let Some(open) = find_top_level_byte(source, b'{') else {
        return true;
    };
    let Some(close) = find_matching_brace(source, open) else {
        return true;
    };
    let bytes = source.as_bytes();
    let mut cursor = open + 1;
    while cursor < close {
        cursor = skip_ws_and_comments(bytes, cursor, close);
        if cursor >= close {
            break;
        }
        if keyword_at(source, cursor, "static")
            && static_class_element_is_eager(source, cursor + "static".len(), close)
        {
            return true;
        }
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'(' => {
                let Some(end) = find_matching_paren(source, cursor) else {
                    return true;
                };
                cursor = end + 1;
            }
            b'{' => {
                let Some(end) = find_matching_brace(source, cursor) else {
                    return true;
                };
                cursor = end + 1;
            }
            _ => cursor += 1,
        }
    }
    false
}

pub(crate) fn static_class_element_is_eager(
    source: &str,
    after_static: usize,
    close: usize,
) -> bool {
    let bytes = source.as_bytes();
    let cursor = skip_ws_and_comments(bytes, after_static, close);
    if cursor >= close {
        return false;
    }
    match bytes[cursor] {
        // `static() {}` / `static = ...` / `static;` are instance
        // members named "static", not static elements, so they don't run
        // during class definition.
        b'(' | b'=' | b';' => return false,
        // Static blocks run immediately while the class is being defined.
        b'{' => return true,
        _ => {}
    }

    let mut cursor = cursor;
    while cursor < close {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor = skip_line_comment(bytes, cursor + 2);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor = skip_block_comment(bytes, cursor + 2);
            }
            // Method/accessor parameter list: safe, because the body is not
            // evaluated until the method is called after the writer module's
            // same-module assignment has run.
            b'(' => return false,
            b'=' => return static_field_initializer_is_eager(source, cursor, close),
            // Empty static fields don't read migrated runtime state.
            b';' => return false,
            // Static blocks run immediately while the class is being defined.
            b'{' => return true,
            _ => cursor += 1,
        }
    }
    false
}

pub(crate) fn static_field_initializer_is_eager(source: &str, equals: usize, close: usize) -> bool {
    let bytes = source.as_bytes();
    let initializer_start = skip_ws_and_comments(bytes, equals + 1, close);
    if initializer_start >= close {
        return false;
    }
    let initializer_end = find_statement_end(&source[..close], initializer_start).unwrap_or(close);
    let initializer = source[initializer_start..initializer_end].trim();
    !initializer.is_empty() && !is_pure_initializer_expression(initializer)
}

pub(crate) fn variable_declaration_names_function_like_binding(
    source: &str,
    binding: &BindingName,
) -> bool {
    let source = source.trim();
    for keyword in ["var", "let", "const"] {
        let Some(rest) = source.strip_prefix(keyword) else {
            continue;
        };
        if !rest.starts_with(|c: char| c.is_ascii_whitespace()) {
            continue;
        }
        let Some(rest) = rest.trim_start().strip_suffix(';') else {
            continue;
        };
        let mut splitter = rest.splitn(2, '=');
        let lhs = splitter.next().unwrap_or("").trim();
        let rhs = splitter.next().unwrap_or("").trim();
        if lhs != binding.as_str() {
            continue;
        }
        if expression_is_function_like_reader(rhs) {
            return true;
        }
    }
    false
}

pub(crate) fn expression_is_function_like_reader(source: &str) -> bool {
    let source = source.trim();
    if keyword_at(source, 0, "function") || looks_like_arrow_function_expression(source) {
        return true;
    }
    if let Some(rest) = source.strip_prefix("async")
        && rest.starts_with(|c: char| c.is_ascii_whitespace())
    {
        let rest = rest.trim_start();
        return keyword_at(rest, 0, "function") || looks_like_arrow_function_expression(rest);
    }
    false
}
