mod api;
mod binding_signatures;
mod graph_neighborhood;
mod index;
mod matcher;
mod model;
mod ownership;
pub mod package_helpers;
mod pipeline;
mod proof;
mod scoring;
mod source;
mod strategy;

pub use api::{
    package_source_exported_members, package_source_normalized_hash,
    package_source_normalized_hashes,
};
pub use graph_neighborhood::{GraphNeighborhoodEvidence, graph_neighborhood_support};
pub(crate) use index::ExternalImportSourceIndex;
pub use index::package_module_source_quality;
pub use index::{SourceFingerprint, fingerprint_source};
pub(crate) use index::{module_match_fingerprint, package_source_fingerprint};
pub use matcher::VersionedPackageMatcher;
pub(crate) use matcher::has_accepted_attribution;
pub(crate) use model::PACKAGE_SOURCE_FINGERPRINT_MAX_BYTES;
pub use model::{
    BestVersionMatch, ModuleMatchFingerprint, ModuleMatchStrategy, ModulePackageMatch,
    PackageImportSite, PackageMatch, PackageMatchingPipelineReport, PackageModuleSourceQuality,
    PackagePublicExportProof, PackageSource, PackageSourceFingerprint, PackageVersionCandidate,
    SourcePackageImportParseError, VersionMatchScore, VersionedPackageMatchReport,
    VersionedPackageMatcherConfig,
};
pub(crate) use model::{
    ConcretePackageSourcePath, CorrectedPackageExternalImportTarget, ExternalImportTarget,
    package_module_source_quality_label,
};
pub(crate) use ownership::dependency_closure::{
    DependencyNeighborhoodEvidence, dependency_neighborhood_ownership_evidence,
    dependency_neighborhood_source_path, has_direct_neighborhood_package_contradiction,
    package_dependency_components,
};
pub use package_helpers::{
    SemanticPathHintMode, accepted_external_modules, canonical_public_path_segments,
    clean_package_semantic_path_hint, direct_module_dependencies, direct_module_dependents,
    has_accepted_external_attribution, is_build_path_segment, is_exact_package_version_hint,
    is_json_source_path, module_package_semantic_path_hints, normalize_hint_text,
    ownership_by_module, package_semantic_path_prefixes, package_source_entry_path,
    package_source_export_path, package_source_external_import_rank, package_source_relative_path,
    package_source_semantic_hint_score, package_source_semantic_surface_hint_score,
    path_hint_tokens, strip_package_prefix_from_semantic_path, strip_source_extension,
};
pub use pipeline::match_packages_with_pipeline;
pub(crate) use proof::concrete_source::{
    concrete_package_source_from_parts, concrete_package_sources_by_module,
    package_version_from_proof_path,
};
pub(crate) use proof::cross_source::{
    cross_package_exact_source_external_import_target,
    same_package_cross_version_source_external_import_target,
};
pub(crate) use proof::dependency_graph::{
    dependency_edge_path_external_import_target,
    dependency_graph_source_fingerprint_external_import_target,
};
#[cfg(test)]
pub(crate) use proof::import_target::resolve_external_import_target;
pub(crate) use proof::import_target::{
    importable_package_source_for_module, proven_external_import_target,
    proven_external_package_version,
};
pub(crate) use proof::policy::source_only_match_can_be_promoted_to_import;
pub use proof::public_exports::package_source_public_export_proofs;
pub(crate) use proof::scratch::ExternalImportProofScratch;
pub use scoring::{
    AcceptanceDecision, FunctionMatch, STRUCTURAL_FREQUENCY_LIMIT, assign_max_weight, classify,
    try_exact, try_exact_alternate, try_feature_similarity, try_feature_similarity_alternate,
    try_structural_anchored, try_structural_anchored_alternate, try_structural_only,
    try_structural_only_alternate,
};
pub(crate) use scoring::{
    accepted_attribution_from_match, best_source_match, disambiguate_exact_source_candidate,
    module_package_match,
};
pub use source::source_imports::{
    package_import_names_from_sources, package_import_sites_from_sources,
};
pub(crate) use source::source_text::normalize_source;
pub use strategy::{
    CascadeMatchReport, CascadeOwnershipMatch, GlobalAssignment, StructuralBag,
    StructuralBagMatchReport, assign_globally, build_structural_bag, cascade_candidates,
    match_function, match_structural_bags, match_structural_bags_with_excluded_modules,
    match_with_cascade, score_structural_bags,
};

#[cfg(test)]
mod tests;
