use reverts_ir::{AxisHashes, AxisKind, FunctionFingerprint, MatchTier, NormalizationPassId};
use reverts_package_index::{Candidate, CfgKey, ExactKey, FeatureKey, PackageFingerprintIndex};

#[derive(Debug, Clone, PartialEq)]
pub struct FunctionMatch {
    pub tier: MatchTier,
    pub candidate: Candidate,
    pub margin: f64,
    pub top_score: f64,
    pub runner_up_score: f64,
    pub matched_alternate: Option<NormalizationPassId>,
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
    pick_unique(candidates, MatchTier::Exact, None)
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
        if let Some(m) = pick_unique(candidates, MatchTier::ExactAlternate, Some(*pass_id)) {
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

    // Retain candidates that share at least one anchor (axis, hash) with the fp.
    let surviving: Vec<Candidate> = cfg_candidates
        .into_iter()
        .filter(|c| {
            fp_anchors.iter().any(|(axis, h)| {
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
        })
        .collect();

    pick_unique(surviving, MatchTier::StructuralAnchored, None)
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

    let mut scored: Vec<(Candidate, f64)> = cands
        .into_iter()
        .map(|cand| {
            let mut overlap = 0u32;
            for (axis, hash) in &remaining {
                let cand_hits = index.query_feature(FeatureKey {
                    param_count: fp.param_count,
                    kind: *axis,
                    hash: *hash,
                });
                if cand_hits.iter().any(|c| {
                    c.package == cand.package && c.external_function_id == cand.external_function_id
                }) {
                    overlap += 1;
                }
            }
            let denom = remaining.len().max(1) as f64;
            let score = f64::from(overlap) / denom;
            (cand, score)
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

    Some(FunctionMatch {
        tier: MatchTier::FeatureSimilarity,
        candidate: best.0.clone(),
        margin: best.1 - scored.get(1).map_or(0.0, |s| s.1),
        top_score: best.1 * f64::from(MatchTier::FeatureSimilarity.weight()),
        runner_up_score: scored.get(1).map_or(0.0, |s| s.1)
            * f64::from(MatchTier::FeatureSimilarity.weight()),
        matched_alternate: None,
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

pub(crate) fn pick_unique(
    mut candidates: Vec<Candidate>,
    tier: MatchTier,
    alt: Option<NormalizationPassId>,
) -> Option<FunctionMatch> {
    if candidates.is_empty() {
        return None;
    }
    candidates.dedup_by(|a, b| {
        a.package == b.package && a.external_function_id == b.external_function_id
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
    })
}
