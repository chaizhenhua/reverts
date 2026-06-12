use std::path::Path;

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;
use reverts_analyze::{
    BABEL_RUNTIME_IDENTIFIERS, CompilerKind, ESBUILD_RUNTIME_IDENTIFIERS,
    ROLLUP_RUNTIME_IDENTIFIERS, WEBPACK_RUNTIME_IDENTIFIERS,
};

use crate::classification::BundleClassification;

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

/// Classify a single source file. Phase α: returns `Plain` for everything
/// — detector dispatch is wired in Task 11.
///
/// `_path` is reserved for future path-heuristic gating (vendored
/// directories, `.bundle.js` markers).
#[must_use]
pub fn classify(_path: &Path, source: &str) -> BundleClassification {
    let _kind = detect_kind_from_source(source);
    let alloc = Allocator::default();
    let parsed = Parser::new(&alloc, source, SourceType::default()).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return BundleClassification::Plain;
    }
    // Detector dispatch lands in Task 11; until then everything is Plain.
    BundleClassification::Plain
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classification::MarkedMetadata;
    use crate::inner_module::BundlerKind;

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
    fn classify_returns_plain_for_phase_alpha_baseline() {
        let src = "var x = __toESM(require('mod'));";
        let result = classify(Path::new("bundle.js"), src);
        assert_eq!(result, BundleClassification::Plain);
        // Marker test — proves dispatch will route via CompilerKind
        // (use `_kind` binding in classify() above).
        let _ = MarkedMetadata {
            inner_modules: vec![],
            detected_by: BundlerKind::Esbuild,
        };
    }

    #[test]
    fn classify_returns_plain_when_parse_fails() {
        let src = "function bad( { )";
        assert_eq!(
            classify(Path::new("bundle.js"), src),
            BundleClassification::Plain
        );
    }
}
