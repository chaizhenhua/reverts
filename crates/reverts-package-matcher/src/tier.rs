use reverts_ir::{FunctionFingerprint, MatchTier, NormalizationPassId};
use reverts_package_index::{Candidate, ExactKey, PackageFingerprintIndex};

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
