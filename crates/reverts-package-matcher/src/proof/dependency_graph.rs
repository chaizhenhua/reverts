use std::collections::{BTreeMap, BTreeSet};

use reverts_input::{InputRows, ModuleInput};
use reverts_ir::ModuleId;
use reverts_package::ExternalImportProofPath;

use super::export_member::export_member_external_package_source_for_source_path;
use super::policy::{
    dependency_edge_path_policy_allows, dependency_graph_source_fingerprint_policy_allows,
    dependency_graph_source_proof_label, dependency_graph_source_proof_rank,
    dependency_graph_source_proof_requires_unique_source_path,
};
use super::scratch::{DependencyGraphEvidence, ExternalImportProofScratch};
use crate::package_helpers::package_source_external_import_rank;
use crate::source::package_refs::{
    package_source_entry_path_from_source_path, source_entry_paths_match,
};
use crate::{
    ConcretePackageSourcePath, ExternalImportSourceIndex, ExternalImportTarget,
    ModuleMatchFingerprint, PackageMatch, PackageSource, PackageSourceFingerprint,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DependencyGraphSourceProof {
    ExactSourceHash,
    FunctionStringFingerprint,
    DependencyNeighborhood,
    StringFingerprintWithGraph,
}

#[derive(Debug, Clone, Copy)]
struct DependencyGraphSourceCandidate<'a> {
    source: &'a PackageSource,
    proof: DependencyGraphSourceProof,
    graph: DependencyGraphEvidence,
    function_matches: usize,
    string_matches: usize,
}

pub(crate) fn dependency_graph_source_fingerprint_external_import_target<'a>(
    rows: &InputRows,
    module: &ModuleInput,
    package_match: &PackageMatch,
    external_source_index: &ExternalImportSourceIndex<'a>,
    module_source: &str,
    concrete_sources_by_module: &BTreeMap<ModuleId, ConcretePackageSourcePath>,
    cache: &ExternalImportProofScratch<'a>,
) -> Option<ExternalImportTarget> {
    if !dependency_graph_source_fingerprint_policy_allows(package_match.strategy)
        || module_source.trim().is_empty()
    {
        return None;
    }
    let module_fingerprint =
        cache.module_fingerprint(module, module.semantic_path.as_str(), module_source)?;
    let mut candidates = cache
        .source_fingerprints_for_version(
            external_source_index,
            package_match.package_name.as_str(),
            package_match.package_version.as_str(),
        )
        .into_iter()
        .filter_map(|source_fingerprint| {
            let source = source_fingerprint.source;
            let graph = cache.dependency_graph_evidence(
                rows,
                module.id,
                source,
                external_source_index,
                concrete_sources_by_module,
            );
            let function_matches = source_fingerprint
                .function_signature_hashes
                .intersection(&module_fingerprint.function_signature_hashes)
                .count();
            let string_matches = source_fingerprint
                .string_anchors
                .intersection(&module_fingerprint.string_anchors)
                .count();
            let proof = dependency_graph_source_proof(
                &module_fingerprint,
                &source_fingerprint,
                graph,
                function_matches,
                string_matches,
            )?;
            Some(DependencyGraphSourceCandidate {
                source,
                proof,
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
        dependency_graph_source_candidate_score(right)
            .cmp(&dependency_graph_source_candidate_score(left))
            .then_with(|| {
                package_source_external_import_rank(left.source)
                    .cmp(&package_source_external_import_rank(right.source))
            })
            .then_with(|| {
                left.source
                    .export_specifier
                    .cmp(&right.source.export_specifier)
            })
            .then_with(|| left.source.source_path.cmp(&right.source.source_path))
    });
    let best_score = dependency_graph_source_candidate_score(candidates.first()?);
    let best = candidates
        .into_iter()
        .filter(|candidate| dependency_graph_source_candidate_score(candidate) == best_score)
        .collect::<Vec<_>>();
    let best_proof = best.first()?.proof;
    let export_specifiers = best
        .iter()
        .map(|candidate| candidate.source.export_specifier.as_str())
        .collect::<BTreeSet<_>>();
    if export_specifiers.len() != 1 {
        return None;
    }
    if dependency_graph_source_proof_requires_unique_source_path(best_proof) {
        let targets = best
            .iter()
            .map(|candidate| {
                (
                    candidate.source.export_specifier.as_str(),
                    candidate.source.source_path.as_str(),
                )
            })
            .collect::<BTreeSet<_>>();
        if targets.len() != 1 {
            return None;
        }
    }
    let selected = best.into_iter().min_by(|left, right| {
        package_source_external_import_rank(left.source)
            .cmp(&package_source_external_import_rank(right.source))
            .then_with(|| left.source.source_path.cmp(&right.source.source_path))
    })?;
    if selected.source.external_importable {
        return Some(ExternalImportTarget {
            export_specifier: selected.source.export_specifier.clone(),
            source_path: ExternalImportProofPath::dependency_graph_source(
                dependency_graph_source_proof_label(selected.proof),
                selected.graph.matched_edges,
                selected.graph.known_edges,
                selected.function_matches,
                selected.string_matches,
                selected.source.source_path.as_str(),
            ),
        });
    }
    export_member_external_package_source_for_source_path(
        selected.source.package_name.as_str(),
        selected.source.package_version.as_str(),
        selected.source.source_path.as_str(),
        external_source_index,
        module_source,
    )
}

fn dependency_graph_source_candidate_score(
    candidate: &DependencyGraphSourceCandidate<'_>,
) -> usize {
    dependency_graph_source_proof_rank(candidate.proof)
        + candidate.graph.matched_edges * 20
        + candidate.function_matches * 3
        + candidate.string_matches
}

fn dependency_graph_source_proof(
    module_fingerprint: &ModuleMatchFingerprint,
    source_fingerprint: &PackageSourceFingerprint<'_>,
    graph: DependencyGraphEvidence,
    function_matches: usize,
    string_matches: usize,
) -> Option<DependencyGraphSourceProof> {
    if !source_fingerprint
        .normalized_source_hashes
        .is_disjoint(&module_fingerprint.normalized_source_hashes)
    {
        return Some(DependencyGraphSourceProof::ExactSourceHash);
    }
    if graph.matched_edges >= 1 && function_matches >= 2 && string_matches >= 1 {
        return Some(DependencyGraphSourceProof::FunctionStringFingerprint);
    }
    if graph.matched_edges >= 1 && string_matches >= 8 {
        return Some(DependencyGraphSourceProof::StringFingerprintWithGraph);
    }
    if graph.matched_edges >= 2 && string_matches >= 3 {
        return Some(DependencyGraphSourceProof::StringFingerprintWithGraph);
    }
    if graph.known_edges >= 2 && graph.matched_edges == graph.known_edges {
        return Some(DependencyGraphSourceProof::DependencyNeighborhood);
    }
    None
}

pub(crate) fn dependency_edge_path_external_import_target(
    rows: &InputRows,
    module: &ModuleInput,
    package_match: &PackageMatch,
    external_source_index: &ExternalImportSourceIndex<'_>,
    concrete_sources_by_module: &BTreeMap<ModuleId, ConcretePackageSourcePath>,
    cache: &ExternalImportProofScratch<'_>,
) -> Option<ExternalImportTarget> {
    if !dependency_edge_path_policy_allows(package_match) {
        return None;
    }
    let mut candidates = Vec::<DependencyEdgePathCandidate<'_>>::new();
    for dependent_id in cache.direct_dependents(rows, module.id) {
        let Some(dependent) = concrete_sources_by_module.get(&dependent_id) else {
            continue;
        };
        if dependent.package_name != package_match.package_name
            || dependent.package_version != package_match.package_version
        {
            continue;
        }
        let dependent_sources = external_source_index.sources_matching_concrete_path(
            dependent.package_name.as_str(),
            dependent.package_version.as_str(),
            dependent.source_path.as_str(),
        );
        for dependent_source in dependent_sources {
            let entries = dependency_edge_path_remaining_entries(
                rows,
                dependent_id,
                module.id,
                dependent_source,
                external_source_index,
                concrete_sources_by_module,
                cache,
            );
            if entries.len() != 1 {
                continue;
            }
            let entry = entries
                .into_iter()
                .next()
                .expect("one remaining dependency entry");
            for source in external_source_index.external_importable_sources_matching_entry(
                package_match.package_name.as_str(),
                package_match.package_version.as_str(),
                entry.as_str(),
            ) {
                candidates.push(DependencyEdgePathCandidate {
                    source,
                    dependent_id,
                    dependent_source_path: dependent_source.source_path.as_str(),
                    entry: entry.clone(),
                });
            }
        }
    }
    if candidates.is_empty() {
        return None;
    }
    let targets = candidates
        .iter()
        .map(|candidate| {
            (
                candidate.source.export_specifier.as_str(),
                candidate.source.source_path.as_str(),
            )
        })
        .collect::<BTreeSet<_>>();
    if targets.len() != 1 {
        return None;
    }
    let selected = candidates.into_iter().min_by(|left, right| {
        package_source_external_import_rank(left.source)
            .cmp(&package_source_external_import_rank(right.source))
            .then_with(|| left.source.source_path.cmp(&right.source.source_path))
            .then_with(|| left.dependent_id.cmp(&right.dependent_id))
    })?;
    Some(ExternalImportTarget {
        export_specifier: selected.source.export_specifier.clone(),
        source_path: ExternalImportProofPath::dependency_edge_path(
            selected.dependent_id.0,
            selected.entry.as_str(),
            selected.dependent_source_path,
            selected.source.source_path.as_str(),
        ),
    })
}

#[derive(Debug, Clone)]
struct DependencyEdgePathCandidate<'a> {
    source: &'a PackageSource,
    dependent_id: ModuleId,
    dependent_source_path: &'a str,
    entry: String,
}

fn dependency_edge_path_remaining_entries(
    rows: &InputRows,
    dependent_id: ModuleId,
    unresolved_module_id: ModuleId,
    dependent_source: &PackageSource,
    external_source_index: &ExternalImportSourceIndex<'_>,
    concrete_sources_by_module: &BTreeMap<ModuleId, ConcretePackageSourcePath>,
    cache: &ExternalImportProofScratch<'_>,
) -> BTreeSet<String> {
    let dependency_ids = cache.direct_dependencies(rows, dependent_id);
    if !dependency_ids.contains(&unresolved_module_id) {
        return BTreeSet::new();
    }
    let mut entries = external_source_index.dependency_entries(dependent_source);
    if entries.is_empty() {
        return entries;
    }
    for dependency_id in dependency_ids {
        if dependency_id == unresolved_module_id {
            continue;
        }
        if let Some(concrete) = concrete_sources_by_module.get(&dependency_id) {
            if concrete.package_name == dependent_source.package_name
                && concrete.package_version == dependent_source.package_version
            {
                let known_entry = package_source_entry_path_from_source_path(
                    concrete.package_name.as_str(),
                    concrete.package_version.as_str(),
                    concrete.source_path.as_str(),
                );
                entries.retain(|entry| {
                    !source_entry_paths_match(entry.as_str(), known_entry.as_str())
                });
            }
            continue;
        }
        if cache.row_module_is_same_package_version(
            rows,
            dependency_id,
            dependent_source.package_name.as_str(),
            dependent_source.package_version.as_str(),
        ) {
            return BTreeSet::new();
        }
    }
    entries
}
