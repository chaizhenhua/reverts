//! Small byte-walking helpers for scanning JavaScript source text without a
//! full parser. Shared by the CommonJS export scanner, the static-import
//! target collectors, and the signature/string-anchor extractors.

use std::path::Path;

use reverts_js::{normalize_source_for_pipeline, parse_error_message};
pub(crate) use reverts_js::{
    read_identifier_at, read_identifier_with_end_at, read_quoted_string_at,
};

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

pub(crate) fn normalize_source(path: &str, source: &str) -> Result<String, String> {
    normalize_source_for_pipeline(source, Some(Path::new(path)))
        .map_err(|error| parse_error_message(&error, "source could not be parsed"))
}
