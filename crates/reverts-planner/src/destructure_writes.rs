//! Destructuring-assignment recognition and helper rewriting.
//!
//! Two purposes:
//!
//! 1. **Detection.** `object_destructuring_assignment_writes` and
//!    `array_destructuring_assignment_writes` walk a recovered source
//!    slice starting at `{` or `[` and decide whether it begins a
//!    destructuring-assignment pattern (not an object literal, not a
//!    member-access expression). They return the rhs end byte and the
//!    list of LHS bindings, so the implicit-global-writes walker can
//!    treat destructuring as writes to its component bindings.
//!
//! 2. **Rewriting.** `rewrite_object_destructuring_helper_writes` /
//!    `rewrite_array_destructuring_helper_writes` are the
//!    destructuring counterparts of
//!    `runtime_helper_writes::rewrite_runtime_helper_writes`. When a
//!    destructuring pattern writes to one or more helper bindings, the
//!    rewrite wraps the RHS in an IIFE that binds it once, then
//!    routes each helper write through `__reverts_set_X(_$t.key)` or
//!    `__reverts_set_X(_$t[i])`.
//!
//! Both rewrites preserve evaluation order (RHS evaluated once, before
//! any setter call) and return the original IIFE value so the
//! expression is still usable in expression position.
//!
//! `split_top_level_properties` is here because it's the shared
//! delimiter-aware comma splitter used by both pattern parsers — and
//! by quite a few other planner passes that need to walk a brace's
//! interior at depth 0.

use std::collections::BTreeSet;

use reverts_ir::BindingName;
use reverts_js::{
    is_ascii_identifier_continue as is_identifier_continue,
    is_ascii_identifier_start as is_identifier_start, is_js_keyword, skip_block_comment,
    skip_line_comment,
};

use crate::byte_lexer::{find_matching_brace, find_matching_bracket, skip_quoted, skip_ws};
use crate::identifiers::parse_identifier;
use crate::runtime_helper_writes::find_assignment_rhs_end;
use crate::statements::runtime_helper_setter_name;

pub(crate) fn object_destructuring_assignment_writes(
    source: &str,
    object_start: usize,
) -> Option<(usize, Vec<BindingName>)> {
    let object_end = find_matching_brace(source, object_start)?;
    let equals = skip_ws(source.as_bytes(), object_end + 1);
    if source.as_bytes().get(equals) != Some(&b'=') {
        return None;
    }
    let rhs_start = skip_ws(source.as_bytes(), equals + 1);
    let rhs_end = find_assignment_rhs_end(source, rhs_start);
    let bindings = parse_object_pattern_bindings(&source[object_start + 1..object_end])
        .into_iter()
        .map(|(_, binding)| binding)
        .collect::<Vec<_>>();
    (!bindings.is_empty()).then_some((rhs_end, bindings))
}

pub(crate) fn array_destructuring_assignment_writes(
    source: &str,
    array_start: usize,
) -> Option<(usize, Vec<BindingName>)> {
    if bracket_starts_member_access(source, array_start) {
        return None;
    }
    let array_end = find_matching_bracket(source, array_start)?;
    let equals = skip_ws(source.as_bytes(), array_end + 1);
    if source.as_bytes().get(equals) != Some(&b'=')
        || source.as_bytes().get(equals + 1) == Some(&b'=')
        || source.as_bytes().get(equals + 1) == Some(&b'>')
    {
        return None;
    }
    let rhs_start = skip_ws(source.as_bytes(), equals + 1);
    let rhs_end = find_assignment_rhs_end(source, rhs_start);
    let bindings = parse_array_pattern_bindings(&source[array_start + 1..array_end])
        .into_iter()
        .map(|(_, binding)| binding)
        .collect::<Vec<_>>();
    (!bindings.is_empty()).then_some((rhs_end, bindings))
}

pub(crate) fn rewrite_object_destructuring_helper_writes(
    source: &str,
    object_start: usize,
    helpers: &BTreeSet<BindingName>,
) -> Option<(usize, String)> {
    let object_end = find_matching_brace(source, object_start)?;
    let equals = skip_ws(source.as_bytes(), object_end + 1);
    if source.as_bytes().get(equals) != Some(&b'=') {
        return None;
    }
    let rhs_start = skip_ws(source.as_bytes(), equals + 1);
    let rhs_end = find_assignment_rhs_end(source, rhs_start);
    let setters = parse_object_pattern_bindings(&source[object_start + 1..object_end])
        .into_iter()
        .filter(|(_, binding)| helpers.contains(binding))
        .collect::<Vec<_>>();
    if setters.is_empty() {
        return None;
    }
    let rhs = &source[rhs_start..rhs_end];
    let assignments = setters
        .into_iter()
        .map(|(key, binding)| {
            format!(
                "{}(_$t{})",
                runtime_helper_setter_name(&binding),
                property_access_source(key.as_str())
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Some((
        rhs_end,
        format!("(() => {{ const _$t = {rhs}; {assignments}; return _$t; }})()"),
    ))
}

pub(crate) fn rewrite_array_destructuring_helper_writes(
    source: &str,
    array_start: usize,
    helpers: &BTreeSet<BindingName>,
) -> Option<(usize, String)> {
    if bracket_starts_member_access(source, array_start) {
        return None;
    }
    let array_end = find_matching_bracket(source, array_start)?;
    let equals = skip_ws(source.as_bytes(), array_end + 1);
    if source.as_bytes().get(equals) != Some(&b'=')
        || source.as_bytes().get(equals + 1) == Some(&b'=')
        || source.as_bytes().get(equals + 1) == Some(&b'>')
    {
        return None;
    }
    let rhs_start = skip_ws(source.as_bytes(), equals + 1);
    let rhs_end = find_assignment_rhs_end(source, rhs_start);
    let setters = parse_array_pattern_bindings(&source[array_start + 1..array_end])
        .into_iter()
        .filter(|(_, binding)| helpers.contains(binding))
        .collect::<Vec<_>>();
    if setters.is_empty() {
        return None;
    }
    let rhs = &source[rhs_start..rhs_end];
    let assignments = setters
        .into_iter()
        .map(|(index, binding)| format!("{}(_$t[{index}])", runtime_helper_setter_name(&binding)))
        .collect::<Vec<_>>()
        .join("; ");
    Some((
        rhs_end,
        format!("(() => {{ const _$t = {rhs}; {assignments}; return _$t; }})()"),
    ))
}

fn bracket_starts_member_access(source: &str, bracket_start: usize) -> bool {
    source
        .as_bytes()
        .get(..bracket_start)
        .and_then(|prefix| {
            prefix
                .iter()
                .rposition(|byte| !byte.is_ascii_whitespace())
                .map(|position| prefix[position])
        })
        .is_some_and(|previous| {
            is_identifier_continue(previous) || matches!(previous, b')' | b']' | b'}' | b'.')
        })
}

pub(crate) fn split_top_level_properties(source: &str) -> Vec<&str> {
    let mut properties = Vec::new();
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    let mut start = 0usize;
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
            b'(' | b'[' | b'{' => {
                depth += 1;
                cursor += 1;
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                cursor += 1;
            }
            b',' if depth == 0 => {
                let property = source[start..cursor].trim();
                if !property.is_empty() {
                    properties.push(property);
                }
                cursor += 1;
                start = cursor;
            }
            _ => cursor += 1,
        }
    }
    let property = source[start..].trim();
    if !property.is_empty() {
        properties.push(property);
    }
    properties
}

/// Local binding names introduced by an object destructuring pattern whose
/// interior (the text between `{` and `}`) is `pattern_interior`. Renamed
/// properties bind the alias (`{ key: local }` -> `local`); shorthand and rest
/// bind their own name.
pub(crate) fn object_pattern_binding_names(pattern_interior: &str) -> Vec<BindingName> {
    parse_object_pattern_bindings(pattern_interior)
        .into_iter()
        .map(|(_, binding)| binding)
        .collect()
}

/// Local binding names introduced by an array destructuring pattern whose
/// interior (the text between `[` and `]`) is `pattern_interior`.
pub(crate) fn array_pattern_binding_names(pattern_interior: &str) -> Vec<BindingName> {
    parse_array_pattern_bindings(pattern_interior)
        .into_iter()
        .map(|(_, binding)| binding)
        .collect()
}

fn parse_object_pattern_bindings(source: &str) -> Vec<(String, BindingName)> {
    split_top_level_properties(source)
        .into_iter()
        .filter_map(|property| {
            let property = property.trim();
            let (key, binding) = if let Some(colon) = property.find(':') {
                let key = property[..colon].trim().trim_matches(['"', '\'']);
                let binding = parse_pattern_binding_identifier(property[colon + 1..].trim())?;
                (key.to_string(), binding)
            } else {
                let binding = parse_pattern_binding_identifier(property)?;
                (binding.as_str().to_string(), binding)
            };
            Some((key, binding))
        })
        .collect()
}

fn parse_array_pattern_bindings(source: &str) -> Vec<(usize, BindingName)> {
    split_top_level_properties(source)
        .into_iter()
        .enumerate()
        .filter_map(|(index, element)| {
            let binding = parse_pattern_binding_identifier(element)?;
            Some((index, binding))
        })
        .collect()
}

fn parse_pattern_binding_identifier(source: &str) -> Option<BindingName> {
    let trimmed = source.trim();
    let target = trimmed
        .strip_prefix("...")
        .unwrap_or(trimmed)
        .split('=')
        .next()?
        .trim();
    let (binding, next) = parse_identifier(target, 0)?;
    if is_js_keyword(binding) || skip_ws(target.as_bytes(), next) != target.len() {
        return None;
    }
    Some(BindingName::new(binding))
}

fn property_access_source(key: &str) -> String {
    if key
        .as_bytes()
        .first()
        .is_some_and(|byte| is_identifier_start(*byte))
        && key.as_bytes()[1..]
            .iter()
            .all(|byte| is_identifier_continue(*byte))
        && !is_js_keyword(key)
    {
        format!(".{key}")
    } else {
        format!("[{key:?}]")
    }
}
