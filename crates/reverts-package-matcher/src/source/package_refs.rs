use std::collections::BTreeSet;

use reverts_package::package_source_entry_path_from_source_path as package_source_entry_path_from_raw_source_path;

use crate::package_helpers::{package_source_entry_path, strip_source_extension};
use crate::source::import_targets::{
    export_all_reexport_targets, reexport_targets, relative_module_specifier_targets,
};
use crate::{ExternalImportSourceIndex, PackageSource};

#[must_use]
pub(crate) fn package_source_export_all_reexport_entries(
    source: &PackageSource,
) -> BTreeSet<String> {
    export_all_reexport_targets(source.source.as_str())
        .into_iter()
        .filter_map(|target| {
            resolve_package_relative_require(package_source_entry_path(source).as_str(), &target)
        })
        .map(|entry| {
            strip_source_extension(entry.as_str())
                .trim_matches('/')
                .to_ascii_lowercase()
        })
        .filter(|entry| !entry.is_empty())
        .collect()
}

#[must_use]
pub(crate) fn package_source_reexport_entries(source: &PackageSource) -> BTreeSet<String> {
    reexport_targets(source.source.as_str())
        .into_iter()
        .filter_map(|target| {
            resolve_package_relative_require(package_source_entry_path(source).as_str(), &target)
        })
        .map(|entry| {
            strip_source_extension(entry.as_str())
                .trim_matches('/')
                .to_ascii_lowercase()
        })
        .filter(|entry| !entry.is_empty())
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackageReexportEdgeKind {
    ExportAll,
    AnyReexport,
}

#[must_use]
pub(crate) fn package_source_reexported_source_only_sources<'a>(
    external: &'a PackageSource,
    external_source_index: &'a ExternalImportSourceIndex<'a>,
    edge_kind: PackageReexportEdgeKind,
) -> Vec<&'a PackageSource> {
    let mut results = BTreeSet::<String>::new();
    let mut sources = Vec::<&'a PackageSource>::new();
    let mut visited = BTreeSet::<String>::new();
    let mut stack = vec![external];
    while let Some(source) = stack.pop() {
        if !visited.insert(package_source_cache_key(source)) {
            continue;
        }
        for entry in package_source_reexport_entries_by_kind(source, edge_kind) {
            for target in external_source_index.sources_matching_entry(
                source.package_name.as_str(),
                source.package_version.as_str(),
                entry.as_str(),
            ) {
                if target.external_importable {
                    continue;
                }
                if results.insert(target.source_path.clone()) {
                    sources.push(target);
                }
                stack.push(target);
            }
        }
    }
    sources
}

#[must_use]
pub(crate) fn package_source_reaches_entry_transitively(
    source: &PackageSource,
    matched_entry: &str,
    external_source_index: &ExternalImportSourceIndex<'_>,
    edge_kind: PackageReexportEdgeKind,
) -> bool {
    let mut visited = BTreeSet::<String>::new();
    package_source_reaches_entry_transitively_inner(
        source,
        matched_entry,
        external_source_index,
        edge_kind,
        &mut visited,
    )
}

fn package_source_reaches_entry_transitively_inner(
    source: &PackageSource,
    matched_entry: &str,
    external_source_index: &ExternalImportSourceIndex<'_>,
    edge_kind: PackageReexportEdgeKind,
    visited: &mut BTreeSet<String>,
) -> bool {
    if !visited.insert(package_source_cache_key(source)) {
        return false;
    }
    for entry in package_source_reexport_entries_by_kind(source, edge_kind) {
        if source_entry_paths_match(entry.as_str(), matched_entry) {
            return true;
        }
        for next in external_source_index.sources_matching_entry(
            source.package_name.as_str(),
            source.package_version.as_str(),
            entry.as_str(),
        ) {
            if package_source_reaches_entry_transitively_inner(
                next,
                matched_entry,
                external_source_index,
                edge_kind,
                visited,
            ) {
                return true;
            }
        }
    }
    false
}

fn package_source_reexport_entries_by_kind(
    source: &PackageSource,
    edge_kind: PackageReexportEdgeKind,
) -> BTreeSet<String> {
    match edge_kind {
        PackageReexportEdgeKind::ExportAll => package_source_export_all_reexport_entries(source),
        PackageReexportEdgeKind::AnyReexport => package_source_reexport_entries(source),
    }
}

#[must_use]
pub(crate) fn package_source_dependency_entries(source: &PackageSource) -> BTreeSet<String> {
    relative_module_specifier_targets(source.source.as_str())
        .into_iter()
        .filter_map(|target| {
            resolve_package_relative_require(package_source_entry_path(source).as_str(), &target)
        })
        .map(|entry| {
            strip_source_extension(entry.as_str())
                .trim_matches('/')
                .to_ascii_lowercase()
        })
        .filter(|entry| !entry.is_empty())
        .collect()
}

#[must_use]
pub(crate) fn relative_require_targets_package_source(
    external: &PackageSource,
    target: &str,
    matched: &PackageSource,
) -> bool {
    let Some(resolved) =
        resolve_package_relative_require(package_source_entry_path(external).as_str(), target)
    else {
        return false;
    };
    source_entry_paths_match(
        resolved.as_str(),
        package_source_entry_path(matched).as_str(),
    )
}

#[must_use]
pub(crate) fn resolve_package_relative_require(from_entry: &str, target: &str) -> Option<String> {
    if !target.starts_with('.') {
        return None;
    }
    let from = from_entry.replace('\\', "/");
    let base = from
        .rsplit_once('/')
        .map(|(base, _file)| base)
        .unwrap_or_default();
    let joined = if base.is_empty() {
        target.to_string()
    } else {
        format!("{base}/{target}")
    };
    let mut segments = Vec::<&str>::new();
    for segment in joined.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    (!segments.is_empty()).then(|| segments.join("/"))
}

#[must_use]
pub(crate) fn source_entry_paths_match(left: &str, right: &str) -> bool {
    let left = strip_source_extension(left)
        .trim_matches('/')
        .to_ascii_lowercase();
    let right = strip_source_extension(right)
        .trim_matches('/')
        .to_ascii_lowercase();
    left == right || format!("{left}/index") == right || left == format!("{right}/index")
}

#[must_use]
pub(crate) fn package_source_entry_path_from_source_path(
    package_name: &str,
    package_version: &str,
    source_path: &str,
) -> String {
    package_source_entry_path_from_raw_source_path(package_name, package_version, source_path)
        .to_ascii_lowercase()
}

#[must_use]
pub(crate) fn package_source_cache_key(source: &PackageSource) -> String {
    format!(
        "{}@{}:{}",
        source.package_name, source.package_version, source.source_path
    )
}
