use std::collections::BTreeSet;

use reverts_ir::hash::fnv1a_hex as stable_hash;
use reverts_js::normalize::{apply_to_source, stable_passes};

use crate::source::exported_members::exported_members_from_source;
use crate::source::source_text::normalize_source;

const NORMALIZED_SOURCE_ALTERNATE_MAX_BYTES: usize = 64 * 1024;

#[must_use]
pub fn package_source_normalized_hash(path: &str, source: &str) -> Option<String> {
    normalize_source(path, source)
        .ok()
        .map(|normalized| stable_hash(normalized.as_bytes()))
}

#[must_use]
pub fn package_source_normalized_hashes(path: &str, source: &str) -> BTreeSet<String> {
    let Ok(normalized) = normalize_source(path, source) else {
        return BTreeSet::new();
    };
    let mut hashes = BTreeSet::new();
    hashes.insert(stable_hash(normalized.as_bytes()));
    if normalized.len() > NORMALIZED_SOURCE_ALTERNATE_MAX_BYTES {
        return hashes;
    }
    for pass in stable_passes() {
        let Ok(transformed) = apply_to_source(pass.as_ref(), normalized.as_str()) else {
            continue;
        };
        let Ok(renormalized) = normalize_source(path, transformed.as_str()) else {
            continue;
        };
        hashes.insert(stable_hash(renormalized.as_bytes()));
    }
    hashes
}

#[must_use]
pub fn package_source_exported_members(path: &str, source: &str) -> BTreeSet<String> {
    exported_members_from_source(path, source)
}
