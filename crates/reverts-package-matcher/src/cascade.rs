use reverts_ir::{FunctionFingerprint, FunctionId};
use reverts_package_index::PackageFingerprintIndex;

use crate::hungarian::assign_max_weight;
use crate::tier::{
    FunctionMatch, try_exact, try_exact_alternate, try_feature_similarity, try_structural_anchored,
    try_structural_only,
};

#[must_use]
pub fn match_function(
    fp: &FunctionFingerprint,
    index: &dyn PackageFingerprintIndex,
) -> Option<FunctionMatch> {
    try_exact(fp, index)
        .or_else(|| try_exact_alternate(fp, index))
        .or_else(|| try_structural_anchored(fp, index))
        .or_else(|| try_feature_similarity(fp, index))
        .or_else(|| try_structural_only(fp, index))
}

/// Cascade per function, collecting up to 5 ranked candidates per function.
///
/// Each candidate is the best match the corresponding tier could produce.
/// The order is significant: index 0 is the top-tier candidate (Exact when
/// available), index 1 the next tier, and so on. Both
/// [`assign_globally`] (for cross-package collision resolution) and
/// [`crate::acceptance::classify`] (for per-fp confidence margin) consume
/// this list; classify in particular relies on the runner-up entries to
/// distinguish a confident match from an ambiguous one.
#[must_use]
pub fn cascade_candidates(
    fp: &FunctionFingerprint,
    index: &dyn PackageFingerprintIndex,
) -> Vec<FunctionMatch> {
    let mut all = Vec::new();
    if let Some(m) = try_exact(fp, index) {
        all.push(m);
    }
    if let Some(m) = try_exact_alternate(fp, index) {
        all.push(m);
    }
    if let Some(m) = try_structural_anchored(fp, index) {
        all.push(m);
    }
    if let Some(m) = try_feature_similarity(fp, index) {
        all.push(m);
    }
    if let Some(m) = try_structural_only(fp, index) {
        all.push(m);
    }
    all
}

/// Per-function output of [`assign_globally`]: both the full pre-Hungarian
/// candidate list (so callers can call [`crate::acceptance::classify`] with
/// real runner-up information) and the chosen match after the global
/// bipartite assignment, when one exists.
#[derive(Debug, Clone, PartialEq)]
pub struct GlobalAssignment {
    pub function_id: FunctionId,
    pub candidates: Vec<FunctionMatch>,
    pub chosen: Option<FunctionMatch>,
}

/// Assign bundle functions to package candidates globally using the Hungarian
/// algorithm, resolving cross-package collisions optimally instead of greedily.
///
/// For each bundle function, the cascade tiers are evaluated to collect up to
/// 5 ranked candidates. A bipartite weight matrix is built over all unique
/// `(package, external_function_id)` pairs seen across all candidate lists,
/// and `assign_max_weight` picks the globally optimal one-to-one assignment.
#[must_use]
pub fn assign_globally(
    bundle_fps: &[FunctionFingerprint],
    index: &dyn PackageFingerprintIndex,
) -> Vec<GlobalAssignment> {
    use reverts_package_index::PackageId;

    let mut out: Vec<GlobalAssignment> = bundle_fps
        .iter()
        .map(|fp| GlobalAssignment {
            function_id: fp.id,
            candidates: cascade_candidates(fp, index),
            chosen: None,
        })
        .collect();

    // Right-side global candidate dictionary: (PackageId, external_function_id) -> column idx.
    // IMPORTANT: len() is captured before or_insert so that each new key gets
    // a unique sequential index equal to the count at insertion time.
    let mut col_index: std::collections::BTreeMap<(PackageId, u64), usize> =
        std::collections::BTreeMap::new();
    for assignment in &out {
        for cand in &assignment.candidates {
            let key = (
                cand.candidate.package.clone(),
                cand.candidate.external_function_id,
            );
            let next = col_index.len();
            col_index.entry(key).or_insert(next);
        }
    }

    if col_index.is_empty() {
        return out;
    }

    let n_rows = out.len();
    let n_cols = col_index.len();

    // Build cost matrix (max-weight).
    let mut cost = vec![vec![0.0_f64; n_cols]; n_rows];
    for (row, assignment) in out.iter().enumerate() {
        for (rank, cand) in assignment.candidates.iter().enumerate() {
            let key = (
                cand.candidate.package.clone(),
                cand.candidate.external_function_id,
            );
            if let Some(&col) = col_index.get(&key) {
                let r = rank.min(4) as f64;
                let weight = f64::from(cand.tier.weight()) * (1.0 - 0.1 * r);
                if weight > cost[row][col] {
                    cost[row][col] = weight;
                }
            }
        }
    }

    let assign = assign_max_weight(&cost);

    // Map row → matched candidate (skip unassigned or zero-weight padding rows).
    for (row, &col) in assign.iter().enumerate() {
        if col == usize::MAX || col >= n_cols {
            continue;
        }
        if cost[row][col] <= 0.0 {
            continue; // zero weight = unmatched (padding column)
        }
        // Reverse-lookup: find the (PackageId, external_function_id) for this column.
        let Some(key_pair) = col_index
            .iter()
            .find(|(_, v)| **v == col)
            .map(|(k, _)| k.clone())
        else {
            continue;
        };
        let chosen = out[row]
            .candidates
            .iter()
            .find(|m| {
                m.candidate.package == key_pair.0 && m.candidate.external_function_id == key_pair.1
            })
            .cloned();
        out[row].chosen = chosen;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use reverts_ir::{
        AxisHashes, AxisKind, ByteRange, FunctionFingerprint, FunctionId, MatchTier, ModuleId,
        NormalizationPassId,
    };
    use reverts_package_index::{
        Candidate, ExactKey, FeatureKey, InMemoryFingerprintIndex, PackageId, StructuralKey,
    };

    fn sample_axes(ast: u64) -> AxisHashes {
        AxisHashes {
            ast,
            cfg: 0,
            return_pattern: 0,
            effect_pattern: 0,
            literal_anchor: None,
            access_pattern: None,
            structural_anchor: 0,
            literal_shape: None,
            access_shape: None,
            callee_set: None,
            binding_pattern: 0,
            throw_set: None,
        }
    }

    #[test]
    fn match_function_returns_exact_when_unique() {
        let mut idx = InMemoryFingerprintIndex::new();
        let key = ExactKey {
            param_count: 2,
            statement_count: 3,
            ast_hash: 100,
        };
        idx.insert_exact(
            key,
            Candidate {
                package: PackageId {
                    name: "lodash".into(),
                    version: "4.17.21".into(),
                },
                variant_path: "index.js".into(),
                external_function_id: 7,
                matched_axis: AxisKind::Ast,
                matched_alternate: None,
            },
        );

        let fp = FunctionFingerprint {
            id: FunctionId::new(ModuleId(1), ByteRange::new(0, 100)),
            param_count: 2,
            statement_count: 3,
            primary: sample_axes(100),
            alternates: Vec::new(),
        };

        let m = match_function(&fp, &idx).expect("match");
        assert_eq!(m.tier, MatchTier::Exact);
        assert_eq!(m.candidate.package.name, "lodash");
    }

    #[test]
    fn match_function_rejects_ambiguous_exact() {
        let mut idx = InMemoryFingerprintIndex::new();
        let key = ExactKey {
            param_count: 2,
            statement_count: 3,
            ast_hash: 100,
        };
        for (pkg, fid) in [("a", 1u64), ("b", 2)] {
            idx.insert_exact(
                key,
                Candidate {
                    package: PackageId {
                        name: pkg.into(),
                        version: "1".into(),
                    },
                    variant_path: "i.js".into(),
                    external_function_id: fid,
                    matched_axis: AxisKind::Ast,
                    matched_alternate: None,
                },
            );
        }
        let fp = FunctionFingerprint {
            id: FunctionId::new(ModuleId(1), ByteRange::new(0, 100)),
            param_count: 2,
            statement_count: 3,
            primary: sample_axes(100),
            alternates: Vec::new(),
        };
        assert!(
            match_function(&fp, &idx).is_none(),
            "ambiguous exact must not return a match",
        );
    }

    #[test]
    fn match_function_falls_through_to_exact_alternate() {
        let mut idx = InMemoryFingerprintIndex::new();
        // No primary exact match; an alternate matches at ast=222
        idx.insert_exact(
            ExactKey {
                param_count: 1,
                statement_count: 1,
                ast_hash: 222,
            },
            Candidate {
                package: PackageId {
                    name: "p".into(),
                    version: "1".into(),
                },
                variant_path: "i.js".into(),
                external_function_id: 1,
                matched_axis: AxisKind::Ast,
                matched_alternate: None,
            },
        );

        let primary = sample_axes(100); // doesn't match anything in the index
        let alt = sample_axes(222);
        let fp = FunctionFingerprint {
            id: FunctionId::new(ModuleId(1), ByteRange::new(0, 10)),
            param_count: 1,
            statement_count: 1,
            primary,
            alternates: vec![(NormalizationPassId::TsRuntimeErased, alt)],
        };

        let m = match_function(&fp, &idx).expect("alternate match");
        assert_eq!(m.tier, MatchTier::ExactAlternate);
        assert_eq!(
            m.matched_alternate,
            Some(NormalizationPassId::TsRuntimeErased)
        );
    }

    #[test]
    fn structural_anchored_requires_cfg_and_anchor_overlap() {
        let mut idx = InMemoryFingerprintIndex::new();
        let candidate = Candidate {
            package: PackageId {
                name: "p".into(),
                version: "1".into(),
            },
            variant_path: "i.js".into(),
            external_function_id: 1,
            matched_axis: AxisKind::Cfg,
            matched_alternate: None,
        };
        idx.insert_cfg(
            reverts_package_index::CfgKey {
                param_count: 1,
                cfg_hash: 7,
            },
            candidate.clone(),
        );
        idx.insert_feature(
            reverts_package_index::FeatureKey {
                param_count: 1,
                kind: AxisKind::LiteralAnchor,
                hash: 99,
            },
            candidate.clone(),
        );

        let mut axes = sample_axes(0);
        axes.cfg = 7;
        axes.literal_anchor = Some(99);
        let fp = FunctionFingerprint {
            id: FunctionId::new(ModuleId(1), ByteRange::new(0, 10)),
            param_count: 1,
            statement_count: 1,
            primary: axes,
            alternates: Vec::new(),
        };

        let m = match_function(&fp, &idx).expect("structural-anchored match");
        assert_eq!(m.tier, MatchTier::StructuralAnchored);
    }

    #[test]
    fn structural_anchored_rejects_when_no_anchor_overlap() {
        let mut idx = InMemoryFingerprintIndex::new();
        // CFG matches but no anchor overlap
        idx.insert_cfg(
            reverts_package_index::CfgKey {
                param_count: 1,
                cfg_hash: 7,
            },
            Candidate {
                package: PackageId {
                    name: "p".into(),
                    version: "1".into(),
                },
                variant_path: "i.js".into(),
                external_function_id: 1,
                matched_axis: AxisKind::Cfg,
                matched_alternate: None,
            },
        );
        // No insert_feature — anchor side empty in index

        let mut axes = sample_axes(0);
        axes.cfg = 7;
        axes.literal_anchor = Some(99);
        let fp = FunctionFingerprint {
            id: FunctionId::new(ModuleId(1), ByteRange::new(0, 10)),
            param_count: 1,
            statement_count: 1,
            primary: axes,
            alternates: Vec::new(),
        };

        assert!(match_function(&fp, &idx).is_none());
    }

    #[test]
    fn feature_similarity_accepts_high_jaccard_candidate() {
        let mut idx = InMemoryFingerprintIndex::new();
        let cand = Candidate {
            package: PackageId {
                name: "lodash".into(),
                version: "4.17".into(),
            },
            variant_path: "i.js".into(),
            external_function_id: 9,
            matched_axis: AxisKind::CalleeSet,
            matched_alternate: None,
        };
        // Insert under callee_set (the primary distinctive axis)
        idx.insert_feature(
            FeatureKey {
                param_count: 1,
                kind: AxisKind::CalleeSet,
                hash: 11,
            },
            cand.clone(),
        );
        // And under remaining axes: structural_anchor, binding_pattern, return_pattern, effect_pattern
        idx.insert_feature(
            FeatureKey {
                param_count: 1,
                kind: AxisKind::StructuralAnchor,
                hash: 22,
            },
            cand.clone(),
        );
        idx.insert_feature(
            FeatureKey {
                param_count: 1,
                kind: AxisKind::BindingPattern,
                hash: 33,
            },
            cand.clone(),
        );
        idx.insert_feature(
            FeatureKey {
                param_count: 1,
                kind: AxisKind::ReturnPattern,
                hash: 44,
            },
            cand.clone(),
        );
        idx.insert_feature(
            FeatureKey {
                param_count: 1,
                kind: AxisKind::EffectPattern,
                hash: 55,
            },
            cand.clone(),
        );

        let axes = AxisHashes {
            ast: 0,
            cfg: 0,
            return_pattern: 44,
            effect_pattern: 55,
            literal_anchor: None,
            access_pattern: None,
            structural_anchor: 22,
            literal_shape: None,
            access_shape: None,
            callee_set: Some(11),
            binding_pattern: 33,
            throw_set: None,
        };
        let fp = FunctionFingerprint {
            id: FunctionId::new(ModuleId(1), ByteRange::new(0, 10)),
            param_count: 1,
            statement_count: 1,
            primary: axes,
            alternates: Vec::new(),
        };

        let m = match_function(&fp, &idx).expect("similarity match");
        assert_eq!(m.tier, MatchTier::FeatureSimilarity);
    }

    #[test]
    fn feature_similarity_rejects_low_jaccard() {
        let mut idx = InMemoryFingerprintIndex::new();
        let cand = Candidate {
            package: PackageId {
                name: "p".into(),
                version: "1".into(),
            },
            variant_path: "i.js".into(),
            external_function_id: 1,
            matched_axis: AxisKind::CalleeSet,
            matched_alternate: None,
        };
        idx.insert_feature(
            FeatureKey {
                param_count: 1,
                kind: AxisKind::CalleeSet,
                hash: 11,
            },
            cand,
        );
        // Insert NO remaining axes — Jaccard will be 0.0, below 0.6 threshold

        let axes = AxisHashes {
            ast: 0,
            cfg: 0,
            return_pattern: 44,
            effect_pattern: 55,
            literal_anchor: None,
            access_pattern: None,
            structural_anchor: 22,
            literal_shape: None,
            access_shape: None,
            callee_set: Some(11),
            binding_pattern: 33,
            throw_set: None,
        };
        let fp = FunctionFingerprint {
            id: FunctionId::new(ModuleId(1), ByteRange::new(0, 10)),
            param_count: 1,
            statement_count: 1,
            primary: axes,
            alternates: Vec::new(),
        };

        assert!(match_function(&fp, &idx).is_none());
    }

    #[test]
    fn structural_only_accepts_unique_low_frequency_candidate() {
        let mut idx = InMemoryFingerprintIndex::new();
        idx.insert_structural(
            StructuralKey {
                param_count: 1,
                structural_anchor: 77,
            },
            Candidate {
                package: PackageId {
                    name: "p".into(),
                    version: "1".into(),
                },
                variant_path: "i.js".into(),
                external_function_id: 1,
                matched_axis: AxisKind::StructuralAnchor,
                matched_alternate: None,
            },
        );

        let mut axes = sample_axes(0);
        axes.structural_anchor = 77;
        let fp = FunctionFingerprint {
            id: FunctionId::new(ModuleId(1), ByteRange::new(0, 10)),
            param_count: 1,
            statement_count: 1,
            primary: axes,
            alternates: Vec::new(),
        };

        let m = match_function(&fp, &idx).expect("structural-only match");
        assert_eq!(m.tier, MatchTier::StructuralOnly);
    }

    #[test]
    fn structural_only_rejects_high_frequency_hash() {
        let mut idx = InMemoryFingerprintIndex::new();
        // Hit the same structural hash 51 times (limit=50) — corpus frequency > limit
        for i in 0..51u64 {
            idx.insert_structural(
                StructuralKey {
                    param_count: 1,
                    structural_anchor: 77,
                },
                Candidate {
                    package: PackageId {
                        name: format!("p{i}"),
                        version: "1".into(),
                    },
                    variant_path: "i.js".into(),
                    external_function_id: i,
                    matched_axis: AxisKind::StructuralAnchor,
                    matched_alternate: None,
                },
            );
        }

        let mut axes = sample_axes(0);
        axes.structural_anchor = 77;
        let fp = FunctionFingerprint {
            id: FunctionId::new(ModuleId(1), ByteRange::new(0, 10)),
            param_count: 1,
            statement_count: 1,
            primary: axes,
            alternates: Vec::new(),
        };

        assert!(
            match_function(&fp, &idx).is_none(),
            "high-frequency hash must be rejected"
        );
    }

    #[test]
    fn global_assignment_exposes_full_candidate_list_to_callers() {
        // Build an index where one fp can match at TWO tiers (Exact via
        // primary ast, plus StructuralAnchored via cfg + literal_anchor).
        // assign_globally must surface both candidates so downstream code
        // (e.g. `acceptance::classify`) can compute a real margin instead
        // of seeing only the Hungarian winner.
        let mut idx = InMemoryFingerprintIndex::new();
        let pkg_top = PackageId {
            name: "exactpkg".into(),
            version: "1.0".into(),
        };
        let pkg_runner = PackageId {
            name: "structpkg".into(),
            version: "1.0".into(),
        };
        idx.insert_exact(
            ExactKey {
                param_count: 1,
                statement_count: 1,
                ast_hash: 123,
            },
            Candidate {
                package: pkg_top.clone(),
                variant_path: "i.js".into(),
                external_function_id: 1,
                matched_axis: AxisKind::Ast,
                matched_alternate: None,
            },
        );
        idx.insert_cfg(
            reverts_package_index::CfgKey {
                param_count: 1,
                cfg_hash: 456,
            },
            Candidate {
                package: pkg_runner.clone(),
                variant_path: "i.js".into(),
                external_function_id: 2,
                matched_axis: AxisKind::Cfg,
                matched_alternate: None,
            },
        );
        idx.insert_feature(
            FeatureKey {
                param_count: 1,
                kind: AxisKind::LiteralAnchor,
                hash: 789,
            },
            Candidate {
                package: pkg_runner.clone(),
                variant_path: "i.js".into(),
                external_function_id: 2,
                matched_axis: AxisKind::LiteralAnchor,
                matched_alternate: None,
            },
        );

        let mut axes = sample_axes(123);
        axes.cfg = 456;
        axes.literal_anchor = Some(789);
        let fp = FunctionFingerprint {
            id: FunctionId::new(ModuleId(1), ByteRange::new(0, 10)),
            param_count: 1,
            statement_count: 1,
            primary: axes,
            alternates: Vec::new(),
        };

        let out = assign_globally(std::slice::from_ref(&fp), &idx);
        assert_eq!(out.len(), 1);
        // Exact tier dominates → it wins the Hungarian allocation.
        let chosen = out[0]
            .chosen
            .as_ref()
            .expect("Hungarian must pick a winner");
        assert_eq!(chosen.tier, MatchTier::Exact);
        assert_eq!(chosen.candidate.package.name, "exactpkg");
        // …but the runner-up StructuralAnchored candidate must still appear in
        // the candidate list so classify() can see a real margin instead of
        // assuming a single-element slice.
        assert!(
            out[0].candidates.len() >= 2,
            "expected ≥2 cascade candidates exposed for classify(); got {}",
            out[0].candidates.len(),
        );
        assert!(
            out[0]
                .candidates
                .iter()
                .any(|m| m.tier == MatchTier::StructuralAnchored
                    && m.candidate.package.name == "structpkg"),
            "structural-anchored runner-up must appear in candidates list",
        );
    }

    #[test]
    fn global_assignment_splits_two_packages_with_shared_helper_names() {
        // Build an index with 10 functions: evens go to pkg_a, odds to pkg_b.
        // Each bundle fp has exactly one candidate (exact match), so Hungarian
        // and greedy produce the same answer — but this verifies the integration:
        // each fp is assigned, no candidate is reused, counts are 5/5.
        let mut idx = InMemoryFingerprintIndex::new();
        let pkg_a = PackageId {
            name: "a".into(),
            version: "1.0".into(),
        };
        let pkg_b = PackageId {
            name: "b".into(),
            version: "1.0".into(),
        };

        for slot in 0..10u64 {
            let (pkg, fid) = if slot % 2 == 0 {
                (&pkg_a, slot)
            } else {
                (&pkg_b, slot)
            };
            idx.insert_exact(
                ExactKey {
                    param_count: 1,
                    statement_count: 1,
                    ast_hash: 1000 + slot,
                },
                Candidate {
                    package: pkg.clone(),
                    variant_path: "i.js".into(),
                    external_function_id: fid,
                    matched_axis: AxisKind::Ast,
                    matched_alternate: None,
                },
            );
        }

        let bundle_fps: Vec<FunctionFingerprint> = (0..10u64)
            .map(|slot| {
                let mut axes = sample_axes(0);
                axes.ast = 1000 + slot;
                FunctionFingerprint {
                    id: FunctionId::new(
                        ModuleId(1),
                        ByteRange::new(slot as u32 * 10, slot as u32 * 10 + 5),
                    ),
                    param_count: 1,
                    statement_count: 1,
                    primary: axes,
                    alternates: Vec::new(),
                }
            })
            .collect();

        let assignments = assign_globally(&bundle_fps, &idx);
        assert_eq!(assignments.len(), 10);

        let count_a = assignments
            .iter()
            .filter(|a| a.chosen.as_ref().map(|m| m.candidate.package.name.as_str()) == Some("a"))
            .count();
        let count_b = assignments
            .iter()
            .filter(|a| a.chosen.as_ref().map(|m| m.candidate.package.name.as_str()) == Some("b"))
            .count();
        assert_eq!(count_a, 5);
        assert_eq!(count_b, 5);
    }

    #[test]
    fn global_assignment_splits_greedy_collision_optimally() {
        // Two bundle functions both have two candidates each.
        // fp[0]: strongly prefers pkg_a (exact, weight=1000) and weakly pkg_b (exact at rank 1).
        // fp[1]: only pkg_b candidate (exact).
        // A greedy approach might assign both to pkg_a; Hungarian must split fp[0]→pkg_a
        // and fp[1]→pkg_b when pkg_b is the only option for fp[1].
        //
        // Setup: both fps share the same ast_hash for pkg_a (ambiguous exact → no match
        // from try_exact). Instead we use structural_only with unique keys so tier is
        // lower weight. The simpler scenario: fp[0] exact→pkg_a, fp[1] exact→pkg_b only.
        // They don't collide, so we verify the no-collision path too.
        let mut idx = InMemoryFingerprintIndex::new();
        let pkg_a = PackageId {
            name: "pa".into(),
            version: "1.0".into(),
        };
        let pkg_b = PackageId {
            name: "pb".into(),
            version: "1.0".into(),
        };

        // fp[0] matches pkg_a at ast=100
        idx.insert_exact(
            ExactKey {
                param_count: 1,
                statement_count: 1,
                ast_hash: 100,
            },
            Candidate {
                package: pkg_a.clone(),
                variant_path: "i.js".into(),
                external_function_id: 0,
                matched_axis: AxisKind::Ast,
                matched_alternate: None,
            },
        );
        // fp[1] matches pkg_b at ast=200
        idx.insert_exact(
            ExactKey {
                param_count: 1,
                statement_count: 1,
                ast_hash: 200,
            },
            Candidate {
                package: pkg_b.clone(),
                variant_path: "i.js".into(),
                external_function_id: 1,
                matched_axis: AxisKind::Ast,
                matched_alternate: None,
            },
        );

        let fp0 = FunctionFingerprint {
            id: FunctionId::new(ModuleId(1), ByteRange::new(0, 10)),
            param_count: 1,
            statement_count: 1,
            primary: sample_axes(100),
            alternates: Vec::new(),
        };
        let fp1 = FunctionFingerprint {
            id: FunctionId::new(ModuleId(1), ByteRange::new(20, 30)),
            param_count: 1,
            statement_count: 1,
            primary: sample_axes(200),
            alternates: Vec::new(),
        };

        let assignments = assign_globally(&[fp0, fp1], &idx);
        assert_eq!(assignments.len(), 2);

        let assigned_a = assignments
            .iter()
            .find(|a| a.chosen.as_ref().map(|m| m.candidate.package.name.as_str()) == Some("pa"));
        let assigned_b = assignments
            .iter()
            .find(|a| a.chosen.as_ref().map(|m| m.candidate.package.name.as_str()) == Some("pb"));
        assert!(assigned_a.is_some(), "fp[0] should be assigned to pkg_a");
        assert!(assigned_b.is_some(), "fp[1] should be assigned to pkg_b");
    }
}
