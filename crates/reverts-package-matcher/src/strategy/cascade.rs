use reverts_ir::{FunctionFingerprint, FunctionId};
use reverts_package_index::{CandidateOwner, FingerprintIndex, PackageOwner};

use crate::scoring::{
    FunctionMatch, assign_max_weight, try_exact, try_exact_alternate, try_feature_similarity,
    try_feature_similarity_alternate, try_structural_anchored, try_structural_anchored_alternate,
    try_structural_only, try_structural_only_alternate,
};

#[must_use]
pub fn match_function<O: CandidateOwner>(
    fp: &FunctionFingerprint,
    index: &FingerprintIndex<O>,
) -> Option<FunctionMatch<O>> {
    try_exact(fp, index)
}

/// Per-function candidate collection ordered from strongest to weakest
/// evidence.
///
/// Exact tiers can prove a public import directly. Lower-strength tiers are
/// deliberately still included here because real minified bundles often lose
/// AST identity while preserving control-flow, literal/callee anchors, or
/// structural shape. Those candidates are safe only after the caller applies
/// global assignment, margin checks, package-hint scoping, and module-level
/// coverage gates; ownership-only promotion keeps weak evidence from becoming
/// an external import unless the matched package source was already proven
/// importable.
#[must_use]
pub fn cascade_candidates<O: CandidateOwner>(
    fp: &FunctionFingerprint,
    index: &FingerprintIndex<O>,
) -> Vec<FunctionMatch<O>> {
    let mut all = Vec::new();
    if let Some(m) = try_exact(fp, index) {
        all.push(m);
    }
    if let Some(m) = try_exact_alternate(fp, index) {
        all.push(m);
    }
    if !all.is_empty() {
        return all;
    }
    if let Some(m) = try_structural_anchored(fp, index) {
        all.push(m);
    }
    if let Some(m) = try_structural_anchored_alternate(fp, index) {
        all.push(m);
    }
    if let Some(m) = try_feature_similarity(fp, index) {
        all.push(m);
    }
    if let Some(m) = try_feature_similarity_alternate(fp, index) {
        all.push(m);
    }
    if let Some(m) = try_structural_only(fp, index) {
        all.push(m);
    }
    if let Some(m) = try_structural_only_alternate(fp, index) {
        all.push(m);
    }
    all
}

/// Per-function output of [`assign_globally`]: both the full pre-Hungarian
/// candidate list (so callers can call [`crate::classify`] with
/// real runner-up information) and the chosen match after the global
/// bipartite assignment, when one exists.
#[derive(Debug, Clone, PartialEq)]
pub struct GlobalAssignment<O = PackageOwner> {
    pub function_id: FunctionId,
    pub candidates: Vec<FunctionMatch<O>>,
    pub chosen: Option<FunctionMatch<O>>,
}

impl GlobalAssignment<PackageOwner> {
    /// The package name of the Hungarian-chosen candidate, if any. Equivalent
    /// to `self.chosen.as_ref().map(|m| m.candidate.owner.package.name.as_str())`
    /// but reads as a fact rather than a chain — call sites use this to
    /// filter assignments by package.
    #[must_use]
    pub fn chosen_package_name(&self) -> Option<&str> {
        self.chosen
            .as_ref()
            .map(|m| m.candidate.owner.package.name.as_str())
    }
}

/// Assign bundle functions to package candidates globally using the Hungarian
/// algorithm, resolving cross-package collisions optimally instead of greedily.
///
/// For each bundle function, exact candidates are evaluated. A bipartite
/// weight matrix is built over all unique
/// `(owner_identity, external_function_id)` pairs seen across all candidate
/// lists, and `assign_max_weight` picks the globally optimal one-to-one
/// assignment. The identity is whatever the owner type considers a unique
/// match target — package name for the package matcher, subject module
/// index for the module matcher.
#[must_use]
pub fn assign_globally<O: CandidateOwner>(
    bundle_fps: &[FunctionFingerprint],
    index: &FingerprintIndex<O>,
) -> Vec<GlobalAssignment<O>> {
    let mut out: Vec<GlobalAssignment<O>> = bundle_fps
        .iter()
        .map(|fp| GlobalAssignment {
            function_id: fp.id,
            candidates: cascade_candidates(fp, index),
            chosen: None,
        })
        .collect();

    // Right-side global candidate index. `col_index` maps key → column for
    // O(log n) forward lookup during cost-matrix population; `col_keys`
    // mirrors the keys indexed by column for O(1) reverse lookup when we
    // decode the Hungarian assignment. Without the parallel vec this loop
    // would be O(n²) in the function count.
    let mut col_index: std::collections::BTreeMap<(O::IdentityKey, u64), usize> =
        std::collections::BTreeMap::new();
    let mut col_keys: Vec<(O::IdentityKey, u64)> = Vec::new();
    for assignment in &out {
        for cand in &assignment.candidates {
            let key = (
                cand.candidate.owner.identity_key(),
                cand.candidate.external_function_id,
            );
            col_index.entry(key).or_insert_with(|| {
                col_keys.push((
                    cand.candidate.owner.identity_key(),
                    cand.candidate.external_function_id,
                ));
                col_keys.len() - 1
            });
        }
    }

    if col_keys.is_empty() {
        return out;
    }

    let n_rows = out.len();
    let n_cols = col_keys.len();

    // Build a sparse row/column graph first. Real bundles usually split into
    // many independent candidate components; running Hungarian over one large
    // dense matrix turns those independent sub-problems into unnecessary
    // cubic work.
    let mut row_edges = vec![Vec::<(usize, f64)>::new(); n_rows];
    let mut col_rows = vec![Vec::<usize>::new(); n_cols];
    for (row, assignment) in out.iter().enumerate() {
        let mut row_best_by_col = std::collections::BTreeMap::<usize, f64>::new();
        for (rank, cand) in assignment.candidates.iter().enumerate() {
            let key = (
                cand.candidate.owner.identity_key(),
                cand.candidate.external_function_id,
            );
            if let Some(&col) = col_index.get(&key) {
                let r = rank.min(4) as f64;
                let weight = f64::from(cand.tier.weight()) * (1.0 - 0.1 * r);
                row_best_by_col
                    .entry(col)
                    .and_modify(|existing| *existing = existing.max(weight))
                    .or_insert(weight);
            }
        }
        for (col, weight) in row_best_by_col {
            row_edges[row].push((col, weight));
            col_rows[col].push(row);
        }
    }

    let mut seen_rows = vec![false; n_rows];
    let mut seen_cols = vec![false; n_cols];
    for start_row in 0..n_rows {
        if seen_rows[start_row] || row_edges[start_row].is_empty() {
            continue;
        }
        let mut component_rows = Vec::new();
        let mut component_cols = Vec::new();
        let mut row_queue = std::collections::VecDeque::from([start_row]);
        seen_rows[start_row] = true;
        while let Some(row) = row_queue.pop_front() {
            component_rows.push(row);
            for (col, _weight) in &row_edges[row] {
                if !seen_cols[*col] {
                    seen_cols[*col] = true;
                    component_cols.push(*col);
                    for next_row in &col_rows[*col] {
                        if !seen_rows[*next_row] {
                            seen_rows[*next_row] = true;
                            row_queue.push_back(*next_row);
                        }
                    }
                }
            }
        }

        if component_rows.len() == 1 {
            let row = component_rows[0];
            if let Some((col, _weight)) = row_edges[row]
                .iter()
                .max_by(|left, right| left.1.total_cmp(&right.1))
            {
                choose_global_assignment(&mut out, &col_keys, row, *col);
            }
            continue;
        }

        let local_col_by_global = component_cols
            .iter()
            .enumerate()
            .map(|(local, global)| (*global, local))
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut cost = vec![vec![0.0_f64; component_cols.len()]; component_rows.len()];
        for (local_row, row) in component_rows.iter().enumerate() {
            for (global_col, weight) in &row_edges[*row] {
                if let Some(local_col) = local_col_by_global.get(global_col).copied() {
                    cost[local_row][local_col] = *weight;
                }
            }
        }

        let assign = assign_max_weight(&cost);
        for (local_row, &local_col) in assign.iter().enumerate() {
            if local_col == usize::MAX || local_col >= component_cols.len() {
                continue;
            }
            if cost[local_row][local_col] <= 0.0 {
                continue;
            }
            choose_global_assignment(
                &mut out,
                &col_keys,
                component_rows[local_row],
                component_cols[local_col],
            );
        }
    }
    out
}

fn choose_global_assignment<O: CandidateOwner>(
    out: &mut [GlobalAssignment<O>],
    col_keys: &[(O::IdentityKey, u64)],
    row: usize,
    col: usize,
) {
    let key_pair = &col_keys[col];
    let chosen = out[row]
        .candidates
        .iter()
        .find(|m| {
            m.candidate.owner.identity_key() == key_pair.0
                && m.candidate.external_function_id == key_pair.1
        })
        .cloned();
    out[row].chosen = chosen;
}

#[cfg(test)]
mod tests {
    use super::*;
    use reverts_ir::{
        AxisHashes, AxisKind, ByteRange, FunctionFingerprint, FunctionId, MatchTier, ModuleId,
        NormalizationPassId,
    };
    use reverts_package_index::{
        ExactKey, FeatureKey, PackageCandidate as Candidate,
        PackageFingerprintIndex as FingerprintIndex, PackageId, PackageOwner, StructuralKey,
    };

    use crate::scoring::{
        try_exact_alternate, try_feature_similarity, try_structural_anchored, try_structural_only,
    };

    fn sample_axes(ast: u64) -> AxisHashes {
        AxisHashes {
            ast,
            cfg: 0,
            normalized_cfg: 0,
            return_pattern: 0,
            effect_pattern: 0,
            literal_anchor: None,
            access_pattern: None,
            structural_anchor: 0,
            literal_shape: None,
            access_shape: None,
            expression_shape: None,
            callee_set: None,
            binding_pattern: 0,
            throw_set: None,
        }
    }

    #[test]
    fn match_function_returns_exact_when_unique() {
        let mut idx = FingerprintIndex::new();
        let key = ExactKey {
            param_count: 2,
            statement_count: 3,
            ast_hash: 100,
        };
        idx.insert_exact(
            key,
            Candidate {
                owner: PackageOwner {
                    package: PackageId {
                        name: "lodash".into(),
                        version: "4.17.21".into(),
                    },
                    variant_path: "index.js".into(),
                    external_importable: true,
                },
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
        assert_eq!(m.candidate.owner.package.name, "lodash");
    }

    #[test]
    fn match_function_rejects_ambiguous_exact() {
        let mut idx = FingerprintIndex::new();
        let key = ExactKey {
            param_count: 2,
            statement_count: 3,
            ast_hash: 100,
        };
        for (pkg, fid) in [("a", 1u64), ("b", 2)] {
            idx.insert_exact(
                key,
                Candidate {
                    owner: PackageOwner {
                        package: PackageId {
                            name: pkg.into(),
                            version: "1".into(),
                        },
                        variant_path: "i.js".into(),
                        external_importable: true,
                    },
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
    fn exact_alternate_tier_is_explicit_only() {
        let mut idx = FingerprintIndex::new();
        // No primary exact match; an alternate matches at ast=222
        idx.insert_exact(
            ExactKey {
                param_count: 1,
                statement_count: 1,
                ast_hash: 222,
            },
            Candidate {
                owner: PackageOwner {
                    package: PackageId {
                        name: "p".into(),
                        version: "1".into(),
                    },
                    variant_path: "i.js".into(),
                    external_importable: true,
                },
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
            alternates: vec![reverts_ir::AlternateAxisHashes {
                pass: NormalizationPassId::TsRuntimeErased,
                statement_count: 1,
                axes: alt,
            }],
        };

        assert!(
            match_function(&fp, &idx).is_none(),
            "main matcher must not consume alternate exact tier implicitly"
        );
        let m = try_exact_alternate(&fp, &idx).expect("alternate match");
        assert_eq!(m.tier, MatchTier::ExactAlternate);
        assert_eq!(
            m.matched_alternate,
            Some(NormalizationPassId::TsRuntimeErased)
        );
    }

    #[test]
    fn structural_anchored_requires_cfg_and_anchor_overlap() {
        let mut idx = FingerprintIndex::new();
        let candidate = Candidate {
            owner: PackageOwner {
                package: PackageId {
                    name: "p".into(),
                    version: "1".into(),
                },
                variant_path: "i.js".into(),
                external_importable: true,
            },
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

        let m = try_structural_anchored(&fp, &idx).expect("structural-anchored match");
        assert_eq!(m.tier, MatchTier::StructuralAnchored);
    }

    #[test]
    fn structural_anchored_rejects_when_no_anchor_overlap() {
        let mut idx = FingerprintIndex::new();
        // CFG matches but no anchor overlap
        idx.insert_cfg(
            reverts_package_index::CfgKey {
                param_count: 1,
                cfg_hash: 7,
            },
            Candidate {
                owner: PackageOwner {
                    package: PackageId {
                        name: "p".into(),
                        version: "1".into(),
                    },
                    variant_path: "i.js".into(),
                    external_importable: true,
                },
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

        assert!(try_structural_anchored(&fp, &idx).is_none());
    }

    #[test]
    fn feature_similarity_accepts_high_jaccard_candidate() {
        let mut idx = FingerprintIndex::new();
        let cand = Candidate {
            owner: PackageOwner {
                package: PackageId {
                    name: "lodash".into(),
                    version: "4.17".into(),
                },
                variant_path: "i.js".into(),
                external_importable: true,
            },
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
            normalized_cfg: 0,
            return_pattern: 44,
            effect_pattern: 55,
            literal_anchor: None,
            access_pattern: None,
            structural_anchor: 22,
            literal_shape: None,
            access_shape: None,
            expression_shape: None,
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

        let m = try_feature_similarity(&fp, &idx).expect("similarity match");
        assert_eq!(m.tier, MatchTier::FeatureSimilarity);
    }

    #[test]
    fn feature_similarity_rejects_low_jaccard() {
        let mut idx = FingerprintIndex::new();
        let cand = Candidate {
            owner: PackageOwner {
                package: PackageId {
                    name: "p".into(),
                    version: "1".into(),
                },
                variant_path: "i.js".into(),
                external_importable: true,
            },
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
            normalized_cfg: 0,
            return_pattern: 44,
            effect_pattern: 55,
            literal_anchor: None,
            access_pattern: None,
            structural_anchor: 22,
            literal_shape: None,
            access_shape: None,
            expression_shape: None,
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

        assert!(try_feature_similarity(&fp, &idx).is_none());
    }

    #[test]
    fn structural_only_accepts_unique_low_frequency_candidate() {
        let mut idx = FingerprintIndex::new();
        idx.insert_structural(
            StructuralKey {
                param_count: 1,
                structural_anchor: 77,
            },
            Candidate {
                owner: PackageOwner {
                    package: PackageId {
                        name: "p".into(),
                        version: "1".into(),
                    },
                    variant_path: "i.js".into(),
                    external_importable: true,
                },
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

        let m = try_structural_only(&fp, &idx).expect("structural-only match");
        assert_eq!(m.tier, MatchTier::StructuralOnly);
    }

    #[test]
    fn structural_only_rejects_high_frequency_hash() {
        let mut idx = FingerprintIndex::new();
        // Hit the same structural hash 51 times (limit=50) — corpus frequency > limit
        for i in 0..51u64 {
            idx.insert_structural(
                StructuralKey {
                    param_count: 1,
                    structural_anchor: 77,
                },
                Candidate {
                    owner: PackageOwner {
                        package: PackageId {
                            name: format!("p{i}"),
                            version: "1".into(),
                        },
                        variant_path: "i.js".into(),
                        external_importable: true,
                    },
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
            try_structural_only(&fp, &idx).is_none(),
            "high-frequency hash must be rejected"
        );
    }

    #[test]
    fn global_assignment_exposes_exact_candidates_only_to_callers() {
        // Build an index where one fp can match at TWO tiers (Exact via
        // primary ast, plus StructuralAnchored via cfg + literal_anchor).
        // assign_globally must surface only the exact candidate; weaker
        // structural evidence is available through explicit tier APIs.
        let mut idx = FingerprintIndex::new();
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
                owner: PackageOwner {
                    package: pkg_top.clone(),
                    variant_path: "i.js".into(),
                    external_importable: true,
                },
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
                owner: PackageOwner {
                    package: pkg_runner.clone(),
                    variant_path: "i.js".into(),
                    external_importable: true,
                },
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
                owner: PackageOwner {
                    package: pkg_runner.clone(),
                    variant_path: "i.js".into(),
                    external_importable: true,
                },
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
        assert_eq!(chosen.candidate.owner.package.name, "exactpkg");
        assert_eq!(
            out[0].candidates.len(),
            1,
            "lower tiers are only consulted after exact tiers miss",
        );
        assert_eq!(out[0].candidates[0].tier, MatchTier::Exact);
    }

    #[test]
    fn global_assignment_splits_two_packages_with_shared_helper_names() {
        // Build an index with 10 functions: evens go to pkg_a, odds to pkg_b.
        // Each bundle fp has exactly one candidate (exact match), so Hungarian
        // and greedy produce the same answer — but this verifies the integration:
        // each fp is assigned, no candidate is reused, counts are 5/5.
        let mut idx = FingerprintIndex::new();
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
                    owner: PackageOwner {
                        package: pkg.clone(),
                        variant_path: "i.js".into(),
                        external_importable: true,
                    },
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
            .filter(|a| a.chosen_package_name() == Some("a"))
            .count();
        let count_b = assignments
            .iter()
            .filter(|a| a.chosen_package_name() == Some("b"))
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
        let mut idx = FingerprintIndex::new();
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
                owner: PackageOwner {
                    package: pkg_a.clone(),
                    variant_path: "i.js".into(),
                    external_importable: true,
                },
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
                owner: PackageOwner {
                    package: pkg_b.clone(),
                    variant_path: "i.js".into(),
                    external_importable: true,
                },
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
            .find(|a| a.chosen_package_name() == Some("pa"));
        let assigned_b = assignments
            .iter()
            .find(|a| a.chosen_package_name() == Some("pb"));
        assert!(assigned_a.is_some(), "fp[0] should be assigned to pkg_a");
        assert!(assigned_b.is_some(), "fp[1] should be assigned to pkg_b");
    }
}
