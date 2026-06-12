use reverts_ir::{AxisHashes, AxisKind, FunctionFingerprint, MatchTier, NormalizationPassId};
use reverts_package_index::{
    Candidate, CfgKey, ExactKey, FeatureKey, PackageFingerprintIndex, StructuralKey,
};

#[derive(Debug, Clone, PartialEq)]
pub struct FunctionMatch {
    pub tier: MatchTier,
    pub candidate: Candidate,
    pub margin: f64,
    pub top_score: f64,
    pub runner_up_score: f64,
    pub matched_alternate: Option<NormalizationPassId>,
    /// Every axis that actually contributed evidence to this match.
    ///
    /// - Exact / ExactAlternate: `[Ast]` (the only axis the tier consults).
    /// - StructuralAnchored: `[Cfg, …overlapping anchor axes]` — every
    ///   `(LiteralAnchor | CalleeSet | ThrowSet)` whose hash actually
    ///   matched the candidate in the index.
    /// - FeatureSimilarity: `[primary, …remaining axes with Jaccard hit]`.
    /// - StructuralOnly: `[StructuralAnchor]`.
    ///
    /// Carrying the real set (rather than a single axis) lets
    /// [`crate::acceptance::AttributionConfidence`] surface the
    /// multi-axis evidence that drove the decision.
    pub matched_axes: Vec<AxisKind>,
}

#[must_use]
pub fn try_exact(
    fp: &FunctionFingerprint,
    index: &dyn PackageFingerprintIndex,
) -> Option<FunctionMatch> {
    let key = ExactKey {
        param_count: fp.param_count,
        statement_count: fp.statement_count,
        ast_hash: fp.primary.ast,
    };
    let candidates = index.query_exact(key);
    pick_unique(candidates, MatchTier::Exact, None, vec![AxisKind::Ast])
}

#[must_use]
pub fn try_exact_alternate(
    fp: &FunctionFingerprint,
    index: &dyn PackageFingerprintIndex,
) -> Option<FunctionMatch> {
    for (pass_id, axes) in &fp.alternates {
        let key = ExactKey {
            param_count: fp.param_count,
            statement_count: fp.statement_count,
            ast_hash: axes.ast,
        };
        let candidates = index.query_exact(key);
        if let Some(m) = pick_unique(
            candidates,
            MatchTier::ExactAlternate,
            Some(*pass_id),
            vec![AxisKind::Ast],
        ) {
            return Some(m);
        }
    }
    None
}

#[must_use]
pub fn try_structural_anchored(
    fp: &FunctionFingerprint,
    index: &dyn PackageFingerprintIndex,
) -> Option<FunctionMatch> {
    let cfg_key = CfgKey {
        param_count: fp.param_count,
        cfg_hash: fp.primary.cfg,
    };
    let cfg_candidates = index.query_cfg(cfg_key);
    if cfg_candidates.is_empty() {
        return None;
    }

    // Gather anchor (axis, hash) tuples that are present on the function side.
    let mut fp_anchors: Vec<(AxisKind, u64)> = Vec::new();
    if let Some(h) = fp.primary.literal_anchor {
        fp_anchors.push((AxisKind::LiteralAnchor, h));
    }
    if let Some(h) = fp.primary.callee_set {
        fp_anchors.push((AxisKind::CalleeSet, h));
    }
    if let Some(h) = fp.primary.throw_set {
        fp_anchors.push((AxisKind::ThrowSet, h));
    }
    if fp_anchors.is_empty() {
        return None;
    }

    // Pair each cfg candidate with the anchor axes that actually overlap;
    // drop candidates with zero overlap. Tracking the overlap set (rather
    // than a boolean) lets us record real multi-axis evidence on the
    // resulting FunctionMatch.
    let surviving: Vec<(Candidate, Vec<AxisKind>)> = cfg_candidates
        .into_iter()
        .filter_map(|c| {
            let overlap_axes: Vec<AxisKind> = fp_anchors
                .iter()
                .filter(|(axis, h)| {
                    index
                        .query_feature(FeatureKey {
                            param_count: fp.param_count,
                            kind: *axis,
                            hash: *h,
                        })
                        .iter()
                        .any(|cand| {
                            cand.package == c.package
                                && cand.external_function_id == c.external_function_id
                        })
                })
                .map(|(axis, _)| *axis)
                .collect();
            if overlap_axes.is_empty() {
                None
            } else {
                Some((c, overlap_axes))
            }
        })
        .collect();

    let mut unique = surviving;
    unique.dedup_by(|a, b| {
        a.0.package == b.0.package && a.0.external_function_id == b.0.external_function_id
    });
    if unique.len() != 1 {
        return None;
    }
    let (candidate, overlap_axes) = unique.into_iter().next()?;
    let mut matched_axes = Vec::with_capacity(1 + overlap_axes.len());
    matched_axes.push(AxisKind::Cfg);
    matched_axes.extend(overlap_axes);

    Some(FunctionMatch {
        tier: MatchTier::StructuralAnchored,
        candidate,
        margin: 1.0,
        top_score: f64::from(MatchTier::StructuralAnchored.weight()),
        runner_up_score: 0.0,
        matched_alternate: None,
        matched_axes,
    })
}

/// Tier 3: pick the most-distinctive axis present on the fingerprint,
/// query the index, and rank survivors by Jaccard overlap over the
/// remaining axis hashes. Require Jaccard ≥ 0.6 and unique best.
#[must_use]
pub fn try_feature_similarity(
    fp: &FunctionFingerprint,
    index: &dyn PackageFingerprintIndex,
) -> Option<FunctionMatch> {
    // Priority: callee_set > throw_set > literal_anchor > access_pattern.
    let (primary_axis, primary_hash) = priority_axis(&fp.primary)?;

    let cands = index.query_feature(FeatureKey {
        param_count: fp.param_count,
        kind: primary_axis,
        hash: primary_hash,
    });
    if cands.is_empty() {
        return None;
    }

    // Score each candidate by Jaccard over the remaining axes available on fp.
    // For each "remaining" axis where fp has a hash, do index.query_feature(...)
    // and check if our specific candidate appears.
    // The Jaccard numerator is "# remaining axes the candidate also has at this hash";
    // the denominator is "# remaining axes fp has" (approximation — we can't enumerate
    // candidate's full axis set without a per-candidate bag lookup).
    let remaining = collect_remaining_axes(&fp.primary, primary_axis);

    // For each candidate, record the actual overlapping axes (not just a
    // count) so the resulting FunctionMatch can carry multi-axis evidence.
    let mut scored: Vec<(Candidate, f64, Vec<AxisKind>)> = cands
        .into_iter()
        .map(|cand| {
            let mut overlap_axes = Vec::new();
            for (axis, hash) in &remaining {
                let cand_hits = index.query_feature(FeatureKey {
                    param_count: fp.param_count,
                    kind: *axis,
                    hash: *hash,
                });
                if cand_hits.iter().any(|c| {
                    c.package == cand.package && c.external_function_id == cand.external_function_id
                }) {
                    overlap_axes.push(*axis);
                }
            }
            let denom = remaining.len().max(1) as f64;
            let score = overlap_axes.len() as f64 / denom;
            (cand, score, overlap_axes)
        })
        .collect();

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let best = scored.first()?;
    if best.1 < 0.6 {
        return None;
    }
    if scored.len() > 1 {
        let runner_up = &scored[1];
        if (best.1 - runner_up.1).abs() < f64::EPSILON {
            return None; // tied — reject
        }
    }

    let mut matched_axes = Vec::with_capacity(1 + best.2.len());
    matched_axes.push(primary_axis);
    matched_axes.extend(best.2.iter().copied());

    Some(FunctionMatch {
        tier: MatchTier::FeatureSimilarity,
        candidate: best.0.clone(),
        margin: best.1 - scored.get(1).map_or(0.0, |s| s.1),
        top_score: best.1 * f64::from(MatchTier::FeatureSimilarity.weight()),
        runner_up_score: scored.get(1).map_or(0.0, |s| s.1)
            * f64::from(MatchTier::FeatureSimilarity.weight()),
        matched_alternate: None,
        matched_axes,
    })
}

fn priority_axis(axes: &AxisHashes) -> Option<(AxisKind, u64)> {
    if let Some(h) = axes.callee_set {
        return Some((AxisKind::CalleeSet, h));
    }
    if let Some(h) = axes.throw_set {
        return Some((AxisKind::ThrowSet, h));
    }
    if let Some(h) = axes.literal_anchor {
        return Some((AxisKind::LiteralAnchor, h));
    }
    if let Some(h) = axes.access_pattern {
        return Some((AxisKind::AccessPattern, h));
    }
    None
}

fn collect_remaining_axes(axes: &AxisHashes, exclude: AxisKind) -> Vec<(AxisKind, u64)> {
    let mut out: Vec<(AxisKind, u64)> = Vec::new();
    // Always-present axes
    if exclude != AxisKind::ReturnPattern {
        out.push((AxisKind::ReturnPattern, axes.return_pattern));
    }
    if exclude != AxisKind::EffectPattern {
        out.push((AxisKind::EffectPattern, axes.effect_pattern));
    }
    if exclude != AxisKind::StructuralAnchor {
        out.push((AxisKind::StructuralAnchor, axes.structural_anchor));
    }
    if exclude != AxisKind::BindingPattern {
        out.push((AxisKind::BindingPattern, axes.binding_pattern));
    }
    // Optional axes
    if let Some(h) = axes.literal_anchor
        && exclude != AxisKind::LiteralAnchor
    {
        out.push((AxisKind::LiteralAnchor, h));
    }
    if let Some(h) = axes.callee_set
        && exclude != AxisKind::CalleeSet
    {
        out.push((AxisKind::CalleeSet, h));
    }
    if let Some(h) = axes.throw_set
        && exclude != AxisKind::ThrowSet
    {
        out.push((AxisKind::ThrowSet, h));
    }
    if let Some(h) = axes.access_pattern
        && exclude != AxisKind::AccessPattern
    {
        out.push((AxisKind::AccessPattern, h));
    }
    if let Some(h) = axes.access_shape
        && exclude != AxisKind::AccessShape
    {
        out.push((AxisKind::AccessShape, h));
    }
    if let Some(h) = axes.literal_shape
        && exclude != AxisKind::LiteralShape
    {
        out.push((AxisKind::LiteralShape, h));
    }
    out
}

/// Tier 4: structural_anchor fallback for functions with no literal/property
/// hooks. Rejects candidates whose structural_anchor frequency in the
/// corpus exceeds STRUCTURAL_FREQUENCY_LIMIT — common shapes are
/// uninformative. Requires unique winner with margin ≥ 0.3 over runner-up.
pub const STRUCTURAL_FREQUENCY_LIMIT: u32 = 50;

#[must_use]
pub fn try_structural_only(
    fp: &FunctionFingerprint,
    index: &dyn PackageFingerprintIndex,
) -> Option<FunctionMatch> {
    let freq = index
        .corpus_stats()
        .frequency(AxisKind::StructuralAnchor, fp.primary.structural_anchor);
    if freq > STRUCTURAL_FREQUENCY_LIMIT {
        return None;
    }

    let key = StructuralKey {
        param_count: fp.param_count,
        structural_anchor: fp.primary.structural_anchor,
    };
    let cands = index.query_structural(key);
    if cands.is_empty() {
        return None;
    }

    // Distinct candidates only
    let mut distinct = cands.clone();
    distinct.dedup_by(|a, b| {
        a.package == b.package && a.external_function_id == b.external_function_id
    });

    if distinct.len() == 1 {
        let only = distinct.into_iter().next()?;
        return Some(FunctionMatch {
            tier: MatchTier::StructuralOnly,
            candidate: only,
            margin: 1.0,
            top_score: f64::from(MatchTier::StructuralOnly.weight()),
            runner_up_score: 0.0,
            matched_alternate: None,
            matched_axes: vec![AxisKind::StructuralAnchor],
        });
    }

    // Multiple candidates — we can't rank further with what we have here.
    // Reject; let global assignment (Phase G) handle disambiguation.
    None
}

pub(crate) fn pick_unique(
    mut candidates: Vec<Candidate>,
    tier: MatchTier,
    alt: Option<NormalizationPassId>,
    matched_axes: Vec<AxisKind>,
) -> Option<FunctionMatch> {
    if candidates.is_empty() {
        return None;
    }
    // Sort by (package name, version) so duplicates are adjacent.
    candidates.sort_by(|a, b| {
        a.package
            .name
            .cmp(&b.package.name)
            .then_with(|| a.package.version.cmp(&b.package.version))
            .then_with(|| a.external_function_id.cmp(&b.external_function_id))
    });
    // Dedupe by (package NAME, external_function_id) — different versions
    // of the same package indexing the same function (e.g. pretty-bytes
    // 6.1.0 and 6.1.1 with identical helper code) are not ambiguous;
    // they are the same library. Later phases (version-narrowing,
    // package-attribution Hungarian) resolve which specific version
    // wins, so the cascade tier should treat the package as uniquely
    // identified.
    candidates.dedup_by(|a, b| {
        a.package.name == b.package.name && a.external_function_id == b.external_function_id
    });
    if candidates.len() != 1 {
        return None;
    }
    let candidate = candidates.into_iter().next()?;
    Some(FunctionMatch {
        tier,
        candidate,
        margin: 1.0,
        top_score: f64::from(tier.weight()),
        runner_up_score: 0.0,
        matched_alternate: alt,
        matched_axes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use reverts_ir::{ByteRange, FunctionId, ModuleId};
    use reverts_package_index::{InMemoryFingerprintIndex, PackageId};

    fn sample_axes() -> AxisHashes {
        AxisHashes {
            ast: 0,
            cfg: 7,
            return_pattern: 0,
            effect_pattern: 0,
            literal_anchor: Some(11),
            access_pattern: None,
            structural_anchor: 0,
            literal_shape: None,
            access_shape: None,
            callee_set: Some(22),
            binding_pattern: 0,
            throw_set: None,
        }
    }

    #[test]
    fn structural_anchored_records_all_overlapping_axes() {
        let mut idx = InMemoryFingerprintIndex::new();
        let cand = Candidate {
            package: PackageId {
                name: "p".into(),
                version: "1.0".into(),
            },
            variant_path: "i.js".into(),
            external_function_id: 1,
            matched_axis: AxisKind::Cfg,
            matched_alternate: None,
        };
        idx.insert_cfg(
            CfgKey {
                param_count: 1,
                cfg_hash: 7,
            },
            cand.clone(),
        );
        // Two anchor axes overlap: literal_anchor=11 and callee_set=22.
        idx.insert_feature(
            FeatureKey {
                param_count: 1,
                kind: AxisKind::LiteralAnchor,
                hash: 11,
            },
            cand.clone(),
        );
        idx.insert_feature(
            FeatureKey {
                param_count: 1,
                kind: AxisKind::CalleeSet,
                hash: 22,
            },
            cand,
        );

        let fp = FunctionFingerprint {
            id: FunctionId::new(ModuleId(1), ByteRange::new(0, 10)),
            param_count: 1,
            statement_count: 1,
            primary: sample_axes(),
            alternates: Vec::new(),
        };

        let m = try_structural_anchored(&fp, &idx).expect("structural-anchored match");
        // matched_axes must record CFG (the gating axis) AND every anchor
        // axis that actually overlapped — proves the field carries real
        // multi-axis evidence rather than a single placeholder.
        assert_eq!(m.tier, MatchTier::StructuralAnchored);
        assert!(m.matched_axes.contains(&AxisKind::Cfg));
        assert!(m.matched_axes.contains(&AxisKind::LiteralAnchor));
        assert!(m.matched_axes.contains(&AxisKind::CalleeSet));
        assert!(!m.matched_axes.contains(&AxisKind::ThrowSet));
    }

    #[test]
    fn exact_match_records_single_ast_axis() {
        let mut idx = InMemoryFingerprintIndex::new();
        idx.insert_exact(
            ExactKey {
                param_count: 1,
                statement_count: 1,
                ast_hash: 99,
            },
            Candidate {
                package: PackageId {
                    name: "p".into(),
                    version: "1.0".into(),
                },
                variant_path: "i.js".into(),
                external_function_id: 1,
                matched_axis: AxisKind::Ast,
                matched_alternate: None,
            },
        );

        let mut axes = sample_axes();
        axes.ast = 99;
        let fp = FunctionFingerprint {
            id: FunctionId::new(ModuleId(1), ByteRange::new(0, 10)),
            param_count: 1,
            statement_count: 1,
            primary: axes,
            alternates: Vec::new(),
        };

        let m = try_exact(&fp, &idx).expect("exact match");
        assert_eq!(m.matched_axes, vec![AxisKind::Ast]);
    }
}
