//! Package-ownership promotion strategies that ride on the module
//! dependency graph.
//!
//! These two passes promote weakly-attributed package modules to accepted
//! matches when their neighborhood in the bundle's dependency graph
//! already strongly favours the same package. They run after the exact /
//! hash / signature matchers have had their say, and only over modules
//! that no stronger evidence has already settled.

use std::collections::{BTreeMap, BTreeSet};

use reverts_input::InputRows;
use reverts_ir::ModuleKind;

use crate::{
    DependencyNeighborhoodEvidence, ModuleMatchStrategy, PackageMatch, VersionedPackageMatchReport,
    accepted_external_modules, dependency_neighborhood_ownership_evidence,
    dependency_neighborhood_source_path, has_direct_neighborhood_package_contradiction,
    ownership_by_module, package_dependency_components,
};

/// Promote package modules to accepted matches when their direct
/// dependency neighborhood already shows strong same-package ownership.
/// Iterates until a fixed point so each round's promotions feed the next.
pub(crate) fn promote_dependency_closure_ownership_matches(
    rows: &InputRows,
    report: &mut VersionedPackageMatchReport,
) {
    let already_accepted = accepted_external_modules(rows, report);
    let mut matched_modules = report
        .matches
        .iter()
        .map(|package_match| package_match.module_id)
        .collect::<BTreeSet<_>>();
    let mut ownership_by_module = ownership_by_module(rows, report);

    let mut round = 0usize;
    loop {
        round += 1;
        let mut promoted = Vec::<(PackageMatch, DependencyNeighborhoodEvidence)>::new();
        for module in &rows.modules {
            if module.kind != ModuleKind::Package
                || already_accepted.contains(&module.id)
                || matched_modules.contains(&module.id)
            {
                continue;
            }
            let Some(package_name) = module.package_name.as_deref() else {
                continue;
            };
            let Some(evidence) = dependency_neighborhood_ownership_evidence(
                rows,
                module,
                package_name,
                &ownership_by_module,
            ) else {
                continue;
            };
            promoted.push((
                PackageMatch {
                    module_id: module.id,
                    package_name: package_name.to_string(),
                    package_version: evidence.package_version.clone(),
                    export_specifier: package_name.to_string(),
                    source_path: dependency_neighborhood_source_path(
                        package_name,
                        &evidence,
                        round,
                    ),
                    normalized_source_hash: String::new(),
                    strategy: ModuleMatchStrategy::DependencyClosureOwnership,
                    function_signature_matches: evidence.same_package_owned_neighbors,
                    string_anchor_matches: evidence.owned_neighbors,
                    external_importable: false,
                },
                evidence,
            ));
        }
        if promoted.is_empty() {
            break;
        }
        for (package_match, evidence) in promoted {
            matched_modules.insert(package_match.module_id);
            ownership_by_module.insert(
                package_match.module_id,
                (
                    package_match.package_name.clone(),
                    evidence.package_version.clone(),
                ),
            );
            report.matches.push(package_match);
        }
    }
}

/// Promote package modules in connected dependency-graph components when
/// a clear majority of already-owned modules in that component point at
/// the same `(package_name, package_version)`. Useful for sweeping in
/// stragglers that share a tightly-coupled subgraph with already-matched
/// modules.
pub(crate) fn promote_dependency_cluster_ownership_matches(
    rows: &InputRows,
    report: &mut VersionedPackageMatchReport,
) {
    let already_accepted = accepted_external_modules(rows, report);
    let mut matched_modules = report
        .matches
        .iter()
        .map(|package_match| package_match.module_id)
        .collect::<BTreeSet<_>>();
    let mut ownership_by_module = ownership_by_module(rows, report);
    let modules_by_id = rows
        .modules
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();

    for component in package_dependency_components(rows) {
        let component_modules = component
            .iter()
            .filter_map(|module_id| modules_by_id.get(module_id).copied())
            .collect::<Vec<_>>();
        let package_named_count = component_modules
            .iter()
            .filter(|module| module.package_name.is_some())
            .count();
        if package_named_count < 4 {
            continue;
        }
        let component_owned_total = component
            .iter()
            .filter(|module_id| ownership_by_module.contains_key(module_id))
            .count();
        if component_owned_total < 3 {
            continue;
        }
        let mut hint_counts = BTreeMap::<String, usize>::new();
        let mut seed_counts = BTreeMap::<String, BTreeMap<String, usize>>::new();
        for module in &component_modules {
            if let Some(package_name) = module.package_name.as_deref() {
                *hint_counts.entry(package_name.to_string()).or_default() += 1;
            }
            if let Some((package_name, package_version)) = ownership_by_module.get(&module.id) {
                *seed_counts
                    .entry(package_name.clone())
                    .or_default()
                    .entry(package_version.clone())
                    .or_default() += 1;
            }
        }

        for (package_name, hint_count) in hint_counts {
            let Some(version_counts) = seed_counts.get(&package_name) else {
                continue;
            };
            let same_package_seed_count = version_counts.values().sum::<usize>();
            if same_package_seed_count < 3
                || same_package_seed_count * 100 < component_owned_total * 70
                || same_package_seed_count * 100 < hint_count * 10
            {
                continue;
            }
            let Some((package_version, version_seed_count)) = version_counts
                .iter()
                .max_by(|left, right| left.1.cmp(right.1).then_with(|| right.0.cmp(left.0)))
            else {
                continue;
            };
            if *version_seed_count * 100 < same_package_seed_count * 70 {
                continue;
            }

            let target_modules = component_modules
                .iter()
                .copied()
                .filter(|module| {
                    module.package_name.as_deref() == Some(package_name.as_str())
                        && !already_accepted.contains(&module.id)
                        && !matched_modules.contains(&module.id)
                        && module.package_version.as_deref().is_none_or(|expected| {
                            expected.trim().is_empty() || expected.trim() == package_version
                        })
                        && !has_direct_neighborhood_package_contradiction(
                            rows,
                            module.id,
                            package_name.as_str(),
                            &ownership_by_module,
                        )
                })
                .collect::<Vec<_>>();
            if target_modules.is_empty() {
                continue;
            }

            for module in target_modules {
                matched_modules.insert(module.id);
                ownership_by_module
                    .insert(module.id, (package_name.clone(), package_version.clone()));
                report.matches.push(PackageMatch {
                    module_id: module.id,
                    package_name: package_name.clone(),
                    package_version: package_version.clone(),
                    export_specifier: package_name.clone(),
                    source_path: format!(
                        "dependency-cluster:{package_name}@{package_version}:owned_seeds={same_package_seed_count}/{component_owned_total}:version_seeds={version_seed_count}:hinted={hint_count}/{package_named_count}:component_size={}",
                        component.len(),
                    ),
                    normalized_source_hash: String::new(),
                    strategy: ModuleMatchStrategy::DependencyClosureOwnership,
                    function_signature_matches: same_package_seed_count,
                    string_anchor_matches: hint_count,
                    external_importable: false,
                });
            }
        }
    }
}
