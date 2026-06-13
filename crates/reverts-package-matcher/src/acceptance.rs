use reverts_input::AttributionConfidence;
use reverts_ir::MatchTier;

use crate::tier::FunctionMatch;

#[derive(Debug, Clone, PartialEq)]
pub enum AcceptanceDecision {
    Accepted {
        confidence: AttributionConfidence,
    },
    AcceptedWithCaveat {
        confidence: AttributionConfidence,
    },
    Ambiguous {
        top_score: f64,
        runner_up_score: f64,
        margin: f64,
    },
    NoMatch,
}

const MARGIN_ACCEPTED_LOW_TIER: f64 = 0.3;
const MARGIN_CAVEAT_FLOOR: f64 = 0.1;

#[must_use]
pub fn classify(matches: &[FunctionMatch]) -> AcceptanceDecision {
    if matches.is_empty() {
        return AcceptanceDecision::NoMatch;
    }

    // Sort descending by top_score so element 0 is the winner.
    let mut sorted = matches.to_vec();
    sorted.sort_by(|a, b| {
        b.top_score
            .partial_cmp(&a.top_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let winner = &sorted[0];
    let runner_up_score = sorted.get(1).map_or(0.0, |m| m.top_score);
    let margin = if winner.top_score == 0.0 {
        0.0
    } else {
        (winner.top_score - runner_up_score) / winner.top_score
    };

    // Tier-based gating
    let is_high_tier = matches!(winner.tier, MatchTier::Exact | MatchTier::ExactAlternate);
    // For Exact / ExactAlternate, single unique winner is enough.
    if is_high_tier && sorted.len() == 1 {
        return AcceptanceDecision::Accepted {
            confidence: build_confidence(winner, margin, runner_up_score),
        };
    }

    if margin >= MARGIN_ACCEPTED_LOW_TIER {
        return AcceptanceDecision::Accepted {
            confidence: build_confidence(winner, margin, runner_up_score),
        };
    }

    if margin >= MARGIN_CAVEAT_FLOOR {
        return AcceptanceDecision::AcceptedWithCaveat {
            confidence: build_confidence(winner, margin, runner_up_score),
        };
    }

    AcceptanceDecision::Ambiguous {
        top_score: winner.top_score,
        runner_up_score,
        margin,
    }
}

fn build_confidence(
    winner: &FunctionMatch,
    margin: f64,
    runner_up_score: f64,
) -> AttributionConfidence {
    AttributionConfidence {
        tier: winner.tier,
        matched_axes: winner.matched_axes.clone(),
        matched_alternate: winner.matched_alternate,
        top_score: winner.top_score,
        runner_up_score,
        margin: margin.clamp(0.0, 1.0),
    }
}

#[cfg(test)]
mod tests {
    use reverts_ir::AxisKind;

    use super::*;
    use reverts_package_index::{Candidate, PackageId};

    fn fn_match(tier: MatchTier, top_score: f64) -> FunctionMatch {
        FunctionMatch {
            tier,
            candidate: Candidate {
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
            margin: 1.0,
            top_score,
            runner_up_score: 0.0,
            matched_alternate: None,
            matched_axes: vec![AxisKind::Ast],
        }
    }

    #[test]
    fn classify_no_matches_yields_no_match() {
        assert!(matches!(classify(&[]), AcceptanceDecision::NoMatch));
    }

    #[test]
    fn classify_exact_unique_winner_is_accepted() {
        let d = classify(&[fn_match(MatchTier::Exact, 10_000.0)]);
        assert!(matches!(d, AcceptanceDecision::Accepted { .. }));
    }

    #[test]
    fn classify_low_tier_high_margin_is_accepted() {
        let d = classify(&[
            fn_match(MatchTier::StructuralAnchored, 1000.0),
            fn_match(MatchTier::StructuralAnchored, 400.0),
        ]);
        // margin = (1000 - 400) / 1000 = 0.6 >= 0.3 ⇒ Accepted
        assert!(matches!(d, AcceptanceDecision::Accepted { .. }));
    }

    #[test]
    fn classify_low_tier_medium_margin_is_caveat() {
        let d = classify(&[
            fn_match(MatchTier::StructuralAnchored, 1000.0),
            fn_match(MatchTier::StructuralAnchored, 800.0),
        ]);
        // margin = 0.2 ⇒ in [0.1, 0.3) ⇒ AcceptedWithCaveat
        assert!(matches!(d, AcceptanceDecision::AcceptedWithCaveat { .. }));
    }

    #[test]
    fn classify_low_tier_low_margin_is_ambiguous() {
        let d = classify(&[
            fn_match(MatchTier::StructuralAnchored, 1000.0),
            fn_match(MatchTier::StructuralAnchored, 950.0),
        ]);
        // margin = 0.05 < 0.1 ⇒ Ambiguous
        assert!(matches!(d, AcceptanceDecision::Ambiguous { .. }));
    }
}
