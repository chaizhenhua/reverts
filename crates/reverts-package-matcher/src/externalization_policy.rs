//! External-import promotion policy.
//!
//! This module owns the "when is a proof allowed" decisions for package
//! externalization. Data records such as [`PackageMatch`] and index structures
//! stay policy-free; resolver/proof code asks these functions explicitly.

use crate::{ModuleMatchStrategy, PackageMatch, SemanticPathHintMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SemanticExternalTargetPolicy {
    pub(crate) hint_mode: SemanticPathHintMode,
    pub(crate) min_score: usize,
}

pub(crate) fn source_only_match_can_be_promoted_to_import(strategy: ModuleMatchStrategy) -> bool {
    matches!(
        strategy,
        ModuleMatchStrategy::NormalizedSourceHash
            | ModuleMatchStrategy::FunctionSignatureAndStringAnchors
            | ModuleMatchStrategy::PropertyShapeAndStringAnchors
            | ModuleMatchStrategy::ObjectShapeAndStringAnchors
            | ModuleMatchStrategy::ClassShapeAndStringAnchors
            | ModuleMatchStrategy::SwitchShapeAndStringAnchors
    )
}

pub(crate) fn semantic_external_target_policies(
    package_match: &PackageMatch,
) -> Vec<SemanticExternalTargetPolicy> {
    match package_match.strategy {
        ModuleMatchStrategy::NormalizedSourceHash => vec![SemanticExternalTargetPolicy {
            hint_mode: SemanticPathHintMode::ImportProof,
            min_score: 1,
        }],
        ModuleMatchStrategy::FunctionSignatureAndStringAnchors
            if package_match.function_signature_matches > 0
                && package_match.string_anchor_matches > 0 =>
        {
            vec![SemanticExternalTargetPolicy {
                hint_mode: SemanticPathHintMode::ImportProof,
                min_score: 1,
            }]
        }
        ModuleMatchStrategy::PropertyShapeAndStringAnchors
            if package_match.function_signature_matches > 0
                && package_match.string_anchor_matches > 0 =>
        {
            vec![SemanticExternalTargetPolicy {
                hint_mode: SemanticPathHintMode::ImportProof,
                min_score: 1,
            }]
        }
        ModuleMatchStrategy::ObjectShapeAndStringAnchors
            if package_match.function_signature_matches > 0
                && package_match.string_anchor_matches > 0 =>
        {
            vec![SemanticExternalTargetPolicy {
                hint_mode: SemanticPathHintMode::ImportProof,
                min_score: 1,
            }]
        }
        ModuleMatchStrategy::ClassShapeAndStringAnchors
            if package_match.function_signature_matches > 0
                && package_match.string_anchor_matches > 0 =>
        {
            vec![SemanticExternalTargetPolicy {
                hint_mode: SemanticPathHintMode::ImportProof,
                min_score: 1,
            }]
        }
        ModuleMatchStrategy::SwitchShapeAndStringAnchors
            if package_match.function_signature_matches > 0
                && package_match.string_anchor_matches > 0 =>
        {
            vec![SemanticExternalTargetPolicy {
                hint_mode: SemanticPathHintMode::ImportProof,
                min_score: 1,
            }]
        }
        ModuleMatchStrategy::DependencyClosureOwnership => {
            if !package_match.source_path.starts_with("exact-hint:") {
                return Vec::new();
            }
            if package_match.source_path.contains(":quality=trusted:") {
                return vec![
                    SemanticExternalTargetPolicy {
                        hint_mode: SemanticPathHintMode::ImportProof,
                        min_score: 1,
                    },
                    SemanticExternalTargetPolicy {
                        hint_mode: SemanticPathHintMode::RelaxedImportProof,
                        min_score: 4,
                    },
                ];
            }
            if package_match.source_path.contains(":quality=weak:") {
                return vec![SemanticExternalTargetPolicy {
                    hint_mode: SemanticPathHintMode::RelaxedImportProof,
                    min_score: 4,
                }];
            }
            Vec::new()
        }
        ModuleMatchStrategy::FunctionSignatureAndStringAnchors
        | ModuleMatchStrategy::PropertyShapeAndStringAnchors
        | ModuleMatchStrategy::ObjectShapeAndStringAnchors
        | ModuleMatchStrategy::ClassShapeAndStringAnchors
        | ModuleMatchStrategy::SwitchShapeAndStringAnchors => Vec::new(),
        ModuleMatchStrategy::AggregateFunctionSignatureAndStringAnchors
        | ModuleMatchStrategy::CascadeFunctionCoverage
        | ModuleMatchStrategy::CascadeFunctionOwnership
        | ModuleMatchStrategy::CascadePartialFunctionCoverage
        | ModuleMatchStrategy::AggregateStructuralBagSimilarity => Vec::new(),
    }
}

pub(crate) fn canonical_subpath_policy_allows(package_match: &PackageMatch) -> bool {
    if source_only_match_can_be_promoted_to_import(package_match.strategy) {
        return true;
    }
    match package_match.strategy {
        ModuleMatchStrategy::DependencyClosureOwnership => {
            package_match.source_path.starts_with("exact-hint:")
        }
        ModuleMatchStrategy::AggregateStructuralBagSimilarity => {
            package_match.function_signature_matches >= 3
                && package_match.string_anchor_matches >= 8
        }
        ModuleMatchStrategy::CascadeFunctionOwnership
        | ModuleMatchStrategy::CascadePartialFunctionCoverage
        | ModuleMatchStrategy::AggregateFunctionSignatureAndStringAnchors => {
            package_match.function_signature_matches >= 2
                && package_match.string_anchor_matches >= 1
        }
        ModuleMatchStrategy::NormalizedSourceHash
        | ModuleMatchStrategy::FunctionSignatureAndStringAnchors
        | ModuleMatchStrategy::PropertyShapeAndStringAnchors
        | ModuleMatchStrategy::ObjectShapeAndStringAnchors
        | ModuleMatchStrategy::ClassShapeAndStringAnchors
        | ModuleMatchStrategy::SwitchShapeAndStringAnchors
        | ModuleMatchStrategy::CascadeFunctionCoverage => false,
    }
}

pub(crate) fn semantic_source_only_export_member_policy_allows(
    package_match: &PackageMatch,
) -> bool {
    match package_match.strategy {
        ModuleMatchStrategy::DependencyClosureOwnership => {
            package_match.source_path.starts_with("exact-hint:")
                && (package_match.source_path.contains(":quality=trusted:")
                    || package_match.source_path.contains(":quality=weak:"))
        }
        ModuleMatchStrategy::NormalizedSourceHash
        | ModuleMatchStrategy::FunctionSignatureAndStringAnchors
        | ModuleMatchStrategy::PropertyShapeAndStringAnchors
        | ModuleMatchStrategy::ObjectShapeAndStringAnchors
        | ModuleMatchStrategy::ClassShapeAndStringAnchors
        | ModuleMatchStrategy::SwitchShapeAndStringAnchors
        | ModuleMatchStrategy::AggregateFunctionSignatureAndStringAnchors
        | ModuleMatchStrategy::CascadeFunctionCoverage
        | ModuleMatchStrategy::CascadeFunctionOwnership
        | ModuleMatchStrategy::CascadePartialFunctionCoverage
        | ModuleMatchStrategy::AggregateStructuralBagSimilarity => false,
    }
}

pub(crate) fn public_export_member_policy_allows(package_match: &PackageMatch) -> bool {
    source_only_match_can_be_promoted_to_import(package_match.strategy)
        || (package_match.strategy == ModuleMatchStrategy::DependencyClosureOwnership
            && package_match.source_path.starts_with("exact-hint:"))
        || (package_match.strategy == ModuleMatchStrategy::AggregateStructuralBagSimilarity
            && package_match.function_signature_matches >= 3
            && package_match.string_anchor_matches >= 8)
        || (matches!(
            package_match.strategy,
            ModuleMatchStrategy::CascadeFunctionOwnership
                | ModuleMatchStrategy::CascadePartialFunctionCoverage
                | ModuleMatchStrategy::AggregateFunctionSignatureAndStringAnchors
        ) && package_match.function_signature_matches >= 2
            && package_match.string_anchor_matches >= 1)
}

pub(crate) fn dependency_graph_source_fingerprint_policy_allows(
    strategy: ModuleMatchStrategy,
) -> bool {
    matches!(
        strategy,
        ModuleMatchStrategy::DependencyClosureOwnership
            | ModuleMatchStrategy::AggregateFunctionSignatureAndStringAnchors
            | ModuleMatchStrategy::CascadeFunctionCoverage
            | ModuleMatchStrategy::CascadeFunctionOwnership
            | ModuleMatchStrategy::CascadePartialFunctionCoverage
            | ModuleMatchStrategy::AggregateStructuralBagSimilarity
            | ModuleMatchStrategy::PropertyShapeAndStringAnchors
            | ModuleMatchStrategy::ObjectShapeAndStringAnchors
            | ModuleMatchStrategy::ClassShapeAndStringAnchors
            | ModuleMatchStrategy::SwitchShapeAndStringAnchors
    )
}

pub(crate) fn dependency_edge_path_policy_allows(package_match: &PackageMatch) -> bool {
    package_match.strategy == ModuleMatchStrategy::DependencyClosureOwnership
        && package_match.source_path.starts_with("exact-hint:")
        && (package_match.source_path.contains(":quality=trusted:")
            || package_match.source_path.contains(":quality=weak:"))
}

pub(crate) fn same_package_cross_version_source_policy_allows(
    package_match: &PackageMatch,
) -> bool {
    package_match.strategy == ModuleMatchStrategy::DependencyClosureOwnership
        && package_match.source_path.starts_with("exact-hint:")
        && package_match.source_path.contains(":quality=trusted:")
}

pub(crate) fn cross_package_exact_source_policy_allows(package_match: &PackageMatch) -> bool {
    package_match.strategy == ModuleMatchStrategy::DependencyClosureOwnership
        && package_match.source_path.starts_with("exact-hint:")
}
