use reverts_ir::FunctionFingerprint;
use reverts_package_index::PackageFingerprintIndex;

use crate::tier::{FunctionMatch, try_exact, try_exact_alternate};

#[must_use]
pub fn match_function(
    fp: &FunctionFingerprint,
    index: &dyn PackageFingerprintIndex,
) -> Option<FunctionMatch> {
    try_exact(fp, index).or_else(|| try_exact_alternate(fp, index))
    // Tier 2-4 added in subsequent tasks.
}

#[cfg(test)]
mod tests {
    use super::*;
    use reverts_ir::{
        AxisHashes, AxisKind, ByteRange, FunctionId, MatchTier, ModuleId, NormalizationPassId,
    };
    use reverts_package_index::{Candidate, ExactKey, InMemoryFingerprintIndex, PackageId};

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
}
