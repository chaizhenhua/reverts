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

/// Match any of `var` / `let` / `const` whose leading byte is at
/// `start`. Returns `(keyword, keyword.len())` on a hit so the caller
/// can advance past it.
pub(crate) fn declaration_keyword_at(source: &str, start: usize) -> Option<(&'static str, usize)> {
    for keyword in ["var", "let", "const"] {
        if keyword_at(source, start, keyword) {
            return Some((keyword, keyword.len()));
        }
    }
    None
}

/// Like `declaration_keyword_at` but unconditionally probing position 0
/// of the slice — handy for "is this whole snippet a declaration?".
pub(crate) fn declaration_keyword_at_start(source: &str) -> Option<(&'static str, usize)> {
    ["var", "let", "const"]
        .into_iter()
        .find(|keyword| keyword_at(source, 0, keyword))
        .map(|keyword| (keyword, keyword.len()))
}

/// Find the *next* occurrence of any of `var` / `let` / `const` after
/// `from`, where the candidate is bounded by identifier boundaries on
/// both sides. Returns the earliest hit if multiple keywords match.
pub(crate) fn find_declaration_keyword(source: &str, from: usize) -> Option<(usize, &'static str)> {
    ["var", "let", "const"]
        .into_iter()
        .filter_map(|keyword| find_keyword(source, keyword, from).map(|index| (index, keyword)))
        .min_by_key(|(index, _)| *index)
}

/// Find the next byte offset at which `keyword` appears as a whole
/// identifier (not as a fragment of a larger one) starting from `from`.
pub(crate) fn find_keyword(source: &str, keyword: &str, from: usize) -> Option<usize> {
    let mut offset = from;
    while let Some(relative) = source[offset..].find(keyword) {
        let absolute = offset + relative;
        let before = absolute
            .checked_sub(1)
            .and_then(|index| source.as_bytes().get(index))
            .copied();
        let after = source.as_bytes().get(absolute + keyword.len()).copied();
        if before.is_none_or(|byte| !is_identifier_continue(byte))
            && after.is_none_or(|byte| !is_identifier_continue(byte))
        {
            return Some(absolute);
        }
        offset = absolute + keyword.len();
    }
    None
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
