//! Lazy initializer parsing extracted from `lib.rs`.
//!
//! Parses lowered `lazyValue(() => { ... })` declarations and the
//! pure-value declarations used by the post-inline pass to recognise
//! purifiable runtime lazy initializers.

use std::collections::BTreeSet;

use reverts_ir::BindingName;

use crate::byte_lexer::{expect_arrow, find_matching_brace, skip_non_code_at, skip_ws};
use crate::identifiers::{
    declaration_keyword_at, declaration_keyword_at_start, keyword_at, parse_identifier,
    parse_identifier_after_function_keyword, parse_identifier_after_keyword,
};
use crate::pure_expression::is_pure_initializer_expression;
use crate::{
    is_runtime_global_identifier, looks_like_lowered_lazy_initializer, top_level_statement_slices,
};

pub(crate) fn private_runtime_lazy_initializer_replacement(
    initializer: &ParsedRuntimeLazyInitializer<'_>,
    writable_helpers: &BTreeSet<BindingName>,
    pure_value_bindings: &BTreeSet<BindingName>,
) -> Option<String> {
    let assignments = pure_runtime_lazy_body_assignments(
        initializer.body,
        writable_helpers,
        pure_value_bindings,
    )?;
    if assignments.is_empty() {
        return None;
    }
    let mut lines = assignments
        .into_iter()
        .map(|(target, value)| format!("{target} = {value};"))
        .collect::<Vec<_>>();
    lines.push(format!(
        "var {} = () => {{}};",
        initializer.binding.as_str()
    ));
    Some(lines.join("\n"))
}

pub(crate) fn pure_runtime_value_bindings(source: &str) -> BTreeSet<BindingName> {
    let mut bindings = BTreeSet::new();
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 {
            if keyword_at(source, cursor, "function")
                && let Some((binding, next)) =
                    parse_identifier_after_function_keyword(source, cursor)
            {
                bindings.insert(BindingName::new(binding));
                cursor = next;
                continue;
            }
            if keyword_at(source, cursor, "class")
                && let Some((binding, next)) =
                    parse_identifier_after_keyword(source, cursor, "class")
            {
                bindings.insert(BindingName::new(binding));
                cursor = next;
                continue;
            }
            if let Some((binding, value, after)) = parse_pure_top_level_var_value(source, cursor) {
                if is_pure_initializer_expression(value) {
                    bindings.insert(binding);
                }
                cursor = after;
                continue;
            }
        }
        match bytes[cursor] {
            b'(' => paren_depth += 1,
            b')' => paren_depth = paren_depth.saturating_sub(1),
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b'{' => brace_depth += 1,
            b'}' => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
        cursor += 1;
    }
    bindings
}

pub(crate) fn parse_pure_top_level_var_value(
    source: &str,
    start: usize,
) -> Option<(BindingName, &str, usize)> {
    let (keyword, after_keyword) = declaration_keyword_at(source, start)?;
    let mut cursor = skip_ws(source.as_bytes(), start + after_keyword);
    let (binding, after_binding) = parse_identifier(source, cursor)?;
    cursor = skip_ws(source.as_bytes(), after_binding);
    if source.as_bytes().get(cursor) != Some(&b'=') {
        return None;
    }
    let value_start = skip_ws(source.as_bytes(), cursor + 1);
    let stmt_end = find_statement_end(source, value_start)?;
    let _ = keyword;
    Some((
        BindingName::new(binding),
        source[value_start..stmt_end].trim(),
        stmt_end + usize::from(source.as_bytes().get(stmt_end) == Some(&b';')),
    ))
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedRuntimeLazyInitializer<'a> {
    pub(crate) binding: BindingName,
    pub(crate) body: &'a str,
    pub(crate) body_span: (usize, usize),
    pub(crate) span: (usize, usize),
}

pub(crate) fn try_parse_runtime_lazy_initializer_declaration(
    source: &str,
    start: usize,
) -> Option<(ParsedRuntimeLazyInitializer<'_>, usize)> {
    let (keyword, after_keyword) = declaration_keyword_at(source, start)?;
    let bytes = source.as_bytes();
    let mut cursor = skip_ws(bytes, start + after_keyword);
    let (binding, after_binding) = parse_identifier(source, cursor)?;
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
    let body_start = cursor + 1;
    let body_end = find_matching_brace(source, cursor)?;
    let body = &source[body_start..body_end];
    let after_body = skip_ws(bytes, body_end + 1);
    if bytes.get(after_body) != Some(&b')') {
        return None;
    }
    let after_paren = skip_ws(bytes, after_body + 1);
    if bytes.get(after_paren) != Some(&b';') {
        return None;
    }
    let _ = keyword;
    let stmt_end = after_paren + 1;
    Some((
        ParsedRuntimeLazyInitializer {
            binding: BindingName::new(binding),
            body,
            body_span: (body_start, body_end),
            span: (start, stmt_end),
        },
        stmt_end,
    ))
}

pub(crate) fn find_statement_end(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut cursor = start;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    while cursor < bytes.len() {
        if let Some(next) = skip_non_code_at(source, cursor) {
            cursor = next;
            continue;
        }
        match bytes[cursor] {
            b'(' => paren_depth += 1,
            b')' => paren_depth = paren_depth.saturating_sub(1),
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b'{' => brace_depth += 1,
            b'}' => brace_depth = brace_depth.saturating_sub(1),
            b';' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => {
                return Some(cursor);
            }
            _ => {}
        }
        cursor += 1;
    }
    None
}

pub(crate) fn pure_runtime_lazy_body_assignments(
    body: &str,
    writable_helpers: &BTreeSet<BindingName>,
    pure_value_bindings: &BTreeSet<BindingName>,
) -> Option<Vec<(BindingName, String)>> {
    let mut assignments = Vec::new();
    let mut written = BTreeSet::new();
    for statement in top_level_statement_slices(body) {
        let statement = statement.trim().trim_end_matches(';').trim();
        if statement.is_empty() {
            continue;
        }
        let (target, after_target) = parse_identifier(statement, 0)?;
        let target = BindingName::new(target);
        if !writable_helpers.contains(&target) || !written.insert(target.clone()) {
            return None;
        }
        let equals = skip_ws(statement.as_bytes(), after_target);
        if statement.as_bytes().get(equals) != Some(&b'=')
            || statement.as_bytes().get(equals + 1) == Some(&b'=')
            || statement.as_bytes().get(equals + 1) == Some(&b'>')
        {
            return None;
        }
        let value_start = skip_ws(statement.as_bytes(), equals + 1);
        let value = statement[value_start..].trim();
        if !is_pure_runtime_assignment_value(value, pure_value_bindings) {
            return None;
        }
        assignments.push((target, value.to_string()));
    }
    Some(assignments)
}

pub(crate) fn is_pure_runtime_assignment_value(
    value: &str,
    pure_value_bindings: &BTreeSet<BindingName>,
) -> bool {
    if is_pure_initializer_expression(value) {
        return true;
    }
    let value = value.trim();
    let Some((identifier, end)) = parse_identifier(value, 0) else {
        return false;
    };
    if end != value.len() {
        return false;
    }
    is_runtime_global_identifier(identifier)
        || pure_value_bindings.contains(&BindingName::new(identifier))
}

pub(crate) fn pure_lazy_initializer_replacement(
    statement: &str,
    writable_helpers: &BTreeSet<BindingName>,
) -> Option<String> {
    let initializer = parse_lowered_lazy_initializer_statement(statement)?;
    let assignments = pure_lazy_body_assignments(initializer.body, writable_helpers)?;
    if assignments.is_empty() {
        return None;
    }
    let mut lines = assignments
        .into_iter()
        .map(|(target, value)| format!("{target} = {value};"))
        .collect::<Vec<_>>();
    lines.push(format!(
        "var {} = () => {{}};",
        initializer.binding.as_str()
    ));
    Some(lines.join("\n"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedLoweredLazyInitializer<'a> {
    pub(crate) binding: BindingName,
    pub(crate) body: &'a str,
}

pub(crate) fn parse_lowered_lazy_initializer_statement(
    statement: &str,
) -> Option<ParsedLoweredLazyInitializer<'_>> {
    let statement = statement.trim().trim_end_matches(';').trim();
    let (_keyword, after_keyword) = declaration_keyword_at_start(statement)?;
    let cursor = skip_ws(statement.as_bytes(), after_keyword);
    let (binding, after_binding) = parse_identifier(statement, cursor)?;
    let equals = skip_ws(statement.as_bytes(), after_binding);
    if statement.as_bytes().get(equals) != Some(&b'=') {
        return None;
    }
    let initializer_start = skip_ws(statement.as_bytes(), equals + 1);
    let body = parse_lowered_lazy_initializer_body(&statement[initializer_start..])?;
    Some(ParsedLoweredLazyInitializer {
        binding: BindingName::new(binding),
        body,
    })
}

pub(crate) fn parse_lowered_lazy_initializer_body(initializer: &str) -> Option<&str> {
    if !looks_like_lowered_lazy_initializer(initializer) {
        return None;
    }
    // Expected shape: `lazyValue(() => { BODY })`.
    let prefix = "lazyValue(";
    if !initializer.starts_with(prefix) {
        return None;
    }
    let mut cursor = prefix.len();
    cursor = skip_ws(initializer.as_bytes(), cursor);
    if initializer.as_bytes().get(cursor) != Some(&b'(') {
        return None;
    }
    cursor = skip_ws(initializer.as_bytes(), cursor + 1);
    if initializer.as_bytes().get(cursor) != Some(&b')') {
        return None;
    }
    cursor = skip_ws(initializer.as_bytes(), cursor + 1);
    cursor = expect_arrow(initializer.as_bytes(), cursor)?;
    cursor = skip_ws(initializer.as_bytes(), cursor);
    if initializer.as_bytes().get(cursor) != Some(&b'{') {
        return None;
    }
    let body_end = find_matching_brace(initializer, cursor)?;
    let after_body = skip_ws(initializer.as_bytes(), body_end + 1);
    if initializer.as_bytes().get(after_body) != Some(&b')') {
        return None;
    }
    Some(initializer[cursor + 1..body_end].trim())
}

pub(crate) fn pure_lazy_body_assignments(
    body: &str,
    writable_helpers: &BTreeSet<BindingName>,
) -> Option<Vec<(BindingName, String)>> {
    let mut assignments = Vec::new();
    let mut written = BTreeSet::new();
    for statement in top_level_statement_slices(body) {
        let statement = statement.trim().trim_end_matches(';').trim();
        if statement.is_empty() {
            continue;
        }
        let (target, after_target) = parse_identifier(statement, 0)?;
        let target = BindingName::new(target);
        if !writable_helpers.contains(&target) || !written.insert(target.clone()) {
            return None;
        }
        let equals = skip_ws(statement.as_bytes(), after_target);
        if statement.as_bytes().get(equals) != Some(&b'=')
            || statement.as_bytes().get(equals + 1) == Some(&b'=')
            || statement.as_bytes().get(equals + 1) == Some(&b'>')
        {
            return None;
        }
        let value_start = skip_ws(statement.as_bytes(), equals + 1);
        let value = statement[value_start..].trim();
        if !is_pure_initializer_expression(value) {
            return None;
        }
        assignments.push((target, value.to_string()));
    }
    Some(assignments)
}
