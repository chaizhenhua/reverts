//! Small byte-walking helpers for scanning JavaScript source text without a
//! full parser. Shared by the CommonJS export scanner, the static-import
//! target collectors, and the signature/string-anchor extractors.

use std::path::Path;

use reverts_js::{normalize_source_for_pipeline, parse_error_message};

#[must_use]
pub(crate) fn compact_ascii_ws(source: &str) -> String {
    source
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect()
}

#[must_use]
pub(crate) fn skip_ascii_ws(bytes: &[u8], mut cursor: usize) -> usize {
    while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
        cursor += 1;
    }
    cursor
}

#[must_use]
pub(crate) fn previous_non_ascii_ws(bytes: &[u8], before: usize) -> Option<u8> {
    let mut cursor = before.checked_sub(1)?;
    while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
        cursor = cursor.checked_sub(1)?;
    }
    bytes.get(cursor).copied()
}

#[must_use]
pub(crate) fn read_identifier_at(source: &str, start: usize) -> Option<&str> {
    let bytes = source.as_bytes();
    let first = *bytes.get(start)?;
    if !(first == b'_' || first == b'$' || first.is_ascii_alphabetic()) {
        return None;
    }
    let mut end = start + 1;
    while bytes
        .get(end)
        .is_some_and(|byte| *byte == b'_' || *byte == b'$' || byte.is_ascii_alphanumeric())
    {
        end += 1;
    }
    source.get(start..end)
}

#[must_use]
pub(crate) fn read_identifier_with_end_at(source: &str, start: usize) -> Option<(&str, usize)> {
    let identifier = read_identifier_at(source, start)?;
    Some((identifier, start + identifier.len()))
}

#[must_use]
pub(crate) fn read_quoted_string_at(source: &str, start: usize) -> Option<(String, usize)> {
    let quote = *source.as_bytes().get(start)?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
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
            return Some((out, start + 1 + offset + ch.len_utf8()));
        }
        out.push(ch);
    }
    None
}

#[must_use]
pub(crate) fn normalize_source(path: &str, source: &str) -> Result<String, String> {
    normalize_source_for_pipeline(source, Some(Path::new(path)))
        .map_err(|error| parse_error_message(&error, "source could not be parsed"))
}
