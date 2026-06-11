use reverts_ir::FunctionFingerprint;
use reverts_package_index::PackageFingerprintIndex;

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

#[cfg(test)]
mod tests {
    use super::*;
    use reverts_ir::{
        AxisHashes, AxisKind, ByteRange, FunctionId, MatchTier, ModuleId, NormalizationPassId,
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
}
