use std::collections::{BTreeMap, BTreeSet};

use reverts_input::{InputRows, ModuleInput};
use reverts_ir::ModuleId;
use semver::Version;

use super::export_member::export_member_external_package_source_for_source_path;
use super::policy::{
    cross_package_exact_source_policy_allows, same_package_cross_version_source_policy_allows,
    source_only_match_can_be_promoted_to_import,
};
use super::scratch::{DependencyGraphEvidence, ExternalImportProofScratch};
use crate::package_helpers::{is_json_source_path, package_source_external_import_rank};
use crate::{
    ConcretePackageSourcePath, CorrectedPackageExternalImportTarget, ExternalImportSourceIndex,
    ExternalImportTarget, ModuleMatchStrategy, ModulePackageMatch, PackageMatch, PackageSource,
    PackageSourceFingerprint, PackageVersionCandidate, VersionedPackageMatcherConfig,
    best_source_match,
};

#[derive(Debug, Clone)]
struct CrossVersionSourceCandidate {
    package_match: ModulePackageMatch,
    target: ExternalImportTarget,
}

pub(crate) fn same_package_cross_version_source_external_import_target<'a>(
    module: &ModuleInput,
    package_match: &PackageMatch,
    external_source_index: &ExternalImportSourceIndex<'a>,
    module_source: &str,
    cache: &ExternalImportProofScratch<'a>,
) -> Option<CorrectedPackageExternalImportTarget> {
    if !same_package_cross_version_source_policy_allows(package_match)
        || module_source.trim().is_empty()
    {
        return None;
    }
    let module_fingerprint =
        cache.module_fingerprint(module, module.semantic_path.as_str(), module_source)?;
    let mut by_version = BTreeMap::<String, Vec<PackageSourceFingerprint<'_>>>::new();
    for source_fingerprint in cache
        .source_fingerprints_for_package(external_source_index, package_match.package_name.as_str())
    {
        if source_fingerprint.source.package_version == package_match.package_version {
            continue;
        }
        by_version
            .entry(source_fingerprint.source.package_version.clone())
            .or_default()
            .push(source_fingerprint);
    }
    let mut candidates = Vec::<CrossVersionSourceCandidate>::new();
    for (package_version, sources) in by_version {
        let version = PackageVersionCandidate {
            package_name: package_match.package_name.clone(),
            package_version,
            sources,
        };
        let Some(source_match) = best_source_match(
            &version,
            &module_fingerprint,
            &VersionedPackageMatcherConfig::default(),
        ) else {
            continue;
        };
        if !source_only_match_can_be_promoted_to_import(source_match.strategy) {
            continue;
        }
        let target = if source_match.external_importable {
            ExternalImportTarget {
                export_specifier: source_match.export_specifier.clone(),
                source_path: format!(
                    "forced-external:cross-version-source:{}:from={}:{}",
                    source_match.strategy.as_str(),
                    package_match.package_version,
                    source_match.source_path
                ),
            }
        } else {
            export_member_external_package_source_for_source_path(
                source_match.package_name.as_str(),
                source_match.package_version.as_str(),
                source_match.source_path.as_str(),
                external_source_index,
                module_source,
            )?
        };
        if !cross_version_source_target_allowed_by_runtime_surface(
            package_match,
            &source_match,
            &target,
            external_source_index,
        ) {
            continue;
        }
        candidates.push(CrossVersionSourceCandidate {
            package_match: source_match,
            target,
        });
    }
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by(|left, right| {
        cross_version_source_candidate_score(&right.package_match)
            .cmp(&cross_version_source_candidate_score(&left.package_match))
            .then_with(|| {
                left.package_match
                    .package_version
                    .cmp(&right.package_match.package_version)
            })
            .then_with(|| {
                left.package_match
                    .export_specifier
                    .cmp(&right.package_match.export_specifier)
            })
            .then_with(|| {
                left.package_match
                    .source_path
                    .cmp(&right.package_match.source_path)
            })
    });
    let best_score = cross_version_source_candidate_score(&candidates.first()?.package_match);
    let best = candidates
        .into_iter()
        .filter(|candidate| {
            cross_version_source_candidate_score(&candidate.package_match) == best_score
        })
        .collect::<Vec<_>>();
    let targets = best
        .iter()
        .map(|candidate| {
            (
                candidate.package_match.package_name.as_str(),
                candidate.package_match.package_version.as_str(),
                candidate.target.export_specifier.as_str(),
                candidate.target.source_path.as_str(),
            )
        })
        .collect::<BTreeSet<_>>();
    if targets.len() != 1 {
        return None;
    }
    let selected = best.into_iter().next()?;
    Some(CorrectedPackageExternalImportTarget {
        package_name: selected.package_match.package_name,
        package_version: selected.package_match.package_version,
        target: selected.target,
        function_signature_matches: selected.package_match.function_signature_matches,
        string_anchor_matches: selected.package_match.string_anchor_matches,
    })
}

fn cross_version_source_target_allowed_by_runtime_surface(
    package_match: &PackageMatch,
    source_match: &ModulePackageMatch,
    target: &ExternalImportTarget,
    external_source_index: &ExternalImportSourceIndex<'_>,
) -> bool {
    if !cross_version_source_proof_is_older_than_hint(
        source_match.package_version.as_str(),
        package_match.package_version.as_str(),
    ) {
        return true;
    }
    external_source_index
        .sources(
            package_match.package_name.as_str(),
            package_match.package_version.as_str(),
        )
        .iter()
        .any(|source| source.export_specifier == target.export_specifier)
}

fn cross_version_source_proof_is_older_than_hint(proof_version: &str, hint_version: &str) -> bool {
    match (Version::parse(proof_version), Version::parse(hint_version)) {
        (Ok(proof_version), Ok(hint_version)) => proof_version < hint_version,
        _ => proof_version != hint_version,
    }
}

fn cross_version_source_candidate_score(package_match: &ModulePackageMatch) -> usize {
    let strategy_score = match package_match.strategy {
        ModuleMatchStrategy::NormalizedSourceHash => 1000,
        ModuleMatchStrategy::FunctionSignatureAndStringAnchors => 700,
        ModuleMatchStrategy::PropertyShapeAndStringAnchors
        | ModuleMatchStrategy::ObjectShapeAndStringAnchors
        | ModuleMatchStrategy::ClassShapeAndStringAnchors
        | ModuleMatchStrategy::SwitchShapeAndStringAnchors => 600,
        ModuleMatchStrategy::AggregateFunctionSignatureAndStringAnchors
        | ModuleMatchStrategy::CascadeFunctionCoverage
        | ModuleMatchStrategy::CascadeFunctionOwnership
        | ModuleMatchStrategy::CascadePartialFunctionCoverage
        | ModuleMatchStrategy::AggregateStructuralBagSimilarity
        | ModuleMatchStrategy::DependencyClosureOwnership => 0,
    };
    strategy_score
        + package_match.function_signature_matches * 3
        + package_match.string_anchor_matches
}

pub(crate) fn cross_package_exact_source_external_import_target<'a>(
    rows: &InputRows,
    module: &ModuleInput,
    package_match: &PackageMatch,
    external_source_index: &ExternalImportSourceIndex<'a>,
    module_source: &str,
    concrete_sources_by_module: &BTreeMap<ModuleId, ConcretePackageSourcePath>,
    cache: &ExternalImportProofScratch<'a>,
) -> Option<CorrectedPackageExternalImportTarget> {
    if !cross_package_exact_source_policy_allows(package_match) || module_source.trim().is_empty() {
        return None;
    }
    let module_fingerprint =
        cache.module_fingerprint(module, module.semantic_path.as_str(), module_source)?;
    let mut candidates = external_source_index
        .normalized_sources_for_any_package(&module_fingerprint.normalized_source_hashes)
        .into_iter()
        .filter(|source| source.external_importable && source.is_within_fingerprint_budget())
        .filter_map(|source| {
            let source_fingerprint = external_source_index.source_fingerprint(source)?;
            if source_fingerprint
                .normalized_source_hashes
                .is_disjoint(&module_fingerprint.normalized_source_hashes)
            {
                return None;
            }
            let function_matches = source_fingerprint
                .function_signature_hashes
                .intersection(&module_fingerprint.function_signature_hashes)
                .count();
            let string_matches = source_fingerprint
                .string_anchors
                .intersection(&module_fingerprint.string_anchors)
                .count();
            let graph = cache.dependency_graph_evidence(
                rows,
                module.id,
                source,
                external_source_index,
                concrete_sources_by_module,
            );
            if !cross_package_exact_source_candidate_allowed(
                module_source,
                source,
                graph,
                function_matches,
                string_matches,
            ) {
                return None;
            }
            Some(CrossPackageExactSourceCandidate {
                source,
                graph,
                function_matches,
                string_matches,
            })
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by(|left, right| {
        cross_package_exact_source_score(right)
            .cmp(&cross_package_exact_source_score(left))
            .then_with(|| {
                package_source_external_import_rank(left.source)
                    .cmp(&package_source_external_import_rank(right.source))
            })
            .then_with(|| left.source.package_name.cmp(&right.source.package_name))
            .then_with(|| {
                left.source
                    .package_version
                    .cmp(&right.source.package_version)
            })
            .then_with(|| {
                left.source
                    .export_specifier
                    .cmp(&right.source.export_specifier)
            })
            .then_with(|| left.source.source_path.cmp(&right.source.source_path))
    });
    let best_score = cross_package_exact_source_score(candidates.first()?);
    let best = candidates
        .into_iter()
        .filter(|candidate| cross_package_exact_source_score(candidate) == best_score)
        .collect::<Vec<_>>();
    let targets = best
        .iter()
        .map(|candidate| {
            (
                candidate.source.package_name.as_str(),
                candidate.source.package_version.as_str(),
                candidate.source.export_specifier.as_str(),
                candidate.source.source_path.as_str(),
            )
        })
        .collect::<BTreeSet<_>>();
    if targets.len() != 1 {
        return None;
    }
    let selected = best.into_iter().next()?;
    Some(CorrectedPackageExternalImportTarget {
        package_name: selected.source.package_name.clone(),
        package_version: selected.source.package_version.clone(),
        function_signature_matches: selected.function_matches,
        string_anchor_matches: selected.string_matches,
        target: ExternalImportTarget {
            export_specifier: selected.source.export_specifier.clone(),
            source_path: format!(
                "forced-external:cross-package-source:source-hash:hint={}@{}:graph={}/{}:functions={}:strings={}:{}",
                package_match.package_name,
                package_match.package_version,
                selected.graph.matched_edges,
                selected.graph.known_edges,
                selected.function_matches,
                selected.string_matches,
                selected.source.source_path,
            ),
        },
    })
}

#[derive(Debug, Clone, Copy)]
struct CrossPackageExactSourceCandidate<'a> {
    source: &'a PackageSource,
    graph: DependencyGraphEvidence,
    function_matches: usize,
    string_matches: usize,
}

fn cross_package_exact_source_candidate_allowed(
    module_source: &str,
    source: &PackageSource,
    graph: DependencyGraphEvidence,
    function_matches: usize,
    string_matches: usize,
) -> bool {
    if is_json_source_path(source.source_path.as_str()) {
        return false;
    }
    graph.matched_edges >= 1
        || (module_source.len() >= 120 && function_matches >= 1 && string_matches >= 1)
        || (module_source.len() >= 300 && (function_matches >= 1 || string_matches >= 2))
}

fn cross_package_exact_source_score(candidate: &CrossPackageExactSourceCandidate<'_>) -> usize {
    1_000
        + candidate.graph.matched_edges * 50
        + candidate.function_matches * 10
        + candidate.string_matches
}
