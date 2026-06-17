//! Pass-local scratch facts shared by external-import proof strategies.
//!
//! This module stores memoized mechanism data only: parsed fingerprints,
//! direct graph neighborhoods, and source graph evidence. Proof modules keep
//! the policy/score decisions outside this cache.

use std::cell::RefCell;
use std::collections::BTreeMap;

use reverts_input::{InputRows, ModuleInput};
use reverts_ir::{ModuleId, ModuleKind};

use crate::package_helpers::{
    direct_module_dependencies, direct_module_dependents, package_source_entry_path,
};
use crate::source::package_refs::{
    package_source_cache_key, package_source_entry_path_from_source_path, source_entry_paths_match,
};
use crate::{
    ConcretePackageSourcePath, ExternalImportSourceIndex, ModuleMatchFingerprint, PackageSource,
    PackageSourceFingerprint, module_match_fingerprint,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DependencyGraphEvidence {
    pub(crate) matched_edges: usize,
    pub(crate) known_edges: usize,
}

/// Pass-local memoization for external-import proof strategies.
///
/// This is scratch state for proof execution, not a package-source index and
/// not a policy object. Keeping it named and scoped as scratch makes the
/// mechanism/strategy boundary explicit: callers may reuse expensive facts
/// while each proof strategy still owns its own allow/score decisions.
#[derive(Debug, Default)]
pub(crate) struct ExternalImportProofScratch<'a> {
    pub(crate) module_fingerprints: RefCell<BTreeMap<ModuleId, Option<ModuleMatchFingerprint>>>,
    pub(crate) source_fingerprints_by_version:
        RefCell<BTreeMap<(String, String), Vec<PackageSourceFingerprint<'a>>>>,
    pub(crate) source_fingerprints_by_package:
        RefCell<BTreeMap<String, Vec<PackageSourceFingerprint<'a>>>>,
    pub(crate) dependency_graph_evidence:
        RefCell<BTreeMap<(ModuleId, String, String), DependencyGraphEvidence>>,
    pub(crate) direct_dependencies: RefCell<BTreeMap<ModuleId, Vec<ModuleId>>>,
    pub(crate) direct_dependents: RefCell<BTreeMap<ModuleId, Vec<ModuleId>>>,
    pub(crate) package_modules_by_id: RefCell<Option<BTreeMap<ModuleId, (String, String)>>>,
}

impl<'a> ExternalImportProofScratch<'a> {
    pub(crate) fn module_fingerprint(
        &self,
        module: &ModuleInput,
        path: &str,
        source: &str,
    ) -> Option<ModuleMatchFingerprint> {
        if let Some(fingerprint) = self.module_fingerprints.borrow().get(&module.id) {
            return fingerprint.clone();
        }
        let fingerprint = module_match_fingerprint(module, path, source).ok();
        self.module_fingerprints
            .borrow_mut()
            .insert(module.id, fingerprint.clone());
        fingerprint
    }

    pub(crate) fn source_fingerprints_for_version(
        &self,
        external_source_index: &ExternalImportSourceIndex<'a>,
        package_name: &str,
        package_version: &str,
    ) -> Vec<PackageSourceFingerprint<'a>> {
        let key = (package_name.to_string(), package_version.to_string());
        if let Some(fingerprints) = self.source_fingerprints_by_version.borrow().get(&key) {
            return fingerprints.clone();
        }
        let fingerprints = external_source_index
            .all_sources(package_name, package_version)
            .iter()
            .filter(|source| source.is_within_fingerprint_budget())
            .filter_map(|source| external_source_index.source_fingerprint(source))
            .collect::<Vec<_>>();
        self.source_fingerprints_by_version
            .borrow_mut()
            .insert(key, fingerprints.clone());
        fingerprints
    }

    pub(crate) fn source_fingerprints_for_package(
        &self,
        external_source_index: &ExternalImportSourceIndex<'a>,
        package_name: &str,
    ) -> Vec<PackageSourceFingerprint<'a>> {
        if let Some(fingerprints) = self
            .source_fingerprints_by_package
            .borrow()
            .get(package_name)
        {
            return fingerprints.clone();
        }
        let fingerprints = external_source_index
            .all_sources_for_package(package_name)
            .into_iter()
            .filter(|source| source.is_within_fingerprint_budget())
            .filter_map(|source| external_source_index.source_fingerprint(source))
            .collect::<Vec<_>>();
        self.source_fingerprints_by_package
            .borrow_mut()
            .insert(package_name.to_string(), fingerprints.clone());
        fingerprints
    }

    pub(crate) fn dependency_graph_evidence(
        &self,
        rows: &InputRows,
        module_id: ModuleId,
        candidate: &PackageSource,
        external_source_index: &ExternalImportSourceIndex<'a>,
        concrete_sources_by_module: &BTreeMap<ModuleId, ConcretePackageSourcePath>,
    ) -> DependencyGraphEvidence {
        let dependency_ids = self.direct_dependencies(rows, module_id);
        let dependent_ids = self.direct_dependents(rows, module_id);
        let neighborhood_signature = dependency_graph_concrete_neighborhood_signature(
            dependency_ids.as_slice(),
            dependent_ids.as_slice(),
            candidate,
            concrete_sources_by_module,
        );
        let key = (
            module_id,
            package_source_cache_key(candidate),
            neighborhood_signature,
        );
        if let Some(evidence) = self.dependency_graph_evidence.borrow().get(&key) {
            return *evidence;
        }
        let evidence = dependency_graph_source_evidence(
            candidate,
            external_source_index,
            concrete_sources_by_module,
            dependency_ids.as_slice(),
            dependent_ids.as_slice(),
        );
        self.dependency_graph_evidence
            .borrow_mut()
            .insert(key, evidence);
        evidence
    }

    pub(crate) fn direct_dependencies(
        &self,
        rows: &InputRows,
        module_id: ModuleId,
    ) -> Vec<ModuleId> {
        if let Some(dependencies) = self.direct_dependencies.borrow().get(&module_id) {
            return dependencies.clone();
        }
        let dependencies = direct_module_dependencies(rows, module_id);
        self.direct_dependencies
            .borrow_mut()
            .insert(module_id, dependencies.clone());
        dependencies
    }

    pub(crate) fn direct_dependents(&self, rows: &InputRows, module_id: ModuleId) -> Vec<ModuleId> {
        if let Some(dependents) = self.direct_dependents.borrow().get(&module_id) {
            return dependents.clone();
        }
        let dependents = direct_module_dependents(rows, module_id);
        self.direct_dependents
            .borrow_mut()
            .insert(module_id, dependents.clone());
        dependents
    }

    pub(crate) fn row_module_is_same_package_version(
        &self,
        rows: &InputRows,
        module_id: ModuleId,
        package_name: &str,
        package_version: &str,
    ) -> bool {
        if self.package_modules_by_id.borrow().is_none() {
            let package_modules = rows
                .modules
                .iter()
                .filter(|module| module.kind == ModuleKind::Package)
                .filter_map(|module| {
                    Some((
                        module.id,
                        (
                            module.package_name.as_deref()?.to_string(),
                            module.package_version.as_deref()?.to_string(),
                        ),
                    ))
                })
                .collect::<BTreeMap<_, _>>();
            *self.package_modules_by_id.borrow_mut() = Some(package_modules);
        }
        self.package_modules_by_id
            .borrow()
            .as_ref()
            .and_then(|package_modules| package_modules.get(&module_id))
            .is_some_and(|(name, version)| name == package_name && version == package_version)
    }
}

fn dependency_graph_concrete_neighborhood_signature(
    dependency_ids: &[ModuleId],
    dependent_ids: &[ModuleId],
    candidate: &PackageSource,
    concrete_sources_by_module: &BTreeMap<ModuleId, ConcretePackageSourcePath>,
) -> String {
    let mut parts = Vec::new();
    for dependency_id in dependency_ids {
        if let Some(concrete) = concrete_sources_by_module.get(dependency_id)
            && concrete.package_name == candidate.package_name
            && concrete.package_version == candidate.package_version
        {
            parts.push(format!("d{}={}", dependency_id.0, concrete.source_path));
        }
    }
    for dependent_id in dependent_ids {
        if let Some(concrete) = concrete_sources_by_module.get(dependent_id)
            && concrete.package_name == candidate.package_name
            && concrete.package_version == candidate.package_version
        {
            parts.push(format!("r{}={}", dependent_id.0, concrete.source_path));
        }
    }
    parts.join("|")
}

fn dependency_graph_source_evidence(
    candidate: &PackageSource,
    external_source_index: &ExternalImportSourceIndex<'_>,
    concrete_sources_by_module: &BTreeMap<ModuleId, ConcretePackageSourcePath>,
    dependency_ids: &[ModuleId],
    dependent_ids: &[ModuleId],
) -> DependencyGraphEvidence {
    let candidate_entry = package_source_entry_path(candidate);
    let candidate_deps = external_source_index.dependency_entries(candidate);
    let mut known_edges = 0usize;
    let mut matched_edges = 0usize;

    for dependency_id in dependency_ids {
        let Some(neighbor) = concrete_sources_by_module.get(dependency_id) else {
            continue;
        };
        if neighbor.package_name != candidate.package_name
            || neighbor.package_version != candidate.package_version
        {
            continue;
        }
        known_edges += 1;
        let neighbor_entry = package_source_entry_path_from_source_path(
            neighbor.package_name.as_str(),
            neighbor.package_version.as_str(),
            neighbor.source_path.as_str(),
        );
        if candidate_deps
            .iter()
            .any(|target| source_entry_paths_match(target.as_str(), neighbor_entry.as_str()))
        {
            matched_edges += 1;
        }
    }

    for dependent_id in dependent_ids {
        let Some(neighbor) = concrete_sources_by_module.get(dependent_id) else {
            continue;
        };
        if neighbor.package_name != candidate.package_name
            || neighbor.package_version != candidate.package_version
        {
            continue;
        }
        let neighbor_sources = external_source_index.sources_matching_concrete_path(
            neighbor.package_name.as_str(),
            neighbor.package_version.as_str(),
            neighbor.source_path.as_str(),
        );
        if neighbor_sources.is_empty() {
            continue;
        }
        known_edges += 1;
        if neighbor_sources.iter().any(|neighbor_source| {
            external_source_index
                .dependency_entries(neighbor_source)
                .iter()
                .any(|target| source_entry_paths_match(target.as_str(), candidate_entry.as_str()))
        }) {
            matched_edges += 1;
        }
    }

    DependencyGraphEvidence {
        matched_edges,
        known_edges,
    }
}
