//! External-import promotion policy.
//!
//! This module owns the "when is a proof allowed" decisions for package
//! externalization. Data records such as [`PackageMatch`] and index structures
//! stay policy-free; resolver/proof code asks these functions explicitly.

use super::dependency_graph::DependencyGraphSourceProof;
use super::export_member::ExportMemberSourceProof;
use super::semantic::SemanticExternalSourceProof;
use crate::{ModuleMatchStrategy, PackageMatch, SemanticPathHintMode};
use reverts_package::{ExternalImportProof, ExternalImportProofKind};

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

pub(crate) fn has_exact_hint_source_path(package_match: &PackageMatch) -> bool {
    ExternalImportProof::parse(package_match.source_path.as_str()).kind()
        == ExternalImportProofKind::ExactHint
}

pub(crate) fn exact_hint_has_quality(package_match: &PackageMatch, quality: &str) -> bool {
    has_exact_hint_source_path(package_match)
        && package_match
            .source_path
            .contains(format!(":quality={quality}:").as_str())
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
            if !has_exact_hint_source_path(package_match) {
                return Vec::new();
            }
            if exact_hint_has_quality(package_match, "trusted") {
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
            if exact_hint_has_quality(package_match, "weak") {
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
        | ModuleMatchStrategy::AggregateStructuralBagSimilarity
        | ModuleMatchStrategy::AggregateStringAnchorSimilarity
        | ModuleMatchStrategy::PackageGraphNeighborhoodOwnership => Vec::new(),
    }
}

pub(crate) fn canonical_subpath_policy_allows(package_match: &PackageMatch) -> bool {
    if source_only_match_can_be_promoted_to_import(package_match.strategy) {
        return true;
    }
    match package_match.strategy {
        ModuleMatchStrategy::DependencyClosureOwnership => {
            has_exact_hint_source_path(package_match)
        }
        ModuleMatchStrategy::AggregateStructuralBagSimilarity => {
            package_match.function_signature_matches >= 3
                && package_match.string_anchor_matches >= 8
        }
        ModuleMatchStrategy::AggregateStringAnchorSimilarity => false,
        ModuleMatchStrategy::CascadeFunctionOwnership
        | ModuleMatchStrategy::CascadePartialFunctionCoverage
        | ModuleMatchStrategy::AggregateFunctionSignatureAndStringAnchors => {
            package_match.function_signature_matches >= 2
                && package_match.string_anchor_matches >= 1
        }
        ModuleMatchStrategy::PackageGraphNeighborhoodOwnership => false,
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
            exact_hint_has_quality(package_match, "trusted")
                || exact_hint_has_quality(package_match, "weak")
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
        | ModuleMatchStrategy::AggregateStructuralBagSimilarity
        | ModuleMatchStrategy::AggregateStringAnchorSimilarity
        | ModuleMatchStrategy::PackageGraphNeighborhoodOwnership => false,
    }
}

pub(crate) fn public_export_member_policy_allows(package_match: &PackageMatch) -> bool {
    source_only_match_can_be_promoted_to_import(package_match.strategy)
        || (package_match.strategy == ModuleMatchStrategy::DependencyClosureOwnership
            && has_exact_hint_source_path(package_match))
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
            | ModuleMatchStrategy::AggregateStringAnchorSimilarity
            | ModuleMatchStrategy::PropertyShapeAndStringAnchors
            | ModuleMatchStrategy::ObjectShapeAndStringAnchors
            | ModuleMatchStrategy::ClassShapeAndStringAnchors
            | ModuleMatchStrategy::SwitchShapeAndStringAnchors
    )
}

pub(crate) fn dependency_edge_path_policy_allows(package_match: &PackageMatch) -> bool {
    package_match.strategy == ModuleMatchStrategy::DependencyClosureOwnership
        && (exact_hint_has_quality(package_match, "trusted")
            || exact_hint_has_quality(package_match, "weak"))
}

pub(crate) fn same_package_cross_version_source_policy_allows(
    package_match: &PackageMatch,
) -> bool {
    package_match.strategy == ModuleMatchStrategy::DependencyClosureOwnership
        && exact_hint_has_quality(package_match, "trusted")
}

pub(crate) fn cross_package_exact_source_policy_allows(package_match: &PackageMatch) -> bool {
    package_match.strategy == ModuleMatchStrategy::DependencyClosureOwnership
        && has_exact_hint_source_path(package_match)
}

pub(crate) const fn semantic_external_source_proof_label(
    proof: SemanticExternalSourceProof,
) -> &'static str {
    match proof {
        SemanticExternalSourceProof::SourcePath => "semantic-source",
        SemanticExternalSourceProof::ExportSurface => "semantic-export",
        SemanticExternalSourceProof::ExportMember => "semantic-member",
    }
}

pub(crate) const fn semantic_external_source_proof_rank(proof: SemanticExternalSourceProof) -> u8 {
    match proof {
        SemanticExternalSourceProof::SourcePath => 0,
        SemanticExternalSourceProof::ExportSurface => 1,
        SemanticExternalSourceProof::ExportMember => 2,
    }
}

pub(crate) const fn export_member_source_proof_label(
    proof: ExportMemberSourceProof,
) -> &'static str {
    match proof {
        ExportMemberSourceProof::BarrelReference => "barrel-reference",
        ExportMemberSourceProof::BuildVariantPeer => "build-variant-peer",
        ExportMemberSourceProof::CommonJsReexport => "commonjs-reexport",
        ExportMemberSourceProof::ExportAllReexport => "export-all-reexport",
        ExportMemberSourceProof::NamedReexport => "named-reexport",
        ExportMemberSourceProof::SourceEquivalent => "source-equivalent",
    }
}

pub(crate) const fn export_member_source_proof_rank(proof: ExportMemberSourceProof) -> u8 {
    match proof {
        ExportMemberSourceProof::BarrelReference => 1,
        ExportMemberSourceProof::BuildVariantPeer => 2,
        ExportMemberSourceProof::CommonJsReexport => 2,
        ExportMemberSourceProof::ExportAllReexport => 2,
        ExportMemberSourceProof::NamedReexport => 2,
        ExportMemberSourceProof::SourceEquivalent => 3,
    }
}

pub(crate) const fn export_member_source_proof_alias_source_is_matched(
    proof: ExportMemberSourceProof,
) -> bool {
    matches!(proof, ExportMemberSourceProof::CommonJsReexport)
}

pub(crate) const fn dependency_graph_source_proof_label(
    proof: DependencyGraphSourceProof,
) -> &'static str {
    match proof {
        DependencyGraphSourceProof::ExactSourceHash => "source-hash",
        DependencyGraphSourceProof::FunctionStringFingerprint => "function-string",
        DependencyGraphSourceProof::DependencyNeighborhood => "dependency-neighborhood",
        DependencyGraphSourceProof::StringFingerprintWithGraph => "string-graph",
    }
}

pub(crate) const fn dependency_graph_source_proof_rank(proof: DependencyGraphSourceProof) -> usize {
    match proof {
        DependencyGraphSourceProof::ExactSourceHash => 300,
        DependencyGraphSourceProof::FunctionStringFingerprint => 200,
        DependencyGraphSourceProof::DependencyNeighborhood => 150,
        DependencyGraphSourceProof::StringFingerprintWithGraph => 100,
    }
}

pub(crate) const fn dependency_graph_source_proof_requires_unique_source_path(
    proof: DependencyGraphSourceProof,
) -> bool {
    matches!(proof, DependencyGraphSourceProof::DependencyNeighborhood)
}
