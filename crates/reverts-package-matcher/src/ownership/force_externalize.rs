//! Proof-driven external import promotion for package modules that no stronger
//! strategy externalized yet.
//!
//! Modules whose bundle hints already name a known matched package are
//! promoted across the external-import boundary only when a concrete proof can
//! resolve both package version and import target: direct standard-target
//! lookup, dependency-graph fingerprint evidence, dependency-edge path
//! evidence, same-package cross-version source equivalence, or cross-package
//! exact source equivalence. Iterates to a fixed point so newly proven
//! attributions can feed the next round.

use std::collections::{BTreeMap, BTreeSet};

use reverts_input::InputRows;
use reverts_ir::{ModuleKind, is_valid_package_name};

use crate::model::{PackageVersionCandidate, VersionedPackageMatcherConfig};
use crate::scoring::best_source_match;
use crate::{
    ExternalImportProofScratch, ExternalImportSourceIndex, ModuleMatchStrategy, PackageSource,
    VersionedPackageMatchReport, accepted_external_modules, concrete_package_source_from_parts,
    concrete_package_sources_by_module, cross_package_exact_source_external_import_target,
    dependency_edge_path_external_import_target,
    dependency_graph_source_fingerprint_external_import_target, package_version_from_proof_path,
    proven_external_import_target, proven_external_package_version,
    same_package_cross_version_source_external_import_target,
};

use super::promotion::{ExternalImportPromotion, apply_external_import_promotion};

pub(crate) fn promote_proven_external_import_targets(
    rows: &InputRows,
    package_sources: &[PackageSource],
    matched_package_names: &BTreeSet<String>,
    report: &mut VersionedPackageMatchReport,
) -> usize {
    let mut accepted_modules = accepted_external_modules(rows, report);
    let source_only_matches = report
        .matches
        .iter()
        .filter(|package_match| !package_match.external_importable)
        .map(|package_match| (package_match.module_id, package_match.clone()))
        .collect::<BTreeMap<_, _>>();
    let source_only_match_indices = report
        .matches
        .iter()
        .enumerate()
        .filter(|(_index, package_match)| !package_match.external_importable)
        .map(|(index, package_match)| (package_match.module_id, index))
        .collect::<BTreeMap<_, _>>();
    let external_source_index = ExternalImportSourceIndex::build(package_sources);
    let cache = ExternalImportProofScratch::default();
    let indexed_versions = package_sources
        .iter()
        .map(|source| (source.package_name.clone(), source.package_version.clone()))
        .collect::<BTreeSet<_>>();
    let mut concrete_sources_by_module = concrete_package_sources_by_module(rows, report);
    let mut promoted = 0usize;

    loop {
        let mut round_promoted = 0usize;
        for module in &rows.modules {
            if module.kind != ModuleKind::Package || accepted_modules.contains(&module.id) {
                continue;
            }
            let Some(package_name) =
                module
                    .package_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|package_name| {
                        !package_name.is_empty() && is_valid_package_name(package_name)
                    })
            else {
                continue;
            };
            if !matched_package_names.contains(package_name) {
                continue;
            }
            let source_only_match = source_only_matches.get(&module.id);
            let Some(package_version) = proven_external_package_version(module, source_only_match)
            else {
                continue;
            };
            let module_source = rows
                .module_source_slice(module.id)
                .map(|slice| slice.source)
                .unwrap_or_default();
            let standard_target = proven_external_import_target(
                rows,
                module,
                package_name,
                package_version.as_str(),
                source_only_match,
                &external_source_index,
            );
            // The hinted package's own target is tried first, but a path hint
            // can be an upstream bundler misattribution (e.g. `ws` code wrapped
            // under a stale `node_modules/zod/...` path). When the module's
            // source is provably equivalent to a *different* indexed package,
            // suppress the hinted-package target so the cross-package source
            // correction below can re-home the module to its true package
            // instead of emitting a wrong external import.
            let standard_target = standard_target.filter(|_target| {
                !module_proven_in_other_package(
                    module,
                    module_source,
                    package_name,
                    &indexed_versions,
                    &external_source_index,
                    &cache,
                )
            });
            let mut accepted_package_name = package_name.to_string();
            let mut accepted_package_version = package_version.clone();
            let mut accepted_function_matches = source_only_match
                .map(|package_match| package_match.function_signature_matches)
                .unwrap_or_default();
            let mut accepted_string_matches = source_only_match
                .map(|package_match| package_match.string_anchor_matches)
                .unwrap_or_default();
            let graph_target = source_only_match.and_then(|package_match| {
                dependency_graph_source_fingerprint_external_import_target(
                    rows,
                    module,
                    package_match,
                    &external_source_index,
                    module_source,
                    &concrete_sources_by_module,
                    &cache,
                )
                .or_else(|| {
                    dependency_edge_path_external_import_target(
                        rows,
                        module,
                        package_match,
                        &external_source_index,
                        &concrete_sources_by_module,
                        &cache,
                    )
                })
            });
            let mut target = graph_target.or(standard_target);
            if target.is_none()
                && let Some(correction) = source_only_match.and_then(|package_match| {
                    same_package_cross_version_source_external_import_target(
                        module,
                        package_match,
                        &external_source_index,
                        module_source,
                        &cache,
                    )
                })
            {
                accepted_package_name = correction.package_name;
                accepted_package_version = correction.package_version;
                accepted_function_matches = correction.function_signature_matches;
                accepted_string_matches = correction.string_anchor_matches;
                target = Some(correction.target);
            }
            if target.is_none()
                && let Some(correction) = source_only_match.and_then(|package_match| {
                    cross_package_exact_source_external_import_target(
                        rows,
                        module,
                        package_match,
                        &external_source_index,
                        module_source,
                        &concrete_sources_by_module,
                        &cache,
                    )
                })
            {
                accepted_package_name = correction.package_name;
                accepted_package_version = correction.package_version;
                accepted_function_matches = correction.function_signature_matches;
                accepted_string_matches = correction.string_anchor_matches;
                target = Some(correction.target);
            }
            let Some(target) = target else {
                continue;
            };
            if let Some(proven_version) = package_version_from_proof_path(
                accepted_package_name.as_str(),
                target.source_path.as_str(),
            ) {
                accepted_package_version = proven_version;
            }
            let target_source_path = target.source_path.clone();
            let match_index = source_only_match_indices.get(&module.id).copied();
            apply_external_import_promotion(
                report,
                match_index,
                ExternalImportPromotion {
                    module_id: module.id,
                    package_name: accepted_package_name.clone(),
                    package_version: accepted_package_version.clone(),
                    export_specifier: target.export_specifier,
                    resolved_file: target.source_path,
                    strategy: source_only_match
                        .map(|package_match| package_match.strategy)
                        .unwrap_or(ModuleMatchStrategy::DependencyClosureOwnership),
                    function_signature_matches: accepted_function_matches,
                    string_anchor_matches: accepted_string_matches,
                },
            );
            accepted_modules.insert(module.id);
            if let Some(concrete) = concrete_package_source_from_parts(
                module.id,
                accepted_package_name.as_str(),
                report
                    .matches
                    .iter()
                    .find(|package_match| package_match.module_id == module.id)
                    .map(|package_match| package_match.package_version.as_str())
                    .unwrap_or(accepted_package_version.as_str()),
                target_source_path.as_str(),
            ) {
                concrete_sources_by_module.insert(module.id, concrete);
            }
            promoted += 1;
            round_promoted += 1;
        }
        if round_promoted == 0 {
            break;
        }
    }
    promoted
}

/// Returns whether the module's source is provably equivalent to an indexed
/// package other than the one its hint names. Equivalence reuses the same
/// per-module source strategies the external-import proof resolver accepts, so
/// a match is concrete evidence of misattribution rather than a coincidental
/// token overlap. A module that truly is the hinted package never matches a
/// different package's source, so genuine hints are unaffected.
fn module_proven_in_other_package<'a>(
    module: &reverts_input::ModuleInput,
    module_source: &str,
    hinted_package_name: &str,
    indexed_versions: &BTreeSet<(String, String)>,
    external_source_index: &ExternalImportSourceIndex<'a>,
    cache: &ExternalImportProofScratch<'a>,
) -> bool {
    let Some(module_fingerprint) =
        cache.module_fingerprint(module, module.semantic_path.as_str(), module_source)
    else {
        return false;
    };
    indexed_versions
        .iter()
        .filter(|(package_name, _version)| package_name != hinted_package_name)
        .any(|(package_name, package_version)| {
            let sources = cache.source_fingerprints_for_version(
                external_source_index,
                package_name,
                package_version,
            );
            if sources.is_empty() {
                return false;
            }
            let version = PackageVersionCandidate {
                package_name: package_name.clone(),
                package_version: package_version.clone(),
                sources,
            };
            best_source_match(
                &version,
                &module_fingerprint,
                &VersionedPackageMatcherConfig::default(),
            )
            .is_some_and(|source_match| is_proven_module_source_strategy(source_match.strategy))
        })
}

/// Per-module source strategies strong enough to prove a module's identity
/// against a concrete package source. Mirrors the set the external-import proof
/// resolver trusts; aggregate, cascade, and ownership strategies are too weak
/// to override a hint and are excluded.
fn is_proven_module_source_strategy(strategy: ModuleMatchStrategy) -> bool {
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
