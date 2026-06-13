use std::collections::BTreeMap;

use crate::variant::VariantSelection;

pub const VERSION_AMBIGUITY_EPSILON: f64 = 0.05;
pub const VERSION_INSUFFICIENT_THRESHOLD: f64 = 300.0;

#[derive(Debug, Clone, PartialEq)]
pub enum BestVersionDecision {
    Selected {
        package_name: String,
        package_version: String,
        score: f64,
        selection: VariantSelection,
    },
    Ambiguous {
        package_name: String,
        scores: Vec<(String, f64)>, // (version, score)
    },
    NoMatch {
        package_name: String,
    },
    InsufficientEvidence {
        package_name: String,
        package_version: String,
        score: f64,
    },
}

/// Picks the best package version per package_name from a flat list of
/// scored variants.
///
/// For each package, scores every version as
/// `variant.score × (matched_function_count / module_function_count)` —
/// the fraction term penalises versions whose matches don't cover the
/// bundle module. Ambiguity within ε of the best score and insufficient
/// evidence below the threshold each get a dedicated decision variant.
///
/// `module_function_count` must reflect how many functions exist in the
/// bundle module these variants were scored against. A value of zero
/// means there is no statistical basis for ranking and every package
/// falls to `InsufficientEvidence` (or `NoMatch` when no variants exist):
/// the function deliberately returns final_score = 0.0 in that case
/// rather than papering over the degenerate input with a magic floor.
#[must_use]
pub fn pick_versions(
    variants: Vec<VariantSelection>,
    module_function_count: usize,
) -> Vec<BestVersionDecision> {
    // Group by package_name. Within each package_name, score per version.
    let mut by_name: BTreeMap<String, Vec<VariantSelection>> = BTreeMap::new();
    for v in variants {
        by_name.entry(v.package.name.clone()).or_default().push(v);
    }

    let mut out = Vec::new();
    for (pkg_name, vars) in by_name {
        // Compute per-version score = variant_score * (matched_count / module_function_count)
        let mut by_version: BTreeMap<String, (VariantSelection, f64)> = BTreeMap::new();
        for v in vars {
            let matched_count = v.function_matches.len();
            let fraction = if module_function_count == 0 {
                0.0
            } else {
                matched_count as f64 / module_function_count as f64
            };
            let final_score = v.score * fraction;
            by_version
                .entry(v.package.version.clone())
                .and_modify(|(_existing, s)| {
                    if final_score > *s {
                        *_existing = v.clone();
                        *s = final_score;
                    }
                })
                .or_insert((v, final_score));
        }

        if by_version.is_empty() {
            out.push(BestVersionDecision::NoMatch {
                package_name: pkg_name,
            });
            continue;
        }

        // Find best score
        let mut all: Vec<(String, VariantSelection, f64)> = by_version
            .into_iter()
            .map(|(ver, (sel, score))| (ver, sel, score))
            .collect();
        all.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

        let (best_ver, best_sel, best_score) = all.first().cloned().expect("non-empty");

        if all.len() > 1 {
            let runner_up_score = all[1].2;
            if (best_score - runner_up_score).abs() < VERSION_AMBIGUITY_EPSILON {
                out.push(BestVersionDecision::Ambiguous {
                    package_name: pkg_name,
                    scores: all.iter().map(|(v, _, s)| (v.clone(), *s)).collect(),
                });
                continue;
            }
        }

        if best_score < VERSION_INSUFFICIENT_THRESHOLD {
            out.push(BestVersionDecision::InsufficientEvidence {
                package_name: pkg_name,
                package_version: best_ver,
                score: best_score,
            });
            continue;
        }

        out.push(BestVersionDecision::Selected {
            package_name: pkg_name,
            package_version: best_ver,
            score: best_score,
            selection: best_sel,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier::FunctionMatch;
    use reverts_ir::MatchTier;
    use reverts_package_index::{Candidate, PackageId};

    fn variant_with_score(ver: &str, n_exact_matches: usize) -> VariantSelection {
        let pkg = PackageId {
            name: "lodash".into(),
            version: ver.into(),
        };
        let fms: Vec<FunctionMatch> = (0..n_exact_matches)
            .map(|i| FunctionMatch {
                tier: MatchTier::Exact,
                candidate: Candidate {
                    package: pkg.clone(),
                    variant_path: "i.js".into(),
                    external_function_id: i as u64,
                    matched_axis: reverts_ir::AxisKind::Ast,
                    matched_alternate: None,
                    external_importable: true,
                },
                margin: 1.0,
                top_score: f64::from(MatchTier::Exact.weight()),
                runner_up_score: 0.0,
                matched_alternate: None,
                matched_axes: vec![reverts_ir::AxisKind::Ast],
            })
            .collect();
        let tier_sum: f64 = fms.iter().map(|m| f64::from(m.tier.weight())).sum();
        VariantSelection {
            package: pkg,
            variant_path: "i.js".into(),
            tier_weight_sum: tier_sum,
            jaccard: 1.0,
            score: tier_sum + 100.0,
            function_matches: fms,
        }
    }

    #[test]
    fn pick_versions_selects_unique_winner() {
        let variants = vec![
            variant_with_score("1.0", 1),
            variant_with_score("2.0", 5),
            variant_with_score("3.0", 2),
        ];
        let decisions = pick_versions(variants, 5);
        assert_eq!(decisions.len(), 1);
        match &decisions[0] {
            BestVersionDecision::Selected {
                package_version, ..
            } => {
                assert_eq!(package_version, "2.0");
            }
            other => panic!("expected Selected, got {other:?}"),
        }
    }

    #[test]
    fn pick_versions_emits_ambiguous_for_tied_scores() {
        let variants = vec![variant_with_score("1.0", 5), variant_with_score("2.0", 5)];
        let decisions = pick_versions(variants, 5);
        assert_eq!(decisions.len(), 1);
        assert!(matches!(
            &decisions[0],
            BestVersionDecision::Ambiguous { .. }
        ));
    }

    #[test]
    fn pick_versions_emits_insufficient_evidence_below_threshold() {
        // Single match with low tier → low total score
        let mut variants = vec![variant_with_score("1.0", 1)];
        variants[0].score = 50.0; // below 300 threshold
        variants[0].function_matches.truncate(1);
        let decisions = pick_versions(variants, 100); // huge module count → small fraction
        assert_eq!(decisions.len(), 1);
        assert!(matches!(
            &decisions[0],
            BestVersionDecision::InsufficientEvidence { .. }
        ));
    }

    fn variant_with_score_for(
        pkg_name: &str,
        ver: &str,
        n_exact_matches: usize,
    ) -> VariantSelection {
        let mut v = variant_with_score(ver, n_exact_matches);
        v.package = PackageId {
            name: pkg_name.into(),
            version: ver.into(),
        };
        for fm in &mut v.function_matches {
            fm.candidate.package = v.package.clone();
        }
        v
    }

    #[test]
    fn pick_versions_returns_empty_for_empty_input() {
        // No variants at all → no decisions to emit. Mirrors legacy's
        // VersionSearchStrategy::new returning Err on empty registry.
        let decisions = pick_versions(Vec::new(), 5);
        assert!(decisions.is_empty());
    }

    #[test]
    fn pick_versions_decides_each_package_independently() {
        // Multi-package input: one Selected, one Ambiguous, one InsufficientEvidence
        // — each package gets its own decision, no cross-package interference.
        let mut variants = vec![
            // pkg "a": clean winner at 2.0
            variant_with_score_for("a", "1.0", 1),
            variant_with_score_for("a", "2.0", 5),
            // pkg "b": tied 1.0 and 2.0 → Ambiguous
            variant_with_score_for("b", "1.0", 5),
            variant_with_score_for("b", "2.0", 5),
            // pkg "c": single low-tier match → InsufficientEvidence
            variant_with_score_for("c", "1.0", 1),
        ];
        // Knock pkg "c"'s score below the threshold.
        for v in &mut variants {
            if v.package.name == "c" {
                v.score = 50.0;
            }
        }

        let decisions = pick_versions(variants, 5);
        assert_eq!(decisions.len(), 3);

        let by_name: std::collections::BTreeMap<&str, &BestVersionDecision> = decisions
            .iter()
            .map(|d| {
                let name = match d {
                    BestVersionDecision::Selected { package_name, .. }
                    | BestVersionDecision::Ambiguous { package_name, .. }
                    | BestVersionDecision::NoMatch { package_name }
                    | BestVersionDecision::InsufficientEvidence { package_name, .. } => {
                        package_name.as_str()
                    }
                };
                (name, d)
            })
            .collect();

        assert!(matches!(
            by_name.get("a"),
            Some(BestVersionDecision::Selected { package_version, .. }) if package_version == "2.0"
        ));
        assert!(matches!(
            by_name.get("b"),
            Some(BestVersionDecision::Ambiguous { .. })
        ));
        assert!(matches!(
            by_name.get("c"),
            Some(BestVersionDecision::InsufficientEvidence { .. })
        ));
    }

    #[test]
    fn pick_versions_with_zero_module_function_count_never_selects() {
        // Degenerate input: the bundle module has no functions, so we have
        // no statistical basis to rank versions. After dropping the magic
        // `.max(0.001)` floor, all scores collapse to 0.0 and pick_versions
        // refuses to commit (Ambiguous when ≥2 versions, InsufficientEvidence
        // when one). Neither outcome must be `Selected`.
        //
        // Mirrors legacy `test_version_match_result_zero_total` checking
        // that zero-total inputs do not produce a positive match_rate.
        let variants = vec![variant_with_score("1.0", 5), variant_with_score("2.0", 5)];
        let decisions = pick_versions(variants, 0);
        assert_eq!(decisions.len(), 1);
        assert!(
            !matches!(&decisions[0], BestVersionDecision::Selected { .. }),
            "zero module_function_count must not yield Selected, got {:?}",
            decisions[0],
        );
    }
}
