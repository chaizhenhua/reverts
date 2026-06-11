use std::collections::BTreeMap;

use reverts_package_index::PackageId;

use crate::tier::FunctionMatch;

/// Score for one variant within a (package, version) pair.
#[derive(Debug, Clone, PartialEq)]
pub struct VariantSelection {
    pub package: PackageId,
    pub variant_path: String,
    pub tier_weight_sum: f64,
    pub jaccard: f64,
    pub score: f64,
    pub function_matches: Vec<FunctionMatch>,
}

const JACCARD_ALPHA: f64 = 100.0;

/// Group function matches by (package, variant_path), score each variant,
/// and return the best variant per (package, version) pair.
#[must_use]
pub fn pick_variants(
    matches: &[FunctionMatch],
    bundle_function_count: usize,
) -> Vec<VariantSelection> {
    // Group matches by (package, variant_path)
    let mut by_key: BTreeMap<(PackageId, String), Vec<FunctionMatch>> = BTreeMap::new();
    for m in matches {
        by_key
            .entry((
                m.candidate.package.clone(),
                m.candidate.variant_path.clone(),
            ))
            .or_default()
            .push(m.clone());
    }

    let mut all: Vec<VariantSelection> = by_key
        .into_iter()
        .map(|((package, variant_path), fms)| {
            let tier_sum: f64 = fms.iter().map(|m| f64::from(m.tier.weight())).sum();
            let matched_count = fms.len();
            let union = bundle_function_count.max(matched_count);
            let jaccard = if union == 0 {
                0.0
            } else {
                matched_count as f64 / union as f64
            };
            let score = tier_sum + JACCARD_ALPHA * jaccard;
            VariantSelection {
                package,
                variant_path,
                tier_weight_sum: tier_sum,
                jaccard,
                score,
                function_matches: fms,
            }
        })
        .collect();

    // Best per (package, version) — group again on package, pick max score
    let mut by_package: BTreeMap<PackageId, Vec<VariantSelection>> = BTreeMap::new();
    for sel in all.drain(..) {
        by_package.entry(sel.package.clone()).or_default().push(sel);
    }

    let mut result = Vec::new();
    for (_pkg, mut variants) in by_package {
        variants.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    variant_preference_rank(&a.variant_path)
                        .cmp(&variant_preference_rank(&b.variant_path))
                })
        });
        if let Some(best) = variants.into_iter().next() {
            result.push(best);
        }
    }
    result
}

/// Lower rank = higher preference. Used to break ties between variants
/// with identical scores. Path-name heuristic: browser > module > main >
/// umd > main-cjs > everything else.
fn variant_preference_rank(path: &str) -> u8 {
    let lc = path.to_ascii_lowercase();
    if lc.contains("browser") {
        0
    } else if lc.contains("module") || lc.contains(".mjs") || lc.contains("esm") {
        1
    } else if lc.contains("main") && !lc.contains("cjs") {
        2
    } else if lc.contains("umd") {
        3
    } else if lc.contains("cjs") {
        4
    } else {
        5
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reverts_ir::MatchTier;
    use reverts_package_index::Candidate;

    fn match_at(pkg: &str, variant: &str, fid: u64, tier: MatchTier) -> FunctionMatch {
        FunctionMatch {
            tier,
            candidate: Candidate {
                package: PackageId {
                    name: pkg.into(),
                    version: "1.0".into(),
                },
                variant_path: variant.into(),
                external_function_id: fid,
                matched_axis: reverts_ir::AxisKind::Ast,
                matched_alternate: None,
            },
            margin: 1.0,
            top_score: f64::from(tier.weight()),
            runner_up_score: 0.0,
            matched_alternate: None,
            matched_axes: vec![reverts_ir::AxisKind::Ast],
        }
    }

    #[test]
    fn variant_with_more_tier_weight_wins() {
        let matches = vec![
            match_at("pkg", "main.js", 1, MatchTier::Exact),
            match_at("pkg", "main.js", 2, MatchTier::Exact),
            match_at("pkg", "browser.js", 3, MatchTier::StructuralAnchored),
        ];
        let picks = pick_variants(&matches, 5);
        assert_eq!(picks.len(), 1);
        assert_eq!(picks[0].variant_path, "main.js");
    }

    #[test]
    fn tie_broken_by_browser_preference() {
        let matches = vec![
            match_at("pkg", "main.js", 1, MatchTier::Exact),
            match_at("pkg", "browser.js", 2, MatchTier::Exact),
        ];
        let picks = pick_variants(&matches, 1);
        assert_eq!(picks.len(), 1);
        assert_eq!(picks[0].variant_path, "browser.js");
    }

    #[test]
    fn pick_variants_returns_empty_when_no_matches() {
        let picks = pick_variants(&[], 0);
        assert!(picks.is_empty());
    }
}
