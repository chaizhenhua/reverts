use std::path::Path;

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;
use reverts_analyze::CompilerKind;
use reverts_ir::ByteRange;

use crate::classification::{BundleClassification, MarkedMetadata};
use crate::inner_module::{BundlerKind, InnerModule};

type DetectorFn =
    for<'p> fn(&'p str, &'p oxc_ast::ast::Program<'p>, reverts_ir::ModuleId) -> Vec<InnerModule>;

/// Detect which bundler runtime fingerprint dominates a source file.
/// Returns `CompilerKind::Unknown` when no detector proves a bundled shape.
pub fn detect_kind_from_source(source: &str) -> Result<CompilerKind, String> {
    let alloc = Allocator::default();
    let parsed = parse_program(&alloc, source)?;
    let parent = reverts_ir::ModuleId(0);
    if !crate::detectors::webpack5::detect(source, &parsed.program, parent).is_empty() {
        return Ok(CompilerKind::Webpack);
    }
    if !crate::detectors::esbuild::detect_commonjs(source, &parsed.program, parent).is_empty()
        || !crate::detectors::esbuild::detect_esm(source, &parsed.program, parent).is_empty()
    {
        return Ok(CompilerKind::Esbuild);
    }
    if !crate::detectors::rollup_cjs::detect(source, &parsed.program, parent).is_empty() {
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
    match classify_with_offset(source, 0) {
        Ok(BundleClassification::Plain) => classify_bun_cjs_wrapper(source),
        Ok(marked) => Ok(marked),
        Err(full_source_error) => {
            let wrapper = classify_bun_cjs_wrapper(source)?;
            if matches!(wrapper, BundleClassification::Plain) {
                Err(full_source_error)
            } else {
                Ok(wrapper)
            }
        }
    }
}

fn classify_with_offset(source: &str, offset: u32) -> Result<BundleClassification, String> {
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
        let inner_modules = detector(source, &parsed.program, parent);
        if !inner_modules.is_empty() {
            groups.entry(*bundler).or_default().extend(inner_modules);
        }
    }
    if let Some((bundler, mut inner_modules)) = groups.into_iter().max_by_key(|(_, m)| m.len()) {
        if offset != 0 {
            for inner in &mut inner_modules {
                inner.body_span = reverts_ir::ByteRange::new(
                    inner.body_span.start.saturating_add(offset),
                    inner.body_span.end.saturating_add(offset),
                );
            }
        }
        return Ok(BundleClassification::Marked(MarkedMetadata {
            inner_modules,
            detected_by: bundler,
        }));
    }
    Ok(BundleClassification::Plain)
}

/// Bun standalone executables wrap the JavaScript entrypoint in a CommonJS
/// function:
///
/// `(function(exports, require, module, __filename, __dirname) { ... })`
///
/// The esbuild helper declarations and module thunks live inside that function
/// body, so the normal top-level detector sees only one expression statement.
/// Re-parse just the wrapper body as a program and shift extracted spans back to
/// the original file. This is still AST-first for module discovery; the byte
/// scan only locates Bun's outer transport wrapper.
fn classify_bun_cjs_wrapper(source: &str) -> Result<BundleClassification, String> {
    let Some((body_start, body_end)) = bun_cjs_wrapper_body_span(source) else {
        return Ok(BundleClassification::Plain);
    };
    let Some(body) = source.get(body_start..body_end) else {
        return Ok(BundleClassification::Plain);
    };
    let ast_classification = classify_with_offset(
        body,
        u32::try_from(body_start).map_err(|_| "Bun wrapper body offset exceeds u32")?,
    );
    if let Ok(ast_classification) = ast_classification
        && !matches!(ast_classification, BundleClassification::Plain)
    {
        return Ok(ast_classification);
    }
    Ok(classify_bun_minified_esbuild_commonjs_by_scan(
        body, body_start,
    ))
}

fn bun_cjs_wrapper_body_span(source: &str) -> Option<(usize, usize)> {
    let trimmed_start = source.find("(function(")?;
    if !source[trimmed_start..]
        .starts_with("(function(exports, require, module, __filename, __dirname)")
    {
        return None;
    }
    let open_rel = source[trimmed_start..].find('{')?;
    let body_start = trimmed_start + open_rel + 1;
    // Bun's wrapper is the outer transport wrapper. The generated file may have
    // braces in strings/comments, so use the known trailer instead of a brace
    // counter that would duplicate JS lexing.
    let trailer = "\n})";
    let body_end = source.rfind(trailer).or_else(|| source.rfind("})"))?;
    (body_end > body_start).then_some((body_start, body_end))
}

fn classify_bun_minified_esbuild_commonjs_by_scan(
    body: &str,
    body_offset: usize,
) -> BundleClassification {
    let aliases = minified_commonjs_aliases(body);
    if aliases.is_empty() {
        return BundleClassification::Plain;
    }
    let mut inner_modules = Vec::new();
    for alias in aliases {
        inner_modules.extend(scan_minified_commonjs_handle_modules(
            body,
            body_offset,
            alias.as_str(),
        ));
    }
    if inner_modules.is_empty() {
        BundleClassification::Plain
    } else {
        BundleClassification::Marked(MarkedMetadata {
            inner_modules,
            detected_by: BundlerKind::Esbuild,
        })
    }
}

fn minified_commonjs_aliases(source: &str) -> Vec<String> {
    let mut aliases = Vec::new();
    let mut search_from = 0;
    while let Some(rel) = source[search_from..].find("=>()=>") {
        let arrow = search_from + rel;
        let Some(eq) = source[..arrow].rfind('=') else {
            search_from = arrow + 1;
            continue;
        };
        let name_end = source[..eq].trim_end().len();
        let Some(name_start) = identifier_start_before(source, name_end) else {
            search_from = arrow + 1;
            continue;
        };
        let name = &source[name_start..name_end];
        let tail = &source[arrow..source.len().min(arrow + 180)];
        if is_identifier(name) && tail.contains(".exports") && !aliases.iter().any(|a| a == name) {
            aliases.push(name.to_string());
        }
        search_from = arrow + 1;
    }
    aliases
}

fn scan_minified_commonjs_handle_modules(
    source: &str,
    source_offset: usize,
    alias: &str,
) -> Vec<InnerModule> {
    let needle = "var ";
    let call_fragment = format!("={alias}((");
    let mut modules = Vec::new();
    let mut search_from = 0;
    while let Some(var_rel) = source[search_from..].find(needle) {
        let var_start = search_from + var_rel;
        let decl_start = var_start + needle.len();
        let Some(name_end) = identifier_end_at(source, decl_start) else {
            search_from = decl_start;
            continue;
        };
        let name = &source[decl_start..name_end];
        let after_name = skip_ascii_ws(source, name_end);
        if !source[after_name..].starts_with(call_fragment.as_str()) {
            search_from = decl_start;
            continue;
        }
        let call_open = after_name + 1 + alias.len();
        let Some(call_end) = balanced_call_end(source, call_open) else {
            search_from = decl_start;
            continue;
        };
        let statement_end = source[call_end..]
            .find(';')
            .map(|rel| call_end + rel + 1)
            .unwrap_or(call_end);
        let Ok(start) = u32::try_from(source_offset + var_start) else {
            search_from = decl_start;
            continue;
        };
        let Ok(end) = u32::try_from(source_offset + statement_end) else {
            search_from = decl_start;
            continue;
        };
        let module_source = &source[var_start..statement_end];
        if !is_parseable_module_slice(module_source) {
            search_from = statement_end;
            continue;
        }
        let synthetic_source = format!(
            "var {alias}=(H,$)=>()=>($||H(($={{exports:{{}}}}).exports,$),$.exports);\n{module_source}"
        );
        modules.push(InnerModule {
            virtual_id: format!("esbuild:{name}"),
            body_span: ByteRange::new(start, end),
            bundler: BundlerKind::Esbuild,
            source_path_hint: None,
            parent_module_id: reverts_ir::ModuleId(0),
            synthetic_source: Some(synthetic_source),
        });
        search_from = statement_end;
    }
    modules
}

fn is_parseable_module_slice(source: &str) -> bool {
    let alloc = Allocator::default();
    let parsed = Parser::new(&alloc, source, SourceType::default()).parse();
    !parsed.panicked && parsed.errors.is_empty()
}

fn identifier_start_before(source: &str, end: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut start = end;
    while start > 0 {
        let c = bytes[start - 1];
        if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' {
            start -= 1;
        } else {
            break;
        }
    }
    (start < end).then_some(start)
}

fn identifier_end_at(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let first = *bytes.get(start)?;
    if !(first.is_ascii_alphabetic() || first == b'_' || first == b'$') {
        return None;
    }
    let mut end = start + 1;
    while end < bytes.len() {
        let c = bytes[end];
        if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' {
            end += 1;
        } else {
            break;
        }
    }
    Some(end)
}

fn skip_ascii_ws(source: &str, mut index: usize) -> usize {
    let bytes = source.as_bytes();
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    index
}

fn is_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

fn balanced_call_end(source: &str, open_paren: usize) -> Option<usize> {
    if source.as_bytes().get(open_paren) != Some(&b'(') {
        return None;
    }
    let bytes = source.as_bytes();
    let mut i = open_paren;
    let mut depth = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => i = skip_quoted(bytes, i, bytes[i])?,
            b'`' => i = skip_template(bytes, i)?,
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.checked_sub(1)?;
                i += 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => i += 1,
        }
    }
    None
}

fn skip_quoted(bytes: &[u8], quote: usize, delimiter: u8) -> Option<usize> {
    let mut i = quote + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
        } else if bytes[i] == delimiter {
            return Some(i + 1);
        } else {
            i += 1;
        }
    }
    None
}

fn skip_template(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
        } else if bytes[i] == b'`' {
            return Some(i + 1);
        } else {
            i += 1;
        }
    }
    None
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
    fn classify_bun_commonjs_transport_wrapper_extracts_inner_esbuild_modules() {
        let src = r#"(function(exports, require, module, __filename, __dirname) {
var o=(H,$)=>()=>($||H(($={exports:{}}).exports,$),$.exports);
var react=o((ex,mod)=>{ex.alpha=1;mod.exports=ex;});
})"#;

        let classified = classify(Path::new("cli.js"), src).expect("parseable Bun wrapper");
        let BundleClassification::Marked(marked) = classified else {
            panic!("expected marked bundle classification");
        };
        assert_eq!(marked.detected_by, BundlerKind::Esbuild);
        assert_eq!(marked.inner_modules.len(), 1);
        let inner = &marked.inner_modules[0];
        assert_eq!(inner.virtual_id, "esbuild:react");
        assert!(
            inner.body_span.start as usize > src.find('{').expect("wrapper body"),
            "span must be shifted into the original source"
        );
        assert!(
            src[inner.body_span.start as usize..inner.body_span.end as usize].contains("react=o")
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
