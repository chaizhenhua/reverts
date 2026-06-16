//! Identifier-shape checks used by the planner's source rewriters.
//!
//! These helpers answer "is this slice a valid JS identifier?" and
//! "where does the next identifier end?" without going through OXC,
//! because the planner uses them in tight byte-walking loops over
//! already-rendered source. They rely on the ASCII identifier
//! classifiers from `reverts-js` so the planner and the parser agree
//! on what counts as an identifier byte.
//!
//! Anything more ambiguous (template literals, classes, expressions)
//! still routes through OXC; ADR 0001 holds.

use reverts_js::{
    is_ascii_identifier_continue as is_identifier_continue,
    is_ascii_identifier_start as is_identifier_start,
};

use crate::byte_lexer::skip_ws;

pub(crate) fn is_identifier_like(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && is_identifier_start(bytes[0])
        && bytes[1..].iter().all(|byte| is_identifier_continue(*byte))
}

pub(crate) fn parse_identifier(source: &str, start: usize) -> Option<(&str, usize)> {
    let bytes = source.as_bytes();
    let first = *bytes.get(start)?;
    if !is_identifier_start(first) {
        return None;
    }
    let mut end = start + 1;
    while end < bytes.len() && is_identifier_continue(bytes[end]) {
        end += 1;
    }
    Some((&source[start..end], end))
}

pub(crate) fn parse_identifier_after_keyword<'a>(
    source: &'a str,
    cursor: usize,
    keyword: &str,
) -> Option<(&'a str, usize)> {
    parse_identifier(source, skip_ws(source.as_bytes(), cursor + keyword.len()))
}

pub(crate) fn parse_identifier_after_function_keyword(
    source: &str,
    cursor: usize,
) -> Option<(&str, usize)> {
    let bytes = source.as_bytes();
    let mut cursor = skip_ws(bytes, cursor + "function".len());
    if bytes.get(cursor) == Some(&b'*') {
        cursor = skip_ws(bytes, cursor + 1);
    }
    parse_identifier(source, cursor)
}

/// Identifies binding names that the planner emits as synthetic scaffolding
/// (lazy wrap temporaries, cross-module setters, createRequire alias). Such
/// names must never be treated as user-defined bindings during import or
/// implicit-write analysis. `__reverts_*` covers module-scope synthetics
/// (setters, createRequire alias); `_$*` covers closure-local temporaries
/// inside lazy wraps and update/destructure lowerings.
pub(crate) fn is_planner_synthetic_binding(name: &str) -> bool {
    name.starts_with("__reverts_") || name.starts_with("_$")
}

pub(crate) fn keyword_at(source: &str, cursor: usize, keyword: &str) -> bool {
    source
        .get(cursor..)
        .is_some_and(|tail| tail.starts_with(keyword))
        && cursor
            .checked_sub(1)
            .and_then(|index| source.as_bytes().get(index))
            .is_none_or(|byte| !is_identifier_continue(*byte))
        && source
            .as_bytes()
            .get(cursor + keyword.len())
            .is_none_or(|byte| !is_identifier_continue(*byte))
}
