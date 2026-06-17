use std::collections::BTreeSet;

use reverts_ir::hash::fnv1a_hex as stable_hash;

use crate::source::exported_members::exported_members_from_source;
use crate::source::source_text::normalize_source;

#[must_use]
pub fn package_source_normalized_hash(path: &str, source: &str) -> Option<String> {
    normalize_source(path, source)
        .ok()
        .map(|normalized| stable_hash(normalized.as_bytes()))
}

#[must_use]
pub fn package_source_exported_members(path: &str, source: &str) -> BTreeSet<String> {
    exported_members_from_source(path, source)
}
