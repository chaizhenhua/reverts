use reverts_js::{
    is_ascii_identifier_continue as is_identifier_continue,
    is_ascii_identifier_start as is_identifier_start, skip_block_comment, skip_line_comment,
};

use crate::byte_lexer::{
    find_matching_brace, find_matching_bracket, find_matching_paren, looks_like_regex_literal,
    skip_quoted, skip_regex_literal, skip_ws,
};
use crate::destructure_writes::split_top_level_properties;
use crate::identifiers::{keyword_at, parse_identifier};
use crate::is_runtime_global_identifier;

pub(crate) fn is_pure_initializer_expression(source: &str) -> bool {
    let source = source.trim();
    if source.is_empty() {
        return false;
    }
    if matches!(source, "void 0") {
        return true;
    }
    if is_literal_expression(source) {
        return true;
    }
    if source.as_bytes().first() == Some(&b'{') {
        return pure_object_literal(source);
    }
    if source.as_bytes().first() == Some(&b'[') {
        return pure_array_literal(source);
    }
    if keyword_at(source, 0, "class") {
        return pure_class_expression(source);
    }
    if keyword_at(source, 0, "function") || looks_like_arrow_function_expression(source) {
        return true;
    }
    false
}

pub(crate) fn is_literal_expression(source: &str) -> bool {
    if matches!(
        source,
        "true" | "false" | "null" | "undefined" | "NaN" | "Infinity"
    ) {
        return true;
    }
    if quoted_literal_covers_source(source) {
        return true;
    }
    if matches!(source, "!0" | "!1") {
        return true;
    }
    if source.starts_with('`') && source.ends_with('`') && !source.contains("${") {
        return true;
    }
    let number = source.strip_prefix(['+', '-']).unwrap_or(source);
    !number.is_empty()
        && number
            .chars()
            .all(|character| character.is_ascii_digit() || matches!(character, '.' | '_' | 'n'))
        && number.chars().any(|character| character.is_ascii_digit())
}

pub(crate) fn quoted_literal_covers_source(source: &str) -> bool {
    let bytes = source.as_bytes();
    let Some(quote @ (b'\'' | b'"')) = bytes.first().copied() else {
        return false;
    };
    skip_quoted(bytes, 0, quote) == bytes.len()
}

pub(crate) fn pure_object_literal(source: &str) -> bool {
    let Some(close) = find_matching_brace(source, 0) else {
        return false;
    };
    if skip_ws(source.as_bytes(), close + 1) != source.len() {
        return false;
    }
    split_top_level_properties(&source[1..close])
        .into_iter()
        .all(pure_object_property)
}

pub(crate) fn pure_object_property(property: &str) -> bool {
    let property = property.trim();
    if property.is_empty() || property.starts_with("...") {
        return false;
    }
    if let Some(colon) = find_top_level_byte(property, b':') {
        if !pure_object_property_key(property[..colon].trim()) {
            return false;
        }
        let value = property[colon + 1..].trim();
        return is_pure_initializer_expression(value);
    }
    pure_object_method_property(property)
}

pub(crate) fn pure_object_property_key(source: &str) -> bool {
    let source = source.trim();
    if source.is_empty() {
        return false;
    }
    if source.as_bytes().first() == Some(&b'[') {
        let Some(close) = find_matching_bracket(source, 0) else {
            return false;
        };
        return skip_ws(source.as_bytes(), close + 1) == source.len()
            && is_literal_expression(&source[1..close]);
    }
    if quoted_literal_covers_source(source) {
        return true;
    }
    let key = source.strip_prefix(['+', '-']).unwrap_or(source);
    key.as_bytes()
        .first()
        .is_some_and(|byte| is_identifier_start(*byte) || byte.is_ascii_digit())
        && key
            .as_bytes()
            .iter()
            .all(|byte| is_identifier_continue(*byte) || *byte == b'.')
}

pub(crate) fn pure_object_method_property(source: &str) -> bool {
    let source = source.trim();
    let method_source = source
        .strip_prefix("async ")
        .or_else(|| source.strip_prefix("get "))
        .or_else(|| source.strip_prefix("set "))
        .unwrap_or(source)
        .trim_start();
    let Some(open_paren) = find_top_level_byte(method_source, b'(') else {
        return false;
    };
    if !pure_object_property_key(method_source[..open_paren].trim()) {
        return false;
    }
    let Some(close_paren) = find_matching_paren(method_source, open_paren) else {
        return false;
    };
    let open_body = skip_ws(method_source.as_bytes(), close_paren + 1);
    if method_source.as_bytes().get(open_body) != Some(&b'{') {
        return false;
    }
    let Some(close_body) = find_matching_brace(method_source, open_body) else {
        return false;
    };
    skip_ws(method_source.as_bytes(), close_body + 1) == method_source.len()
}

pub(crate) fn pure_array_literal(source: &str) -> bool {
    let Some(close) = find_matching_bracket(source, 0) else {
        return false;
    };
    if skip_ws(source.as_bytes(), close + 1) != source.len() {
        return false;
    }
    split_top_level_properties(&source[1..close])
        .into_iter()
        .all(|element| {
            let element = element.trim();
            !element.is_empty()
                && !element.starts_with("...")
                && is_pure_initializer_expression(element)
        })
}

pub(crate) fn pure_class_expression(source: &str) -> bool {
    let Some(open) = find_top_level_byte(source, b'{') else {
        return false;
    };
    let header = source[..open].trim();
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
    let Some(close) = find_matching_brace(source, open) else {
        return false;
    };
    if skip_ws(source.as_bytes(), close + 1) != source.len() {
        return false;
    }
    !source[open + 1..close]
        .split(|character: char| !is_identifier_continue(character as u8))
        .any(|word| word == "static")
}

pub(crate) fn looks_like_arrow_function_expression(source: &str) -> bool {
    find_top_level_arrow(source).is_some()
}

pub(crate) fn find_top_level_arrow(source: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    while cursor + 1 < bytes.len() {
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
            b'=' if paren_depth == 0
                && bracket_depth == 0
                && brace_depth == 0
                && bytes.get(cursor + 1) == Some(&b'>') =>
            {
                return Some(cursor);
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

pub(crate) fn find_top_level_byte(source: &str, target: u8) -> Option<usize> {
    let bytes = source.as_bytes();
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
            byte if byte == target
                && paren_depth == 0
                && bracket_depth == 0
                && brace_depth == 0 =>
            {
                return Some(cursor);
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
