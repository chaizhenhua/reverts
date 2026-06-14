//! End-to-end cascade + variant + version + Hungarian pipeline against a slice
//! of [`PackageSource`]s.
//!
//! This function-level evidence engine is now orchestrated by
//! [`crate::match_packages_with_pipeline`]. It builds an in-memory fingerprint
//! index from the supplied package sources, runs the cascade tiers per bundle
//! function, lets the Hungarian assignment resolve cross-package collisions,
//! classifies each accepted match through [`acceptance::classify`], and emits
//! [`PackageAttributionInput`] rows with `function_span` + `confidence`
//! populated.

use std::collections::BTreeMap;

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;
use reverts_graph::FunctionExtractor;
use reverts_input::{AttributionConfidence, PackageAttributionInput};
use reverts_ir::{AxisHashes, AxisKind, ByteRange, FunctionFingerprint, FunctionId, ModuleId};
use reverts_js::parse_options_for;
use reverts_observe::{AuditFinding, AuditReport, FindingCode};
use reverts_package_index::{
    Candidate, CfgKey, ExactKey, FeatureKey, InMemoryFingerprintIndex, PackageId, StructuralKey,
};

use crate::PackageSource;
use crate::acceptance::{AcceptanceDecision, classify};
use crate::cascade::assign_globally;

/// Aggregate result of [`match_with_cascade`].
#[derive(Debug, Clone, PartialEq)]
pub struct CascadeMatchReport {
    /// Accepted attribution rows enriched with function span and confidence.
    pub attributions: Vec<PackageAttributionInput>,
    /// Accepted function-level ownership matches, including source-only
    /// package sources that are useful evidence but unsafe to emit as imports.
    pub ownership_matches: Vec<CascadeOwnershipMatch>,
    /// Findings produced during the match (parse failures, ambiguity, low
    /// confidence, etc.).
    pub audit: AuditReport,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CascadeOwnershipMatch {
    pub module_id: ModuleId,
    pub package_name: String,
    pub package_version: String,
    pub export_specifier: String,
    pub function_span: ByteRange,
    pub confidence: AttributionConfidence,
    pub external_importable: bool,
}

/// Runs the cascade + variant + version + Hungarian pipeline against the given
/// per-module bundle fingerprints, using `package_sources` as the right-hand
/// side index.
///
/// Public lower-level evidence engine. Most callers should prefer
/// [`crate::match_packages_with_pipeline`], which folds this evidence back into
/// module-level attributions/ownership matches.
#[must_use]
pub fn match_with_cascade(
    fingerprints_by_module: &BTreeMap<ModuleId, Vec<FunctionFingerprint>>,
    package_sources: &[PackageSource],
) -> CascadeMatchReport {
    let mut audit = AuditReport::default();
    let index = build_index(package_sources, &mut audit);

    let mut attributions = Vec::new();
    let mut ownership_matches = Vec::new();
    for (module_id, fps) in fingerprints_by_module {
        let assignments = assign_globally(fps, &index);
        for assignment in assignments {
            // Per-fp acceptance decision is computed from the full cascade
            // candidate list — including runner-ups — so the classifier can
            // see the margin between the top tier and the next-best. The
            // Hungarian-chosen candidate (which may not be the top of the
            // list, if cross-package collisions forced a swap) supplies the
            // actual attribution row.
            let decision = classify(&assignment.candidates);
            let fn_id = assignment.function_id;
            match decision {
                AcceptanceDecision::Accepted { confidence } => {
                    let Some(chosen) = assignment.chosen else {
                        continue;
                    };
                    let candidate = &chosen.candidate;
                    ownership_matches.push(build_ownership_match(
                        *module_id,
                        fn_id,
                        candidate,
                        confidence.clone(),
                    ));
                    if candidate.external_importable {
                        attributions
                            .push(build_attribution(*module_id, fn_id, candidate, confidence));
                    }
                }
                AcceptanceDecision::AcceptedWithCaveat { confidence } => {
                    let Some(chosen) = assignment.chosen else {
                        continue;
                    };
                    let cand = &chosen.candidate;
                    ownership_matches.push(build_ownership_match(
                        *module_id,
                        fn_id,
                        cand,
                        confidence.clone(),
                    ));
                    if cand.external_importable {
                        attributions.push(build_attribution(*module_id, fn_id, cand, confidence));
                    }
                    audit.push(
                        AuditFinding::warning(
                            FindingCode::LowConfidenceAttribution,
                            "cascade function attribution accepted with low margin",
                        )
                        .with_module(module_id.0.to_string())
                        .with_binding(cand.package.name.clone()),
                    );
                }
                AcceptanceDecision::Ambiguous { .. } => {
                    // Use the top candidate's package name for the binding
                    // tag; we are not emitting an attribution row because
                    // the matcher cannot pick decisively.
                    let binding = assignment
                        .candidates
                        .first()
                        .map(|m| m.candidate.package.name.clone())
                        .unwrap_or_default();
                    audit.push(
                        AuditFinding::error(
                            FindingCode::AmbiguousPackageMatch,
                            "cascade function match has ambiguous tier",
                        )
                        .with_module(module_id.0.to_string())
                        .with_binding(binding),
                    );
                }
                AcceptanceDecision::NoMatch => {}
            }
        }
    }

    CascadeMatchReport {
        attributions,
        ownership_matches,
        audit,
    }
}

fn build_index(
    package_sources: &[PackageSource],
    audit: &mut AuditReport,
) -> InMemoryFingerprintIndex {
    let mut index = InMemoryFingerprintIndex::new();
    for (idx, source) in package_sources.iter().enumerate() {
        if !source.is_within_fingerprint_budget() {
            continue;
        }
        let synthetic_module_id = ModuleId(u32::MAX - idx as u32);
        let alloc = Allocator::default();
        let source_type = SourceType::default().with_typescript(true).with_jsx(true);
        let parsed = Parser::new(&alloc, source.source.as_str(), source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if parsed.panicked || !parsed.errors.is_empty() {
            audit.push(
                AuditFinding::error(
                    FindingCode::UnparseablePackageSource,
                    "package source failed to parse for cascade index",
                )
                .with_module(source.source_path.clone())
                .with_binding(format!(
                    "{}@{}",
                    source.package_name, source.package_version
                )),
            );
            continue;
        }
        let pkg_fps = FunctionExtractor::fingerprint(synthetic_module_id, source.source.as_str());
        let pkg_id = PackageId {
            name: source.package_name.clone(),
            version: source.package_version.clone(),
        };

        for fp in pkg_fps {
            let external_id = encode_function_id(&fp.id);
            let base_candidate = Candidate {
                package: pkg_id.clone(),
                variant_path: source.export_specifier.clone(),
                external_function_id: external_id,
                matched_axis: AxisKind::Ast,
                matched_alternate: None,
                external_importable: source.external_importable,
            };

            // Exact key: primary AST hash.
            index.insert_exact(
                ExactKey {
                    param_count: fp.param_count,
                    statement_count: fp.statement_count,
                    ast_hash: fp.primary.ast,
                },
                base_candidate.clone(),
            );

            // Exact alternate keys: every alternate AST hash from normalization
            // passes. Each alternate carries its OWN statement_count so
            // passes that change the count (e.g. DeclaratorSplit) are
            // queryable under the post-pass count, not the primary one.
            for alt in &fp.alternates {
                let mut alt_cand = base_candidate.clone();
                alt_cand.matched_alternate = Some(alt.pass);
                index.insert_exact(
                    ExactKey {
                        param_count: fp.param_count,
                        statement_count: alt.statement_count,
                        ast_hash: alt.axes.ast,
                    },
                    alt_cand,
                );
            }

            // CFG key: primary control-flow hash.
            let mut cfg_cand = base_candidate.clone();
            cfg_cand.matched_axis = AxisKind::Cfg;
            index.insert_cfg(
                CfgKey {
                    param_count: fp.param_count,
                    cfg_hash: fp.primary.cfg,
                },
                cfg_cand,
            );

            // Feature keys for every axis where the primary fingerprint
            // carries a hash (Some(_) variants plus the always-present axes
            // that participate in feature similarity scoring).
            for (axis, hash) in axes_to_feature_keys(&fp.primary) {
                let mut feat_cand = base_candidate.clone();
                feat_cand.matched_axis = axis;
                index.insert_feature(
                    FeatureKey {
                        param_count: fp.param_count,
                        kind: axis,
                        hash,
                    },
                    feat_cand,
                );
            }

            // Structural key: primary structural anchor.
            let mut struct_cand = base_candidate.clone();
            struct_cand.matched_axis = AxisKind::StructuralAnchor;
            index.insert_structural(
                StructuralKey {
                    param_count: fp.param_count,
                    structural_anchor: fp.primary.structural_anchor,
                },
                struct_cand,
            );
        }
    }
    index
}

fn build_ownership_match(
    module_id: ModuleId,
    fn_id: FunctionId,
    candidate: &Candidate,
    confidence: AttributionConfidence,
) -> CascadeOwnershipMatch {
    CascadeOwnershipMatch {
        module_id,
        package_name: candidate.package.name.clone(),
        package_version: candidate.package.version.clone(),
        export_specifier: candidate.variant_path.clone(),
        function_span: fn_id.span,
        confidence,
        external_importable: candidate.external_importable,
    }
}

fn build_attribution(
    module_id: ModuleId,
    fn_id: FunctionId,
    candidate: &Candidate,
    confidence: AttributionConfidence,
) -> PackageAttributionInput {
    PackageAttributionInput::accepted_external(
        module_id,
        candidate.package.name.as_str(),
        candidate.package.version.as_str(),
        candidate.variant_path.as_str(),
    )
    .with_function_span(fn_id.span)
    .with_confidence(confidence)
}

/// Stable opaque identifier for a `FunctionId`, used to dedupe candidates in
/// the cascade index. FNV-1a over (module_id, span.start, span.end) bytes so
/// every component contributes uniformly and high module ids do not alias.
fn encode_function_id(fn_id: &FunctionId) -> u64 {
    let mut hash = reverts_ir::hash::FNV_OFFSET_BASIS;
    reverts_ir::hash::update_fnv1a(&mut hash, &fn_id.module_id.0.to_le_bytes());
    reverts_ir::hash::update_fnv1a(&mut hash, &fn_id.span.start.to_le_bytes());
    reverts_ir::hash::update_fnv1a(&mut hash, &fn_id.span.end.to_le_bytes());
    hash
}

/// Returns the (axis, hash) pairs the function-similarity tier will look up.
///
/// Includes both the always-present axes (return/effect/structural/binding)
/// and the `Option`al axes when populated, mirroring
/// [`crate::tier::collect_remaining_axes`] without the exclusion filter.
fn axes_to_feature_keys(axes: &AxisHashes) -> Vec<(AxisKind, u64)> {
    let mut out = vec![
        (AxisKind::ReturnPattern, axes.return_pattern),
        (AxisKind::EffectPattern, axes.effect_pattern),
        (AxisKind::StructuralAnchor, axes.structural_anchor),
        (AxisKind::BindingPattern, axes.binding_pattern),
    ];
    if let Some(h) = axes.literal_anchor {
        out.push((AxisKind::LiteralAnchor, h));
    }
    if let Some(h) = axes.callee_set {
        out.push((AxisKind::CalleeSet, h));
    }
    if let Some(h) = axes.throw_set {
        out.push((AxisKind::ThrowSet, h));
    }
    if let Some(h) = axes.access_pattern {
        out.push((AxisKind::AccessPattern, h));
    }
    if let Some(h) = axes.access_shape {
        out.push((AxisKind::AccessShape, h));
    }
    if let Some(h) = axes.literal_shape {
        out.push((AxisKind::LiteralShape, h));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use reverts_ir::ModuleId;

    #[test]
    fn cascade_match_attributes_exact_function_with_function_span() {
        let bundle_source = "function f(a, b) { return a + b; }";
        let bundle_fps = FunctionExtractor::fingerprint(ModuleId(10), bundle_source);
        assert!(
            !bundle_fps.is_empty(),
            "extractor must produce fingerprints for the bundle body",
        );

        let mut fps_map: BTreeMap<ModuleId, Vec<FunctionFingerprint>> = BTreeMap::new();
        fps_map.insert(ModuleId(10), bundle_fps);

        // Identical body verbatim — guarantees an Exact tier hit so the
        // pipeline accepts the attribution.
        let pkg_sources = [PackageSource::external(
            "pkg",
            "1.0.0",
            "pkg/add",
            "add.js",
            bundle_source,
        )];

        let report = match_with_cascade(&fps_map, &pkg_sources);

        assert!(
            !report.attributions.is_empty(),
            "expected at least one attribution, audit={:?}",
            report.audit.findings(),
        );
        let attr = &report.attributions[0];
        assert_eq!(attr.package_name, "pkg");
        assert_eq!(attr.package_version.as_deref(), Some("1.0.0"));
        assert_eq!(attr.export_specifier.as_deref(), Some("pkg/add"));
        assert!(attr.function_span.is_some(), "function_span must be set");
        assert!(attr.confidence.is_some(), "confidence must be set");
        assert_eq!(report.ownership_matches.len(), report.attributions.len());
        assert_eq!(report.ownership_matches[0].package_name, "pkg");
        assert_eq!(report.ownership_matches[0].package_version, "1.0.0");
        assert_eq!(report.ownership_matches[0].export_specifier, "pkg/add");
        assert!(report.ownership_matches[0].external_importable);
    }

    #[test]
    fn cascade_match_records_source_only_ownership_without_external_attribution() {
        let bundle_source = "function f(a, b) { return a + b; }";
        let bundle_fps = FunctionExtractor::fingerprint(ModuleId(10), bundle_source);
        let mut fps_map: BTreeMap<ModuleId, Vec<FunctionFingerprint>> = BTreeMap::new();
        fps_map.insert(ModuleId(10), bundle_fps);
        let pkg_sources = [PackageSource::source_only(
            "pkg",
            "1.0.0",
            "pkg/internal/add.js",
            "internal/add.js",
            bundle_source,
        )];

        let report = match_with_cascade(&fps_map, &pkg_sources);

        assert!(
            report.attributions.is_empty(),
            "source-only package roots must not emit external cascade attributions"
        );
        assert_eq!(report.ownership_matches.len(), 1);
        let ownership = &report.ownership_matches[0];
        assert_eq!(ownership.module_id, ModuleId(10));
        assert_eq!(ownership.package_name, "pkg");
        assert_eq!(ownership.package_version, "1.0.0");
        assert_eq!(ownership.export_specifier, "pkg/internal/add.js");
        assert!(!ownership.external_importable);
        assert!(report.audit.is_clean());
    }

    #[test]
    fn cascade_match_yields_no_attributions_when_index_is_empty() {
        let mut fps_map: BTreeMap<ModuleId, Vec<FunctionFingerprint>> = BTreeMap::new();
        let bundle_fps = FunctionExtractor::fingerprint(ModuleId(1), "function f(){return 1;}");
        fps_map.insert(ModuleId(1), bundle_fps);

        let report = match_with_cascade(&fps_map, &[]);
        assert!(report.attributions.is_empty());
        assert!(report.ownership_matches.is_empty());
        assert!(report.audit.is_clean());
    }

    #[test]
    fn cascade_match_records_unparseable_package_source() {
        let mut fps_map: BTreeMap<ModuleId, Vec<FunctionFingerprint>> = BTreeMap::new();
        fps_map.insert(ModuleId(1), Vec::new());
        let pkg_sources = [PackageSource::external(
            "broken",
            "0.0.1",
            "broken",
            "broken.js",
            "function (",
        )];

        let report = match_with_cascade(&fps_map, &pkg_sources);
        assert!(report.attributions.is_empty());
        assert!(report.audit.has(FindingCode::UnparseablePackageSource));
    }

    #[test]
    fn encode_function_id_does_not_alias_high_module_ids() {
        use reverts_ir::{ByteRange, FunctionId};
        // The previous bit-shift encoding lost bits 16-31 of module_id by
        // shifting past the u64 boundary. Distinct module ids that share
        // their low 16 bits used to collide; FNV-1a must keep them apart.
        let span = ByteRange::new(0, 100);
        let a = encode_function_id(&FunctionId::new(ModuleId(0x0001_0000), span));
        let b = encode_function_id(&FunctionId::new(ModuleId(0x0002_0000), span));
        let c = encode_function_id(&FunctionId::new(ModuleId(0x0001_0001), span));
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
    }

    #[test]
    fn encode_function_id_distinguishes_swapped_span_endpoints() {
        use reverts_ir::{ByteRange, FunctionId};
        // (start=A, end=B) and (start=B, end=A) must hash distinctly; the
        // previous XOR-based encoding made them suspicious. FNV-1a is
        // order-sensitive so they diverge unconditionally.
        let m = ModuleId(7);
        let ab = encode_function_id(&FunctionId::new(m, ByteRange::new(10, 50)));
        let small = encode_function_id(&FunctionId::new(m, ByteRange::new(10, 11)));
        let large = encode_function_id(&FunctionId::new(m, ByteRange::new(10, 5000)));
        assert_ne!(ab, small);
        assert_ne!(ab, large);
        assert_ne!(small, large);
    }
}
