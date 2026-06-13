use std::path::Path;

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;
use reverts_analyze::{
    BABEL_RUNTIME_IDENTIFIERS, CompilerKind, ESBUILD_RUNTIME_IDENTIFIERS,
    ROLLUP_RUNTIME_IDENTIFIERS, WEBPACK_RUNTIME_IDENTIFIERS,
};

use crate::classification::{BundleClassification, MarkedMetadata};
use crate::inner_module::{BundlerKind, InnerModule};

type DetectorFn =
    for<'p> fn(&'p oxc_ast::ast::Program<'p>, reverts_ir::ModuleId) -> Vec<InnerModule>;

/// Detect which bundler runtime fingerprint dominates a source file.
/// Returns `CompilerKind::Unknown` when no runtime identifier matches —
/// callers should treat that as `Plain`.
#[must_use]
pub fn detect_kind_from_source(source: &str) -> CompilerKind {
    let probe = |needles: &[&'static str]| needles.iter().any(|n| source.contains(*n));
    if probe(WEBPACK_RUNTIME_IDENTIFIERS) {
        CompilerKind::Webpack
    } else if probe(ESBUILD_RUNTIME_IDENTIFIERS) {
        CompilerKind::Esbuild
    } else if probe(ROLLUP_RUNTIME_IDENTIFIERS) {
        CompilerKind::Rollup
    } else if probe(BABEL_RUNTIME_IDENTIFIERS) {
        CompilerKind::Babel
    } else {
        CompilerKind::Unknown
    }
}

/// Classify a single source file. Invokes detectors in priority order
/// based on the detected `CompilerKind`; the first non-empty result wins.
///
/// `_path` is reserved for future path-heuristic gating (vendored
/// directories, `.bundle.js` markers).
#[must_use]
pub fn classify(_path: &Path, source: &str) -> BundleClassification {
    let alloc = Allocator::default();
    let parsed = Parser::new(&alloc, source, SourceType::default()).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return BundleClassification::Plain;
    }

    let kind = detect_kind_from_source(source);
    let parent = reverts_ir::ModuleId(0);

    // Priority order: prefer detectors aligned with the detected kind.
    let runs: &[(BundlerKind, DetectorFn)] = match kind {
        CompilerKind::Esbuild => &[
            (
                BundlerKind::Esbuild,
                crate::detectors::esbuild::detect_commonjs,
            ),
            (BundlerKind::Esbuild, crate::detectors::esbuild::detect_esm),
        ],
        CompilerKind::Webpack => &[(BundlerKind::Webpack5, crate::detectors::webpack5::detect)],
        CompilerKind::Rollup => &[(BundlerKind::RollupCjs, crate::detectors::rollup_cjs::detect)],
        // No-match kinds: still try every detector defensively so an
        // unannotated bundle that happens to use a recognisable shape
        // is still classified. Unknown kind here means "no runtime
        // identifier found"; the file may still register modules.
        _ => &[
            (
                BundlerKind::Esbuild,
                crate::detectors::esbuild::detect_commonjs,
            ),
            (BundlerKind::Esbuild, crate::detectors::esbuild::detect_esm),
            (BundlerKind::Webpack5, crate::detectors::webpack5::detect),
            (BundlerKind::RollupCjs, crate::detectors::rollup_cjs::detect),
        ],
    };

    // Run every detector in the schedule and accumulate matches by
    // bundler. A single bundle can register modules through more than
    // one mechanism (esbuild emits both `__commonJS` and `__esm`
    // registrations in the same file), so we MUST NOT return after the
    // first non-empty detector. The winning bundler is the one that
    // produced the largest set of inner modules.
    let mut groups: std::collections::BTreeMap<BundlerKind, Vec<InnerModule>> =
        std::collections::BTreeMap::new();
    for (bundler, detector) in runs {
        let inner_modules = detector(&parsed.program, parent);
        if !inner_modules.is_empty() {
            groups.entry(*bundler).or_default().extend(inner_modules);
        }
    }
    if let Some((bundler, inner_modules)) = groups.into_iter().max_by_key(|(_, m)| m.len()) {
        return BundleClassification::Marked(MarkedMetadata {
            inner_modules,
            detected_by: bundler,
        });
    }
    BundleClassification::Plain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_kind_recognises_webpack_runtime() {
        let src = "var x = __webpack_require__('./foo');";
        assert_eq!(detect_kind_from_source(src), CompilerKind::Webpack);
    }

    #[test]
    fn detect_kind_recognises_esbuild_runtime() {
        let src = "var pkg = __toESM(require('mod'));";
        assert_eq!(detect_kind_from_source(src), CompilerKind::Esbuild);
    }

    #[test]
    fn detect_kind_is_unknown_for_plain_js() {
        let src = "function add(a, b) { return a + b; }";
        assert_eq!(detect_kind_from_source(src), CompilerKind::Unknown);
    }

    #[test]
    fn classify_returns_plain_when_parse_fails() {
        let src = "function bad( { )";
        assert_eq!(
            classify(Path::new("bundle.js"), src),
            BundleClassification::Plain
        );
    }

    #[test]
    fn classify_routes_esbuild_commonjs_to_marked() {
        let src = r#"
            var __commonJS=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports);
            var x = __commonJS({
                "node_modules/lodash/index.js": (e, m) => { m.exports = 1; }
            });
        "#;
        let result = classify(Path::new("bundle.js"), src);
        match result {
            BundleClassification::Marked(meta) => {
                assert_eq!(meta.detected_by, BundlerKind::Esbuild);
                assert_eq!(meta.inner_modules.len(), 1);
                assert_eq!(
                    meta.inner_modules[0].source_path_hint.as_deref(),
                    Some("node_modules/lodash/index.js")
                );
            }
            other => panic!("expected Marked, got {other:?}"),
        }
    }

    #[test]
    fn classify_routes_webpack5_module_map_to_marked() {
        let src = r#"
            var __webpack_modules__ = {
                "./src/foo.js": (m, e, r) => { e.x = 1; }
            };
            __webpack_require__("./src/foo.js");
        "#;
        let result = classify(Path::new("bundle.js"), src);
        match result {
            BundleClassification::Marked(meta) => {
                assert_eq!(meta.detected_by, BundlerKind::Webpack5);
                assert_eq!(meta.inner_modules.len(), 1);
            }
            other => panic!("expected Marked, got {other:?}"),
        }
    }

    #[test]
    fn classify_falls_through_to_plain_when_no_detector_matches() {
        let src = "function plain() { return 1; }";
        assert_eq!(
            classify(Path::new("plain.js"), src),
            BundleClassification::Plain
        );
    }
}
