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
    exact_for_axes(
        fp.param_count,
        fp.statement_count,
        fp.primary.ast,
        index,
        MatchTier::Exact,
        None,
    )
}

fn exact_for_axes(
    param_count: u32,
    statement_count: u32,
    ast_hash: u64,
    index: &dyn PackageFingerprintIndex,
    tier: MatchTier,
    matched_alternate: Option<NormalizationPassId>,
) -> Option<FunctionMatch> {
    let key = ExactKey {
        param_count,
        statement_count,
        ast_hash,
    };
    let candidates = index.query_exact(key);
    pick_unique(candidates, tier, matched_alternate, vec![AxisKind::Ast])
}

fn first_alternate_match(
    fp: &FunctionFingerprint,
    mut match_alternate: impl FnMut(u32, &AxisHashes, NormalizationPassId) -> Option<FunctionMatch>,
) -> Option<FunctionMatch> {
    for alt in &fp.alternates {
        if let Some(function_match) = match_alternate(alt.statement_count, &alt.axes, alt.pass) {
            return Some(function_match);
        }
    }
    None
}

#[must_use]
pub fn try_exact_alternate(
    fp: &FunctionFingerprint,
    index: &dyn PackageFingerprintIndex,
) -> Option<FunctionMatch> {
    first_alternate_match(fp, |statement_count, axes, pass| {
        exact_for_axes(
            fp.param_count,
            statement_count,
            axes.ast,
            index,
            MatchTier::ExactAlternate,
            Some(pass),
        )
    })
}

#[must_use]
pub fn try_structural_anchored(
    fp: &FunctionFingerprint,
    index: &dyn PackageFingerprintIndex,
) -> Option<FunctionMatch> {
    structural_anchored_for_axes(
        fp.param_count,
        &fp.primary,
        index,
        MatchTier::StructuralAnchored,
        None,
    )
}

/// Like [`try_structural_anchored`], but consults each alternate
/// fingerprint produced by a normalization pass. Returns matches at
/// [`MatchTier::StructuralAnchoredAlternate`] (weight 500, strictly
/// below the primary structural tier) so the Hungarian global
/// assignment prefers a primary structural match over an alternate
/// one when both compete — and falls back to the alternate when only
/// it has evidence.
#[must_use]
pub fn try_structural_anchored_alternate(
    fp: &FunctionFingerprint,
    index: &dyn PackageFingerprintIndex,
) -> Option<FunctionMatch> {
    first_alternate_match(fp, |_statement_count, axes, pass| {
        structural_anchored_for_axes(
            fp.param_count,
            axes,
            index,
            MatchTier::StructuralAnchoredAlternate,
            Some(pass),
        )
    })
}

fn structural_anchored_for_axes(
    param_count: u32,
    axes: &AxisHashes,
    index: &dyn PackageFingerprintIndex,
    tier: MatchTier,
    matched_alternate: Option<NormalizationPassId>,
) -> Option<FunctionMatch> {
    let cfg_key = CfgKey {
        param_count,
        cfg_hash: axes.cfg,
    };
    let cfg_candidates = index.query_cfg(cfg_key);
    if cfg_candidates.is_empty() {
        return None;
    }

    let mut fp_anchors: Vec<(AxisKind, u64)> = Vec::new();
    if let Some(h) = axes.literal_anchor {
        fp_anchors.push((AxisKind::LiteralAnchor, h));
    }
    if let Some(h) = axes.callee_set {
        fp_anchors.push((AxisKind::CalleeSet, h));
    }
    if let Some(h) = axes.throw_set {
        fp_anchors.push((AxisKind::ThrowSet, h));
    }
    if fp_anchors.is_empty() {
        return None;
    }

    let surviving: Vec<(Candidate, Vec<AxisKind>)> = cfg_candidates
        .into_iter()
        .filter_map(|c| {
            let overlap_axes: Vec<AxisKind> = fp_anchors
                .iter()
                .filter(|(axis, h)| {
                    index
                        .query_feature(FeatureKey {
                            param_count,
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
        tier,
        candidate,
        margin: 1.0,
        top_score: f64::from(tier.weight()),
        runner_up_score: 0.0,
        matched_alternate,
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
    feature_similarity_for_axes(
        fp.param_count,
        &fp.primary,
        index,
        MatchTier::FeatureSimilarity,
        None,
    )
}

/// Like [`try_feature_similarity`], but consults each alternate
/// fingerprint. Returns matches at
/// [`MatchTier::FeatureSimilarityAlternate`] (weight 50) so the
/// Hungarian global assignment prefers a primary feature-similarity
/// match when one exists, and falls back to an alternate when only
/// the alt-source axes line up.
#[must_use]
pub fn try_feature_similarity_alternate(
    fp: &FunctionFingerprint,
    index: &dyn PackageFingerprintIndex,
) -> Option<FunctionMatch> {
    first_alternate_match(fp, |_statement_count, axes, pass| {
        feature_similarity_for_axes(
            fp.param_count,
            axes,
            index,
            MatchTier::FeatureSimilarityAlternate,
            Some(pass),
        )
    })
}

fn feature_similarity_for_axes(
    param_count: u32,
    axes: &AxisHashes,
    index: &dyn PackageFingerprintIndex,
    tier: MatchTier,
    matched_alternate: Option<NormalizationPassId>,
) -> Option<FunctionMatch> {
    let (primary_axis, primary_hash) = priority_axis(axes)?;

    let cands = index.query_feature(FeatureKey {
        param_count,
        kind: primary_axis,
        hash: primary_hash,
    });
    if cands.is_empty() {
        return None;
    }

    let remaining = collect_remaining_axes(axes, primary_axis);

    let mut scored: Vec<(Candidate, f64, Vec<AxisKind>)> = cands
        .into_iter()
        .map(|cand| {
            let mut overlap_axes = Vec::new();
            for (axis, hash) in &remaining {
                let cand_hits = index.query_feature(FeatureKey {
                    param_count,
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
    // Primary feature-similarity tier requires Jaccard ≥ 0.6 (high
    // confidence); the alt-source tier (lower weight, second-class
    // evidence) accepts ≥ 0.5 — Hungarian global assignment still
    // prefers any primary match over an alternate one when both
    // compete for the same (package, fn_id) pair because of the
    // tier-weight ordering, so loosening the threshold here is a
    // recall-only knob without precision cost on shared candidates.
    let threshold = if tier == MatchTier::FeatureSimilarityAlternate {
        0.5
    } else {
        0.6
    };
    if best.1 < threshold {
        return None;
    }
    if scored.len() > 1 {
        let runner_up = &scored[1];
        if (best.1 - runner_up.1).abs() < f64::EPSILON {
            return None;
        }
    }

    let mut matched_axes = Vec::with_capacity(1 + best.2.len());
    matched_axes.push(primary_axis);
    matched_axes.extend(best.2.iter().copied());

    Some(FunctionMatch {
        tier,
        candidate: best.0.clone(),
        margin: best.1 - scored.get(1).map_or(0.0, |s| s.1),
        top_score: best.1 * f64::from(tier.weight()),
        runner_up_score: scored.get(1).map_or(0.0, |s| s.1) * f64::from(tier.weight()),
        matched_alternate,
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

/// Tier 4: structural_anchor-only match for functions with no literal/property
/// hooks. Rejects candidates whose structural_anchor frequency in the
/// corpus exceeds STRUCTURAL_FREQUENCY_LIMIT — common shapes are
/// uninformative. Requires unique winner with margin ≥ 0.3 over runner-up.
pub const STRUCTURAL_FREQUENCY_LIMIT: u32 = 50;

/// Higher frequency ceiling for the alt-source structural-only tier.
/// Strategically loosened because the alt-tier weight (5) is already
/// discounted under the primary structural_only weight (10), so the
/// Hungarian global assignment will prefer a primary match whenever
/// one exists. We trade slightly more permissive anchor frequency at
/// the alt tier for the chance to recover matches the primary tier
/// would silently reject.
const STRUCTURAL_FREQUENCY_LIMIT_ALT: u32 = 75;

#[must_use]
pub fn try_structural_only(
    fp: &FunctionFingerprint,
    index: &dyn PackageFingerprintIndex,
) -> Option<FunctionMatch> {
    structural_only_for_anchor(
        fp.param_count,
        fp.primary.structural_anchor,
        index,
        MatchTier::StructuralOnly,
        None,
    )
}

/// Like [`try_structural_only`], but consults each alternate
/// fingerprint's structural_anchor hash. Returns matches at
/// [`MatchTier::StructuralOnlyAlternate`] (weight 5) so the Hungarian
/// global assignment prefers a primary structural-only match over an
/// alt one when both compete. Useful when a normalization pass (e.g.
/// `infinite_for_to_while`, `conditional_statement_expanded`) shifts
/// the loop/cond counts that feed `structural_anchor`.
#[must_use]
pub fn try_structural_only_alternate(
    fp: &FunctionFingerprint,
    index: &dyn PackageFingerprintIndex,
) -> Option<FunctionMatch> {
    first_alternate_match(fp, |_statement_count, axes, pass| {
        structural_only_for_anchor(
            fp.param_count,
            axes.structural_anchor,
            index,
            MatchTier::StructuralOnlyAlternate,
            Some(pass),
        )
    })
}

fn structural_only_for_anchor(
    param_count: u32,
    structural_anchor: u64,
    index: &dyn PackageFingerprintIndex,
    tier: MatchTier,
    matched_alternate: Option<NormalizationPassId>,
) -> Option<FunctionMatch> {
    let freq = index
        .corpus_stats()
        .frequency(AxisKind::StructuralAnchor, structural_anchor);
    let limit = match tier {
        MatchTier::StructuralOnlyAlternate => STRUCTURAL_FREQUENCY_LIMIT_ALT,
        _ => STRUCTURAL_FREQUENCY_LIMIT,
    };
    if freq > limit {
        return None;
    }

    let key = StructuralKey {
        param_count,
        structural_anchor,
    };
    let cands = index.query_structural(key);
    if cands.is_empty() {
        return None;
    }

    let mut distinct = cands.clone();
    distinct.dedup_by(|a, b| {
        a.package == b.package && a.external_function_id == b.external_function_id
    });

    if distinct.len() == 1 {
        let only = distinct.into_iter().next()?;
        return Some(FunctionMatch {
            tier,
            candidate: only,
            margin: 1.0,
            top_score: f64::from(tier.weight()),
            runner_up_score: 0.0,
            matched_alternate,
            matched_axes: vec![AxisKind::StructuralAnchor],
        });
    }

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
            external_importable: true,
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
    fn feature_similarity_alternate_accepts_jaccard_just_above_alt_threshold() {
        // The primary `try_feature_similarity` requires Jaccard ≥ 0.6;
        // the alt-source variant relaxes to ≥ 0.5. Build an index where
        // exactly half of the function's remaining axes overlap the
        // candidate — Jaccard ≈ 0.5 — and verify:
        //   * the primary tier rejects (0.5 < 0.6);
        //   * the alt tier accepts when the same axis values appear on an
        //     alternate fingerprint.
        let mut idx = InMemoryFingerprintIndex::new();
        let cand = Candidate {
            package: PackageId {
                name: "p".into(),
                version: "1.0".into(),
            },
            variant_path: "i.js".into(),
            external_function_id: 33,
            matched_axis: AxisKind::CalleeSet,
            matched_alternate: None,
            external_importable: true,
        };
        // Index the primary-axis (CalleeSet=99) and exactly one of the
        // remaining axes (LiteralAnchor=10) — half of the two-axis
        // remaining set after `collect_remaining_axes` drops the
        // primary. With remaining={return_pattern, effect_pattern,
        // structural_anchor, binding_pattern, literal_anchor (when
        // Some)} ≈ 5 axes total, exactly 3 overlapping yields ~0.6.
        // To engineer a Jaccard of 0.5, we make 1 of 2 indexed axes
        // overlap by keeping only one optional axis Some.
        idx.insert_feature(
            FeatureKey {
                param_count: 1,
                kind: AxisKind::CalleeSet,
                hash: 99,
            },
            cand.clone(),
        );
        // Provide overlap on exactly half the remaining axes.
        idx.insert_feature(
            FeatureKey {
                param_count: 1,
                kind: AxisKind::ReturnPattern,
                hash: 1,
            },
            cand.clone(),
        );
        idx.insert_feature(
            FeatureKey {
                param_count: 1,
                kind: AxisKind::StructuralAnchor,
                hash: 1,
            },
            cand.clone(),
        );

        // Primary axes lack the indexed values entirely (no callee_set
        // hit), so primary feature_similarity finds no candidates.
        let primary = AxisHashes {
            ast: 0,
            cfg: 0,
            return_pattern: 99_999, // doesn't match indexed return_pattern=1
            effect_pattern: 0,
            literal_anchor: None,
            access_pattern: None,
            structural_anchor: 99_999, // doesn't match indexed structural=1
            literal_shape: None,
            access_shape: None,
            callee_set: None, // primary has no callee_set → priority_axis returns None
            binding_pattern: 0,
            throw_set: None,
        };

        // Alternate-source axes match the candidate on primary axis
        // (callee_set=99) and exactly 2 of 4 remaining axes
        // (return_pattern=1, structural_anchor=1). Jaccard ≈ 2/4 = 0.5,
        // which the alt threshold accepts.
        let alt = AxisHashes {
            ast: 0,
            cfg: 0,
            return_pattern: 1,
            effect_pattern: 0,
            literal_anchor: None,
            access_pattern: None,
            structural_anchor: 1,
            literal_shape: None,
            access_shape: None,
            callee_set: Some(99),
            binding_pattern: 0,
            throw_set: None,
        };

        let fp = FunctionFingerprint {
            id: FunctionId::new(ModuleId(1), ByteRange::new(0, 10)),
            param_count: 1,
            statement_count: 1,
            primary,
            alternates: vec![reverts_ir::AlternateAxisHashes {
                pass: NormalizationPassId::ComputedToStaticMember,
                statement_count: 1,
                axes: alt,
            }],
        };

        // Primary tier rejects (no callee_set on primary).
        assert!(try_feature_similarity(&fp, &idx).is_none());

        // Alt tier accepts via the alternate axes at ≥0.5 threshold.
        let m = try_feature_similarity_alternate(&fp, &idx)
            .expect("alt feature-similarity match expected at Jaccard ≥ 0.5");
        assert_eq!(m.tier, MatchTier::FeatureSimilarityAlternate);
        assert!(
            m.tier.weight() < MatchTier::FeatureSimilarity.weight(),
            "alt-tier weight must be strictly below primary"
        );
        assert_eq!(
            m.matched_alternate,
            Some(NormalizationPassId::ComputedToStaticMember)
        );
    }

    #[test]
    fn structural_only_alternate_uses_alt_structural_anchor() {
        // Index a candidate keyed on a specific structural_anchor hash.
        let mut idx = InMemoryFingerprintIndex::new();
        let cand = Candidate {
            package: PackageId {
                name: "p".into(),
                version: "1.0".into(),
            },
            variant_path: "i.js".into(),
            external_function_id: 9,
            matched_axis: AxisKind::StructuralAnchor,
            matched_alternate: None,
            external_importable: true,
        };
        let alt_anchor: u64 = 4242;
        idx.insert_structural(
            reverts_package_index::StructuralKey {
                param_count: 1,
                structural_anchor: alt_anchor,
            },
            cand.clone(),
        );

        // Primary has an unrelated structural_anchor that won't match.
        let mut primary = sample_axes();
        primary.structural_anchor = 1;

        // An alternate produced by some normalization pass shifts the
        // structural_anchor counts to match the indexed candidate.
        let mut alt_axes = sample_axes();
        alt_axes.structural_anchor = alt_anchor;

        let fp = FunctionFingerprint {
            id: FunctionId::new(ModuleId(1), ByteRange::new(0, 10)),
            param_count: 1,
            statement_count: 1,
            primary,
            alternates: vec![reverts_ir::AlternateAxisHashes {
                pass: NormalizationPassId::InfiniteForToWhile,
                statement_count: 1,
                axes: alt_axes,
            }],
        };

        // Primary structural_only doesn't match (different anchor).
        assert!(try_structural_only(&fp, &idx).is_none());

        // Alt-tier finds the unique candidate via the alt's anchor.
        let m = try_structural_only_alternate(&fp, &idx).expect("structural-only-alternate match");
        assert_eq!(m.tier, MatchTier::StructuralOnlyAlternate);
        assert!(
            m.tier.weight() < MatchTier::StructuralOnly.weight(),
            "alternate tier weight must be strictly below primary structural_only"
        );
        assert_eq!(
            m.matched_alternate,
            Some(NormalizationPassId::InfiniteForToWhile)
        );
        assert_eq!(m.matched_axes, vec![AxisKind::StructuralAnchor]);
    }

    #[test]
    fn structural_anchored_alternate_uses_alt_axes_when_primary_misses() {
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
            external_importable: true,
        };
        idx.insert_cfg(
            CfgKey {
                param_count: 1,
                cfg_hash: 555,
            },
            cand.clone(),
        );
        idx.insert_feature(
            FeatureKey {
                param_count: 1,
                kind: AxisKind::CalleeSet,
                hash: 777,
            },
            cand.clone(),
        );

        // Primary fp has UNRELATED cfg/callee_set values that won't match.
        let mut primary = sample_axes();
        primary.cfg = 1;
        primary.callee_set = Some(2);

        // An alternate produced by a normalization pass exposes the
        // cfg=555 + callee_set=777 shape — same as the indexed candidate.
        let mut alt_axes = sample_axes();
        alt_axes.cfg = 555;
        alt_axes.callee_set = Some(777);
        alt_axes.literal_anchor = None;
        alt_axes.throw_set = None;

        let fp = FunctionFingerprint {
            id: FunctionId::new(ModuleId(1), ByteRange::new(0, 10)),
            param_count: 1,
            statement_count: 1,
            primary,
            alternates: vec![reverts_ir::AlternateAxisHashes {
                pass: NormalizationPassId::ComputedToStaticMember,
                statement_count: 1,
                axes: alt_axes,
            }],
        };

        // Primary doesn't match.
        assert!(try_structural_anchored(&fp, &idx).is_none());

        // Alternate-tier finds it at the new, lower-weight tier and
        // records the source pass.
        let m = try_structural_anchored_alternate(&fp, &idx).expect("alternate structural match");
        assert_eq!(m.tier, MatchTier::StructuralAnchoredAlternate);
        assert!(
            m.tier.weight() < MatchTier::StructuralAnchored.weight(),
            "alternate tier weight must be strictly below primary structural tier"
        );
        assert_eq!(
            m.matched_alternate,
            Some(NormalizationPassId::ComputedToStaticMember)
        );
        assert!(m.matched_axes.contains(&AxisKind::Cfg));
        assert!(m.matched_axes.contains(&AxisKind::CalleeSet));
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
                external_importable: true,
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
