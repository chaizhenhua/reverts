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

use reverts_input::{InputRows, PackageAttributionInput};
use reverts_ir::{ModuleKind, is_valid_package_name, split_bare_specifier};

use crate::{
    ExternalImportProofScratch, ExternalImportSourceIndex, ModuleMatchStrategy, PackageMatch,
    PackageSource, VersionedPackageMatchReport, accepted_external_modules,
    concrete_package_source_from_parts, concrete_package_sources_by_module,
    cross_package_exact_source_external_import_target, dependency_edge_path_external_import_target,
    dependency_graph_source_fingerprint_external_import_target, package_version_from_proof_path,
    proven_external_import_target, proven_external_package_version,
    same_package_cross_version_source_external_import_target,
};

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
            let mut accepted_package_name = package_name.to_string();
            let mut accepted_package_version = package_version.clone();
            let mut accepted_function_matches = source_only_match
                .map(|package_match| package_match.function_signature_matches)
                .unwrap_or_default();
            let mut accepted_string_matches = source_only_match
                .map(|package_match| package_match.string_anchor_matches)
                .unwrap_or_default();
            let mut target = standard_target;
            if target.is_none() {
                target = source_only_match.and_then(|package_match| {
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
            }
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
            let mut attribution = PackageAttributionInput::accepted_external(
                module.id,
                accepted_package_name.as_str(),
                accepted_package_version.as_str(),
                target.export_specifier.as_str(),
            )
            .with_resolved_file(target.source_path.as_str());
            if let Some((_package_name, Some(subpath))) =
                split_bare_specifier(target.export_specifier.as_str())
            {
                attribution = attribution.with_subpath(subpath);
            }
            report.attributions.push(attribution);
            if let Some(index) = source_only_match_indices.get(&module.id).copied() {
                let package_match = &mut report.matches[index];
                package_match.package_name = accepted_package_name.clone();
                package_match.package_version = accepted_package_version.clone();
                package_match.export_specifier = target.export_specifier;
                package_match.source_path = target.source_path;
                package_match.function_signature_matches = accepted_function_matches;
                package_match.string_anchor_matches = accepted_string_matches;
                package_match.external_importable = true;
            } else {
                report.matches.push(PackageMatch {
                    module_id: module.id,
                    package_name: accepted_package_name.clone(),
                    package_version: accepted_package_version.clone(),
                    export_specifier: target.export_specifier,
                    source_path: target.source_path,
                    normalized_source_hash: String::new(),
                    strategy: ModuleMatchStrategy::DependencyClosureOwnership,
                    function_signature_matches: accepted_function_matches,
                    string_anchor_matches: accepted_string_matches,
                    external_importable: true,
                });
            }
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
