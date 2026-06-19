//! `reference-source-names` command: name a decompiled project's modules,
//! exports, and local bindings by matching its emitted TypeScript against a
//! historical first-party source tree. Tier-gated: only provable matches are
//! auto-accepted; everything else is left for an agent.

use std::collections::BTreeSet;

use reverts_package_matcher::{SourceFingerprint, fingerprint_source};

/// One source file from the reference tree, fingerprinted for matching.
#[derive(Debug, Clone)]
pub(crate) struct ReferenceSourceModule {
    /// Path relative to the source root, e.g. `features/audio-capture.ts`.
    pub file_path: String,
    pub fingerprint: SourceFingerprint,
    /// Exported member names (from `export:` anchors).
    pub export_names: BTreeSet<String>,
    /// Native-asset literals referenced (string anchors ending in `.node`).
    pub asset_literals: BTreeSet<String>,
}

/// In-memory index over a reference source tree. Not persisted.
#[derive(Debug, Clone)]
pub(crate) struct ReferenceSourceIndex {
    pub version: String,
    pub modules: Vec<ReferenceSourceModule>,
}

fn classify_anchors(fingerprint: &SourceFingerprint) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut exports = BTreeSet::new();
    let mut assets = BTreeSet::new();
    for anchor in &fingerprint.string_anchors {
        if let Some(name) = anchor.strip_prefix("export:") {
            exports.insert(name.to_string());
        } else if anchor.ends_with(".node") {
            assets.insert(anchor.clone());
        }
    }
    (exports, assets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_anchors_splits_exports_and_native_assets() {
        // `_HL` normalizes to "hl" (2 chars) and is filtered by
        // `is_specific_export_member` (requires >= 3 normalized chars), so it
        // is never stored as an `export:` anchor.  Use `captureAudio` instead,
        // which normalises to "captureaudio" (12 chars) and passes all filters.
        let fingerprint = fingerprint_source(
            "features/audio-capture.ts",
            "export var captureAudio = require('/$bunfs/root/audio-capture.node');",
        )
        .expect("fingerprint");
        let (exports, assets) = classify_anchors(&fingerprint);
        assert!(exports.contains("captureAudio"), "exports: {exports:?}");
        assert!(
            assets.contains("/$bunfs/root/audio-capture.node"),
            "assets: {assets:?}"
        );
    }
}
