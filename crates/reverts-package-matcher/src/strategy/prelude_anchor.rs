//! Anchor a scope-hoisted bundle's eager "entry island" bindings to npm
//! package sources by function-ownership fingerprinting.
//!
//! esbuild scope-hoisting flattens many eagerly-evaluated modules — including
//! bundled third-party libraries — into one top-level scope with no per-module
//! wrapper. Minification then strips every boundary marker (`__esm`, `__export`,
//! comments), so these bindings never become model modules and the per-module
//! package matcher never sees them: they end up inlined into the single
//! synthesized entry-island file, inflating the naming denominator with library
//! code that has nothing to do with the application.
//!
//! This pass fingerprints each eager binding's snippet INDIVIDUALLY and runs the
//! existing function-ownership cascade ([`match_with_cascade`]) against package
//! sources, recovering which library each binding belongs to. The match is
//! minification-robust by construction: it relies on the cascade's
//! structural/anchored/feature tiers, never on raw source text, so renamed
//! identifiers and stripped whitespace do not defeat it.
//!
//! The result is per-binding, not per-module — exactly the granularity the
//! flattened island needs. Downstream passes consume these anchors to drop
//! library bindings from the naming denominator and, eventually, to externalize
//! them as `import … from 'pkg'`.

use std::collections::BTreeMap;

use reverts_graph::{FunctionExtractor, RuntimePrelude, RuntimePreludeBindingKind};
use reverts_input::AttributionConfidence;
use reverts_ir::{ByteRange, FunctionFingerprint, ModuleId};

use super::cascade_match::{CascadeOwnershipMatch, match_with_cascade};
use crate::PackageSource;

/// One eager prelude binding to anchor: its name, the source slice of its
/// initializer, and the absolute byte offset of that slice within the original
/// bundle source (so recovered spans can be re-based onto the bundle).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreludeBindingSource {
    /// Bundle-local binding name (typically minified, e.g. `Cb`).
    pub binding: String,
    /// Absolute byte offset of `source` within the original bundle source file.
    pub byte_start: u32,
    /// The binding's initializer/declaration source slice.
    pub source: String,
}

/// A package anchor recovered for a single eager prelude binding.
#[derive(Debug, Clone, PartialEq)]
pub struct PreludeBindingAnchor {
    /// The originating bundle-local binding name.
    pub binding: String,
    pub package_name: String,
    pub package_version: String,
    pub export_specifier: String,
    /// Absolute byte range of the matched function within the bundle source.
    pub function_span: ByteRange,
    pub confidence: AttributionConfidence,
    /// Whether the matched package source is safe to emit as an external import
    /// (vs. source-only ownership evidence).
    pub external_importable: bool,
}

/// Collect a runtime prelude's eager (`SourceBacked`) bindings as anchor inputs.
///
/// Lazy-initializer (`__esm`) and CommonJS-wrapper bindings are skipped: those
/// carry their own boundary marker and already become model modules the
/// per-module matcher handles. Only the eager, scope-hoisted `SourceBacked`
/// bindings are flattened into the entry island with no module of their own, so
/// they are the ones that need per-binding anchoring. A binding without a
/// recorded snippet (no source slice to fingerprint) is skipped.
#[must_use]
pub fn prelude_binding_sources(prelude: &RuntimePrelude) -> Vec<PreludeBindingSource> {
    prelude
        .bindings
        .iter()
        .filter(|(_, kind)| matches!(kind, RuntimePreludeBindingKind::SourceBacked))
        .filter_map(|(name, _)| {
            let snippet = prelude.snippets.get(name)?;
            Some(PreludeBindingSource {
                binding: name.as_str().to_string(),
                byte_start: snippet.byte_start,
                source: snippet.source.clone(),
            })
        })
        .collect()
}

/// Fingerprint each eager prelude binding and anchor it to a package source via
/// the function-ownership cascade.
///
/// Each binding is fingerprinted under its own synthetic [`ModuleId`] (its index
/// in `bindings`) so the cascade keeps bindings independent and so an ownership
/// match can be mapped back to the originating binding by that index. A binding
/// whose snippet parses to no function, or whose functions match no package,
/// simply produces no anchor.
#[must_use]
pub fn anchor_prelude_bindings(
    bindings: &[PreludeBindingSource],
    package_sources: &[PackageSource],
) -> Vec<PreludeBindingAnchor> {
    if bindings.is_empty() || package_sources.is_empty() {
        return Vec::new();
    }

    let mut subjects: BTreeMap<ModuleId, Vec<FunctionFingerprint>> = BTreeMap::new();
    for (index, binding) in bindings.iter().enumerate() {
        let module_id = ModuleId(index as u32);
        let fingerprints = FunctionExtractor::fingerprint(module_id, &binding.source);
        if !fingerprints.is_empty() {
            subjects.insert(module_id, fingerprints);
        }
    }
    if subjects.is_empty() {
        return Vec::new();
    }

    let report = match_with_cascade(&subjects, package_sources);
    report
        .ownership_matches
        .into_iter()
        .filter_map(|ownership| anchor_from_ownership(bindings, ownership))
        .collect()
}

/// Map one cascade ownership match back to its originating binding and re-base
/// its span onto the bundle.
fn anchor_from_ownership(
    bindings: &[PreludeBindingSource],
    ownership: CascadeOwnershipMatch,
) -> Option<PreludeBindingAnchor> {
    let binding = bindings.get(ownership.module_id.0 as usize)?;
    // The cascade reports the span relative to the snippet source we fed it.
    // Eager prelude bindings are top-level declarations (`var X = …`,
    // `function X(){}`), never a brace-wrapped block, so the extractor's
    // outer-brace stripping never fires and the snippet-relative offset is exact
    // — re-base it onto the bundle by adding the binding's absolute offset.
    let function_span = ByteRange::new(
        binding.byte_start + ownership.function_span.start,
        binding.byte_start + ownership.function_span.end,
    );
    Some(PreludeBindingAnchor {
        binding: binding.binding.clone(),
        package_name: ownership.package_name,
        package_version: ownership.package_version,
        export_specifier: ownership.export_specifier,
        function_span,
        confidence: ownership.confidence,
        external_importable: ownership.external_importable,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use reverts_graph::{RuntimePreludeBindingKind, RuntimePreludeSnippet};
    use reverts_ir::BindingName;

    use super::*;

    fn snippet(source: &str, byte_start: u32) -> RuntimePreludeSnippet {
        RuntimePreludeSnippet {
            source: source.to_string(),
            byte_start,
            sub_snippets: Vec::new(),
            augmentations: Vec::new(),
        }
    }

    #[test]
    fn prelude_binding_sources_selects_only_eager_source_backed_bindings() {
        let mut bindings = BTreeMap::new();
        bindings.insert(
            BindingName::new("Cb"),
            RuntimePreludeBindingKind::SourceBacked,
        );
        bindings.insert(
            BindingName::new("lazyMod"),
            RuntimePreludeBindingKind::LazyInitializer,
        );
        bindings.insert(
            BindingName::new("cjsMod"),
            RuntimePreludeBindingKind::CommonJsWrapper,
        );

        let mut snippets = BTreeMap::new();
        snippets.insert(BindingName::new("Cb"), snippet("var Cb = 1;", 4096));
        snippets.insert(
            BindingName::new("lazyMod"),
            snippet("var lazyMod = nt(() => {});", 8192),
        );

        let prelude = RuntimePrelude {
            source_file_id: 1,
            source_file_path: "bundle.js".to_string(),
            source: String::new(),
            bindings,
            snippets,
            namespace_exports: Vec::new(),
            entrypoint: None,
        };

        let sources = prelude_binding_sources(&prelude);

        assert_eq!(sources.len(), 1, "only the eager binding: {sources:?}");
        assert_eq!(sources[0].binding, "Cb");
        assert_eq!(sources[0].byte_start, 4096);
        assert_eq!(sources[0].source, "var Cb = 1;");
    }

    /// A distinctive function body shared between an eager-inlined library
    /// binding and the package source. The bodies are byte-identical here, but
    /// the cascade matches on structural/anchored axes, so identifier renaming
    /// or reformatting would match just as well — this fixture abstracts the
    /// failure mode (a bundled library function flattened into the island) into
    /// the minimal shape the anchor pass must recognize.
    const LIBRARY_FUNCTION: &str = r#"function parseDuration(input) {
        const match = String(input).match(/^(\d+)(ms|s|m|h)$/);
        if (!match) { throw new TypeError("invalid duration: " + input); }
        const value = Number(match[1]);
        switch (match[2]) {
            case "ms": return value;
            case "s": return value * 1000;
            case "m": return value * 60000;
            case "h": return value * 3600000;
            default: return value;
        }
    }"#;

    fn library_package_source() -> PackageSource {
        PackageSource::external(
            "duration-utils",
            "2.1.0",
            "duration-utils",
            "duration-utils@2.1.0/index.js",
            LIBRARY_FUNCTION,
        )
    }

    #[test]
    fn anchors_an_eager_library_binding_to_its_package() {
        let bindings = vec![PreludeBindingSource {
            binding: "Cb".to_string(),
            byte_start: 4096,
            // The island inlines the library function as a binding initializer.
            source: format!("var Cb = {LIBRARY_FUNCTION};"),
        }];

        let anchors = anchor_prelude_bindings(&bindings, &[library_package_source()]);

        assert_eq!(anchors.len(), 1, "expected one anchor: {anchors:?}");
        let anchor = &anchors[0];
        assert_eq!(anchor.binding, "Cb");
        assert_eq!(anchor.package_name, "duration-utils");
        assert_eq!(anchor.export_specifier, "duration-utils");
        assert!(anchor.external_importable);
        // Span re-based onto the bundle: starts at/after the binding's offset.
        assert!(
            anchor.function_span.start >= 4096,
            "span not re-based onto the bundle: {:?}",
            anchor.function_span
        );
    }

    #[test]
    fn leaves_a_genuine_application_binding_unanchored() {
        let app_binding = PreludeBindingSource {
            binding: "render".to_string(),
            byte_start: 0,
            source: "var render = function(node) { \
                return node.children.map(child => child.id).join(\",\"); \
            };"
            .to_string(),
        };

        let anchors = anchor_prelude_bindings(&[app_binding], &[library_package_source()]);

        assert!(
            anchors.is_empty(),
            "application binding must not anchor to an unrelated package: {anchors:?}"
        );
    }

    #[test]
    fn no_package_sources_yields_no_anchors() {
        let bindings = vec![PreludeBindingSource {
            binding: "Cb".to_string(),
            byte_start: 0,
            source: format!("var Cb = {LIBRARY_FUNCTION};"),
        }];
        assert!(anchor_prelude_bindings(&bindings, &[]).is_empty());
    }
}
