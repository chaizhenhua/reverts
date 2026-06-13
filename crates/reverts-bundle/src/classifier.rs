use std::path::Path;

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;
use reverts_analyze::CompilerKind;

use crate::classification::{BundleClassification, MarkedMetadata};
use crate::inner_module::{BundlerKind, InnerModule};

type DetectorFn =
    for<'p> fn(&'p oxc_ast::ast::Program<'p>, reverts_ir::ModuleId) -> Vec<InnerModule>;

/// Detect which bundler runtime fingerprint dominates a source file.
/// Returns `CompilerKind::Unknown` when no detector proves a bundled shape.
pub fn detect_kind_from_source(source: &str) -> Result<CompilerKind, String> {
    let alloc = Allocator::default();
    let parsed = parse_program(&alloc, source)?;
    let parent = reverts_ir::ModuleId(0);
    if !crate::detectors::webpack5::detect(&parsed.program, parent).is_empty() {
        return Ok(CompilerKind::Webpack);
    }
    if !crate::detectors::esbuild::detect_commonjs(&parsed.program, parent).is_empty()
        || !crate::detectors::esbuild::detect_esm(&parsed.program, parent).is_empty()
    {
        return Ok(CompilerKind::Esbuild);
    }
    if !crate::detectors::rollup_cjs::detect(&parsed.program, parent).is_empty() {
        return Ok(CompilerKind::Rollup);
    }
    Ok(CompilerKind::Unknown)
}

/// Classify a single source file. Invokes detectors in priority order
/// based on the detected `CompilerKind`; the first non-empty result wins.
///
/// `_path` is reserved for future path-heuristic gating (vendored
/// directories, `.bundle.js` markers).
pub fn classify(_path: &Path, source: &str) -> Result<BundleClassification, String> {
    let alloc = Allocator::default();
    let parsed = parse_program(&alloc, source)?;
    let parent = reverts_ir::ModuleId(0);

    let runs: &[(BundlerKind, DetectorFn)] = &[
        (
            BundlerKind::Esbuild,
            crate::detectors::esbuild::detect_commonjs,
        ),
        (BundlerKind::Esbuild, crate::detectors::esbuild::detect_esm),
        (BundlerKind::Webpack5, crate::detectors::webpack5::detect),
        (BundlerKind::RollupCjs, crate::detectors::rollup_cjs::detect),
    ];

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
        return Ok(BundleClassification::Marked(MarkedMetadata {
            inner_modules,
            detected_by: bundler,
        }));
    }
    Ok(BundleClassification::Plain)
}

fn parse_program<'a>(
    alloc: &'a Allocator,
    source: &'a str,
) -> Result<oxc_parser::ParserReturn<'a>, String> {
    let parsed = Parser::new(alloc, source, SourceType::default()).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        let diagnostics = parsed
            .errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        return Err(if diagnostics.is_empty() {
            "bundle classifier parse panicked".to_string()
        } else {
            diagnostics
        });
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_kind_recognises_webpack_runtime() {
        let src = r#"var __webpack_modules__ = {"./foo": () => {}};"#;
        assert_eq!(
            detect_kind_from_source(src).expect("parseable webpack fixture"),
            CompilerKind::Webpack
        );
    }

    #[test]
    fn detect_kind_recognises_esbuild_runtime() {
        let src = r#"var __commonJS=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports); var x = __commonJS({"a.js": () => {}});"#;
        assert_eq!(
            detect_kind_from_source(src).expect("parseable esbuild fixture"),
            CompilerKind::Esbuild
        );
    }

    #[test]
    fn detect_kind_is_unknown_for_plain_js() {
        let src = "function add(a, b) { return a + b; }";
        assert_eq!(
            detect_kind_from_source(src).expect("parseable plain fixture"),
            CompilerKind::Unknown
        );
    }

    #[test]
    fn classify_returns_error_when_parse_fails() {
        let src = "function bad( { )";
        assert!(classify(Path::new("bundle.js"), src).is_err());
    }

    #[test]
    fn classify_routes_esbuild_commonjs_to_marked() {
        let src = r#"
            var __commonJS=(A,Q)=>()=>(Q||A((Q={exports:{}}).exports,Q),Q.exports);
            var x = __commonJS({
                "node_modules/lodash/index.js": (e, m) => { m.exports = 1; }
            });
        "#;
        let result = classify(Path::new("bundle.js"), src).expect("parseable fixture");
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
        let result = classify(Path::new("bundle.js"), src).expect("parseable fixture");
        match result {
            BundleClassification::Marked(meta) => {
                assert_eq!(meta.detected_by, BundlerKind::Webpack5);
                assert_eq!(meta.inner_modules.len(), 1);
            }
            other => panic!("expected Marked, got {other:?}"),
        }
    }

    #[test]
    fn classify_returns_plain_when_no_detector_matches() {
        let src = "function plain() { return 1; }";
        assert_eq!(
            classify(Path::new("plain.js"), src).expect("parseable fixture"),
            BundleClassification::Plain
        );
    }
}
