use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use reverts_ir::hash::fnv1a_hex as stable_hash;

use super::{SourceFingerprint, fingerprint_source, package_source_fingerprint_from_source};
use crate::package_helpers::{package_source_entry_path, package_source_external_import_rank};
use crate::source::exported_members::exported_members_from_source;
use crate::source::package_refs::{
    package_source_dependency_entries, package_source_entry_path_from_source_path,
    source_entry_paths_match,
};
use crate::{PackageSource, PackageSourceFingerprint, normalize_source};

#[derive(Debug, Default)]
pub(crate) struct ExternalImportSourceIndex<'a> {
    all_by_version_path:
        BTreeMap<String, BTreeMap<String, BTreeMap<String, Vec<&'a PackageSource>>>>,
    all_by_version: BTreeMap<String, BTreeMap<String, Vec<&'a PackageSource>>>,
    by_version: BTreeMap<String, BTreeMap<String, Vec<&'a PackageSource>>>,
    normalized_by_version_hash:
        BTreeMap<String, BTreeMap<String, BTreeMap<String, Vec<&'a PackageSource>>>>,
    normalized_by_hash: BTreeMap<String, Vec<&'a PackageSource>>,
    export_members_by_source_path: RefCell<BTreeMap<String, BTreeSet<String>>>,
    fingerprints_by_source_path: RefCell<BTreeMap<String, Option<SourceFingerprint>>>,
    dependency_entries_by_source_path: RefCell<BTreeMap<String, BTreeSet<String>>>,
}

impl<'a> ExternalImportSourceIndex<'a> {
    pub(crate) fn build(package_sources: &'a [PackageSource]) -> Self {
        let mut index = Self::default();
        for source in package_sources {
            index
                .all_by_version_path
                .entry(source.package_name.clone())
                .or_default()
                .entry(source.package_version.clone())
                .or_default()
                .entry(source.source_path.clone())
                .or_default()
                .push(source);
            index
                .all_by_version
                .entry(source.package_name.clone())
                .or_default()
                .entry(source.package_version.clone())
                .or_default()
                .push(source);
            if !source.external_importable {
                continue;
            }
            index
                .by_version
                .entry(source.package_name.clone())
                .or_default()
                .entry(source.package_version.clone())
                .or_default()
                .push(source);
            if let Ok(normalized) =
                normalize_source(source.source_path.as_str(), source.source.as_str())
            {
                let normalized_hash = stable_hash(normalized.as_bytes());
                index
                    .normalized_by_version_hash
                    .entry(source.package_name.clone())
                    .or_default()
                    .entry(source.package_version.clone())
                    .or_default()
                    .entry(normalized_hash.clone())
                    .or_default()
                    .push(source);
                index
                    .normalized_by_hash
                    .entry(normalized_hash)
                    .or_default()
                    .push(source);
            }
        }
        for versions in index.all_by_version_path.values_mut() {
            for paths in versions.values_mut() {
                for sources in paths.values_mut() {
                    sort_external_sources(sources);
                }
            }
        }
        for versions in index.all_by_version.values_mut() {
            for sources in versions.values_mut() {
                sort_external_sources(sources);
            }
        }
        for versions in index.by_version.values_mut() {
            for sources in versions.values_mut() {
                sort_external_sources(sources);
            }
        }
        for versions in index.normalized_by_version_hash.values_mut() {
            for hashes in versions.values_mut() {
                for sources in hashes.values_mut() {
                    sort_external_sources(sources);
                }
            }
        }
        for sources in index.normalized_by_hash.values_mut() {
            sort_external_sources(sources);
        }
        index
    }

    pub(crate) fn all_sources_by_path(
        &self,
        package_name: &str,
        package_version: &str,
        source_path: &str,
    ) -> &[&'a PackageSource] {
        self.all_by_version_path
            .get(package_name)
            .and_then(|versions| versions.get(package_version))
            .and_then(|paths| paths.get(source_path))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub(crate) fn all_sources(
        &self,
        package_name: &str,
        package_version: &str,
    ) -> &[&'a PackageSource] {
        self.all_by_version
            .get(package_name)
            .and_then(|versions| versions.get(package_version))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub(crate) fn all_sources_for_package(&self, package_name: &str) -> Vec<&'a PackageSource> {
        self.all_by_version
            .get(package_name)
            .into_iter()
            .flat_map(BTreeMap::values)
            .flat_map(|sources| sources.iter().copied())
            .collect()
    }

    pub(crate) fn normalized_sources_for_any_package(
        &self,
        normalized_hashes: &BTreeSet<String>,
    ) -> Vec<&'a PackageSource> {
        normalized_hashes
            .iter()
            .filter_map(|hash| self.normalized_by_hash.get(hash))
            .flat_map(|sources| sources.iter().copied())
            .collect()
    }

    pub(crate) fn sources(
        &self,
        package_name: &str,
        package_version: &str,
    ) -> &[&'a PackageSource] {
        self.by_version
            .get(package_name)
            .and_then(|versions| versions.get(package_version))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub(crate) fn normalized_sources(
        &self,
        package_name: &str,
        package_version: &str,
        normalized_hash: &str,
    ) -> &[&'a PackageSource] {
        self.normalized_by_version_hash
            .get(package_name)
            .and_then(|versions| versions.get(package_version))
            .and_then(|hashes| hashes.get(normalized_hash))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub(crate) fn export_members(&self, source: &PackageSource) -> BTreeSet<String> {
        let key = format!(
            "{}@{}:{}",
            source.package_name, source.package_version, source.source_path
        );
        if let Some(members) = self.export_members_by_source_path.borrow().get(&key) {
            return members.clone();
        }
        let members =
            exported_members_from_source(source.source_path.as_str(), source.source.as_str());
        self.export_members_by_source_path
            .borrow_mut()
            .insert(key, members.clone());
        members
    }

    pub(crate) fn source_fingerprint(
        &self,
        source: &'a PackageSource,
    ) -> Option<PackageSourceFingerprint<'a>> {
        let key = format!(
            "{}@{}:{}",
            source.package_name, source.package_version, source.source_path
        );
        if let Some(fingerprint) = self.fingerprints_by_source_path.borrow().get(&key) {
            return fingerprint
                .clone()
                .map(|fingerprint| package_source_fingerprint_from_source(source, fingerprint));
        }
        let fingerprint =
            fingerprint_source(source.source_path.as_str(), source.source.as_str()).ok();
        self.fingerprints_by_source_path
            .borrow_mut()
            .insert(key, fingerprint.clone());
        fingerprint.map(|fingerprint| package_source_fingerprint_from_source(source, fingerprint))
    }

    pub(crate) fn dependency_entries(&self, source: &PackageSource) -> BTreeSet<String> {
        let key = format!(
            "{}@{}:{}",
            source.package_name, source.package_version, source.source_path
        );
        if let Some(entries) = self.dependency_entries_by_source_path.borrow().get(&key) {
            return entries.clone();
        }
        let entries = package_source_dependency_entries(source);
        self.dependency_entries_by_source_path
            .borrow_mut()
            .insert(key, entries.clone());
        entries
    }

    pub(crate) fn sources_matching_concrete_path(
        &self,
        package_name: &str,
        package_version: &str,
        source_path: &str,
    ) -> Vec<&'a PackageSource> {
        let exact = self.all_sources_by_path(package_name, package_version, source_path);
        if !exact.is_empty() {
            return exact.to_vec();
        }
        let source_entry =
            package_source_entry_path_from_source_path(package_name, package_version, source_path);
        self.all_sources(package_name, package_version)
            .iter()
            .copied()
            .filter(|source| {
                source_entry_paths_match(
                    package_source_entry_path(source).as_str(),
                    source_entry.as_str(),
                )
            })
            .collect()
    }

    pub(crate) fn sources_matching_entry(
        &self,
        package_name: &str,
        package_version: &str,
        entry: &str,
    ) -> Vec<&'a PackageSource> {
        self.all_sources(package_name, package_version)
            .iter()
            .copied()
            .filter(|source| {
                source_entry_paths_match(package_source_entry_path(source).as_str(), entry)
            })
            .collect()
    }

    pub(crate) fn external_importable_sources_matching_entry(
        &self,
        package_name: &str,
        package_version: &str,
        entry: &str,
    ) -> Vec<&'a PackageSource> {
        self.sources_matching_entry(package_name, package_version, entry)
            .into_iter()
            .filter(|source| source.external_importable)
            .collect()
    }
}

fn sort_external_sources(sources: &mut [&PackageSource]) {
    sources.sort_by(|left, right| compare_external_sources(left, right));
}

fn compare_external_sources(left: &PackageSource, right: &PackageSource) -> Ordering {
    package_source_external_import_rank(left)
        .cmp(&package_source_external_import_rank(right))
        .then_with(|| left.export_specifier.cmp(&right.export_specifier))
        .then_with(|| left.source_path.cmp(&right.source_path))
}
