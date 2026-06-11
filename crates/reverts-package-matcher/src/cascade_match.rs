//! End-to-end cascade + variant + version + Hungarian pipeline against a slice
//! of [`PackageSource`]s.
//!
//! This is the Phase-1 entry point introduced by the new package matcher spec
//! (§13). It builds an in-memory fingerprint index from the supplied package
//! sources, runs the cascade tiers per bundle function, lets the Hungarian
//! assignment resolve cross-package collisions, classifies each accepted match
//! through [`acceptance::classify`], and finally emits
//! [`PackageAttributionInput`] rows with `function_span` + `confidence`
//! populated.
//!
//! The existing `ExactPackageMatcher` / `VersionedPackageMatcher` paths in the
//! parent module are untouched: the new pipeline runs alongside the legacy one
//! during Phase-1 rollout.

use std::collections::BTreeMap;

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;
use reverts_graph::FunctionExtractor;
use reverts_input::PackageAttributionInput;
use reverts_ir::{
    AxisHashes, AxisKind, ControlFlowGraph, FunctionFingerprint, FunctionId, ModuleId,
};
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
    /// Findings produced during the match (parse failures, ambiguity, low
    /// confidence, etc.).
    pub audit: AuditReport,
}

/// Runs the cascade + variant + version + Hungarian pipeline against the given
/// per-module bundle fingerprints, using `package_sources` as the right-hand
/// side index.
///
/// Phase-1: this runs alongside [`crate::ExactPackageMatcher`] /
/// [`crate::VersionedPackageMatcher`]; callers may consume either or both.
#[must_use]
pub fn match_with_cascade(
    fingerprints_by_module: &BTreeMap<ModuleId, Vec<FunctionFingerprint>>,
    package_sources: &[PackageSource],
) -> CascadeMatchReport {
    let mut audit = AuditReport::default();
    let index = build_index(package_sources, &mut audit);

    let mut attributions = Vec::new();
    for (module_id, fps) in fingerprints_by_module {
        let assignments = assign_globally(fps, &index);
        for (fn_id, function_match) in assignments {
            let decision = classify(std::slice::from_ref(&function_match));
            match decision {
                AcceptanceDecision::Accepted { confidence } => {
                    attributions.push(build_attribution(
                        *module_id,
                        fn_id,
                        &function_match.candidate,
                        confidence,
                    ));
                }
                AcceptanceDecision::AcceptedWithCaveat { confidence } => {
                    let cand = &function_match.candidate;
                    attributions.push(build_attribution(*module_id, fn_id, cand, confidence));
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
                    let cand = &function_match.candidate;
                    audit.push(
                        AuditFinding::error(
                            FindingCode::AmbiguousPackageMatch,
                            "cascade function match has ambiguous tier",
                        )
                        .with_module(module_id.0.to_string())
                        .with_binding(cand.package.name.clone()),
                    );
                }
                AcceptanceDecision::NoMatch => {}
            }
        }
    }

    CascadeMatchReport {
        attributions,
        audit,
    }
}

fn build_index(
    package_sources: &[PackageSource],
    audit: &mut AuditReport,
) -> InMemoryFingerprintIndex {
    let mut index = InMemoryFingerprintIndex::new();
    for (idx, source) in package_sources.iter().enumerate() {
        let synthetic_module_id = ModuleId(u32::MAX - idx as u32);
        let alloc = Allocator::default();
        let source_type = SourceType::default().with_typescript(true).with_jsx(true);
        let parsed = Parser::new(&alloc, source.source.as_str(), source_type).parse();
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
        let cfg = ControlFlowGraph::default();
        let pkg_fps =
            FunctionExtractor::fingerprint(synthetic_module_id, source.source.as_str(), &cfg);
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
            // passes.
            for (pass_id, alt) in &fp.alternates {
                let mut alt_cand = base_candidate.clone();
                alt_cand.matched_alternate = Some(*pass_id);
                index.insert_exact(
                    ExactKey {
                        param_count: fp.param_count,
                        statement_count: fp.statement_count,
                        ast_hash: alt.ast,
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

fn build_attribution(
    module_id: ModuleId,
    fn_id: FunctionId,
    candidate: &Candidate,
    confidence: reverts_input::AttributionConfidence,
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

fn encode_function_id(fn_id: &FunctionId) -> u64 {
    let m = u64::from(fn_id.module_id.0);
    let s = u64::from(fn_id.span.start);
    let e = u64::from(fn_id.span.end);
    (m << 48) ^ (s << 24) ^ e
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
    use reverts_ir::{ControlFlowGraph, ModuleId};

    #[test]
    fn cascade_match_attributes_exact_function_with_function_span() {
        let bundle_source = "function f(a, b) { return a + b; }";
        let cfg = ControlFlowGraph::default();
        let bundle_fps = FunctionExtractor::fingerprint(ModuleId(10), bundle_source, &cfg);
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
    }

    #[test]
    fn cascade_match_yields_no_attributions_when_index_is_empty() {
        let mut fps_map: BTreeMap<ModuleId, Vec<FunctionFingerprint>> = BTreeMap::new();
        let cfg = ControlFlowGraph::default();
        let bundle_fps =
            FunctionExtractor::fingerprint(ModuleId(1), "function f(){return 1;}", &cfg);
        fps_map.insert(ModuleId(1), bundle_fps);

        let report = match_with_cascade(&fps_map, &[]);
        assert!(report.attributions.is_empty());
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
}
