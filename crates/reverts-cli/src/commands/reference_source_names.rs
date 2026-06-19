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

use std::path::Path;

const SOURCE_EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"];
const SKIP_DIRS: &[&str] = &["node_modules", "test", "tests", "__tests__", "coverage"];

pub(crate) fn build_reference_source_index(
    root: &Path,
    version: &str,
) -> Result<ReferenceSourceIndex, String> {
    let mut files = Vec::new();
    collect_source_files(root, root, &mut files)?;
    files.sort();
    let mut modules = Vec::new();
    for absolute in files {
        let relative = absolute
            .strip_prefix(root)
            .map_err(|error| error.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        let source = std::fs::read_to_string(&absolute)
            .map_err(|error| format!("read {}: {error}", absolute.display()))?;
        let Ok(fingerprint) = fingerprint_source(relative.as_str(), source.as_str()) else {
            continue; // unparseable reference file — skip, do not guess
        };
        let (export_names, asset_literals) = classify_anchors(&fingerprint);
        modules.push(ReferenceSourceModule {
            file_path: relative,
            fingerprint,
            export_names,
            asset_literals,
        });
    }
    Ok(ReferenceSourceIndex {
        version: version.to_string(),
        modules,
    })
}

fn collect_source_files(
    root: &Path,
    dir: &Path,
    out: &mut Vec<std::path::PathBuf>,
) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|error| format!("read_dir {}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        let name = entry.file_name().to_string_lossy().to_string();
        if file_type.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            collect_source_files(root, &path, out)?;
        } else if file_type.is_file() {
            if name.ends_with(".d.ts") {
                continue;
            }
            let is_source = path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext));
            if is_source {
                out.push(path);
            }
        }
    }
    Ok(())
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
    fn build_index_fingerprints_source_files_and_skips_dts_and_node_modules() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path();
        std::fs::create_dir_all(root.join("features")).expect("mkdir");
        std::fs::create_dir_all(root.join("node_modules/x")).expect("mkdir");
        std::fs::write(
            root.join("features/audio-capture.ts"),
            "export var _HL = require('/$bunfs/root/audio-capture.node');",
        )
        .expect("write ts");
        std::fs::write(root.join("features/types.d.ts"), "export type T = number;")
            .expect("write dts");
        std::fs::write(root.join("node_modules/x/index.js"), "module.exports = 1;")
            .expect("write nm");

        let index = build_reference_source_index(root, "2.1.76").expect("index");
        assert_eq!(index.version, "2.1.76");
        let paths: Vec<&str> = index.modules.iter().map(|m| m.file_path.as_str()).collect();
        assert_eq!(paths, vec!["features/audio-capture.ts"]);
        assert!(
            index.modules[0]
                .asset_literals
                .contains("/$bunfs/root/audio-capture.node")
        );
    }

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
