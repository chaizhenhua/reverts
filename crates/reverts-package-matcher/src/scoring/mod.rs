mod acceptance;
mod hungarian;
mod tier;
mod version;

pub use acceptance::{AcceptanceDecision, classify};
pub use hungarian::assign_max_weight;
pub use tier::{
    FunctionMatch, STRUCTURAL_FREQUENCY_LIMIT, try_exact, try_exact_alternate,
    try_feature_similarity, try_feature_similarity_alternate, try_structural_anchored,
    try_structural_anchored_alternate, try_structural_only, try_structural_only_alternate,
};
pub(crate) use version::{
    accepted_attribution_from_match, best_source_match, compare_versions,
    disambiguate_exact_source_candidate, module_package_match, score_version,
};
