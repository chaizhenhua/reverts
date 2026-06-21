//! Package-source loading and matching support for CLI use-cases.
//!
//! This module contains the package source/cache adapter logic used by
//! `match-packages` and externalization-hints commands. Keeping it out of the
//! root CLI facade makes the command layer thinner without introducing a new
//! storage crate yet.

mod externalization;

pub(crate) use externalization::promote_package_sources_with_externalization_hints;
pub(crate) use reverts_package::{clean_package_entry_path, package_export_specifier};

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use reverts_input::{
    InputRows, ModuleDependencyTarget, PackageEmissionMode, ProjectInput, SourceFileInput,
};
use reverts_ir::{ModuleKind, is_valid_package_name};
use reverts_observe::{AuditFinding, AuditReport, FindingCode};
use reverts_package_matcher::{
    PackageModuleSourceQuality, PackageSource, clean_package_semantic_path_hint,
    has_accepted_external_attribution, is_exact_package_version_hint,
    package_import_names_from_sources, package_module_source_quality, strip_source_extension,
};
use rusqlite::Connection;
use semver::Version;

use crate::errors::MatchPackagesError;
use crate::persistence::fingerprint_cache::{FingerprintCacheStats, attach_global_fingerprints};
use crate::persistence::source_cache::{
    load_cached_package_sources, persist_package_source_cache, stale_package_source_cache_versions,
};
use crate::pkg_sources;
use crate::pkg_sources::filtering::{
    dedup_package_sources, filter_package_sources_to_best_build_variants,
    filter_package_sources_to_relevant_path_hints,
};
use crate::pkg_sources::version_resolution::materialize_package_sources_from_hints;
use crate::{collect_sqlite_rows, sqlite_table_exists, sqlite_table_has_column};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PackageModuleSourceQualityCounts {
    pub(crate) trusted: usize,
    pub(crate) weak: usize,
    pub(crate) invalid: usize,
    pub(crate) missing: usize,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct LoadedPackageSources {
    pub(crate) sources: Vec<PackageSource>,
    /// Sources for packages with NO bundle module — the libraries a bundler
    /// inlined into the eager entry island. Captured before the module-relevance
    /// filters strip them (those filters keep only sources some module
    /// references, which by definition excludes inlined libraries). The island
    /// anchoring pass matches against this corpus; the per-module matcher does
    /// not see it.
    pub(crate) island_corpus: Vec<PackageSource>,
    pub(crate) fingerprint_cache: FingerprintCacheStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceUnitPackagePathHint {
    package_name: String,
    package_version: Option<String>,
    semantic_path: String,
}
pub(crate) fn remove_package_attributions_for_revalidation(
    rows: &mut InputRows,
    requested_package_names: &BTreeSet<String>,
) -> usize {
    let before = rows.package_attributions.len();
    rows.package_attributions.retain(|attribution| {
        (!requested_package_names.is_empty()
            && !requested_package_names.contains(&attribution.package_name))
            || attribution.emission_mode != PackageEmissionMode::ExternalImport
    });
    before.saturating_sub(rows.package_attributions.len())
}

pub(crate) fn package_module_source_quality_counts(
    rows: &InputRows,
    package_filter: Option<&BTreeSet<String>>,
) -> PackageModuleSourceQualityCounts {
    let mut counts = PackageModuleSourceQualityCounts::default();
    for module in &rows.modules {
        if module.kind != ModuleKind::Package {
            continue;
        }
        if let Some(package_filter) = package_filter
            && !module
                .package_name
                .as_deref()
                .is_some_and(|package_name| package_filter.contains(package_name))
        {
            continue;
        }
        let Some(slice) = rows.module_source_slice(module.id) else {
            counts.missing += 1;
            continue;
        };
        match package_module_source_quality(module, slice.source_file_path, slice.source) {
            PackageModuleSourceQuality::Trusted => counts.trusted += 1,
            PackageModuleSourceQuality::Weak => counts.weak += 1,
            PackageModuleSourceQuality::Invalid => counts.invalid += 1,
        }
    }
    counts
}

pub(crate) fn enrich_package_modules_from_source_units(
    connection: &Connection,
    rows: &mut InputRows,
    project_id: u32,
) -> Result<(), MatchPackagesError> {
    let hints = load_source_unit_package_path_hints(connection, project_id)?;
    if hints.is_empty() {
        return Ok(());
    }
    for module in &mut rows.modules {
        if module.kind != ModuleKind::Package {
            continue;
        }
        let Some(source_file_id) = module.source_file_id else {
            continue;
        };
        let Some(package_name) = module.package_name.as_deref() else {
            continue;
        };
        let Some(file_hints) = hints.get(&source_file_id) else {
            continue;
        };
        let Some(hint) = file_hints
            .iter()
            .find(|hint| hint.package_name == package_name)
        else {
            continue;
        };
        if clean_package_semantic_path_hint(package_name, module.semantic_path.as_str()).is_none()
            || hint.semantic_path.len() > module.semantic_path.len()
        {
            module.semantic_path = hint.semantic_path.clone();
        }
        if module.package_version.is_none() {
            module.package_version = hint.package_version.clone();
        }
    }
    Ok(())
}

fn load_source_unit_package_path_hints(
    connection: &Connection,
    project_id: u32,
) -> Result<BTreeMap<u32, Vec<SourceUnitPackagePathHint>>, MatchPackagesError> {
    if !sqlite_table_exists(connection, "source_units")
        .map_err(MatchPackagesError::QueryPackageSources)?
    {
        return Ok(BTreeMap::new());
    }
    for column in ["file_id", "logical_path", "package_name", "package_version"] {
        if !sqlite_table_has_column(connection, "source_units", column)
            .map_err(MatchPackagesError::QueryPackageSources)?
        {
            return Ok(BTreeMap::new());
        }
    }

    let mut statement = connection
        .prepare(
            r"
            SELECT file_id, logical_path, package_name, package_version
              FROM source_units
             WHERE project_id = ?1
               AND file_id IS NOT NULL
               AND TRIM(COALESCE(logical_path, '')) != ''
            ",
        )
        .map_err(MatchPackagesError::QueryPackageSources)?;
    let rows = statement
        .query_map([i64::from(project_id)], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(MatchPackagesError::QueryPackageSources)?;
    let raw_rows = collect_sqlite_rows(rows).map_err(MatchPackagesError::QueryPackageSources)?;
    let mut hints = BTreeMap::<u32, Vec<SourceUnitPackagePathHint>>::new();
    for (file_id, logical_path, package_name, package_version) in raw_rows {
        let Ok(file_id) = u32::try_from(file_id) else {
            continue;
        };
        let Some((package_name, semantic_path)) =
            source_unit_package_semantic_path(package_name.as_deref(), logical_path.as_str())
        else {
            continue;
        };
        hints
            .entry(file_id)
            .or_default()
            .push(SourceUnitPackagePathHint {
                package_name,
                package_version: package_version
                    .map(|version| version.trim().to_string())
                    .filter(|version| is_exact_package_version_hint(version)),
                semantic_path,
            });
    }
    for file_hints in hints.values_mut() {
        file_hints.sort_by(|left, right| {
            right
                .semantic_path
                .len()
                .cmp(&left.semantic_path.len())
                .then_with(|| left.semantic_path.cmp(&right.semantic_path))
        });
        file_hints.dedup();
    }
    Ok(hints)
}

fn source_unit_package_semantic_path(
    package_name_hint: Option<&str>,
    logical_path: &str,
) -> Option<(String, String)> {
    let clean = logical_path
        .trim()
        .split(['?', '#'])
        .next()
        .unwrap_or(logical_path)
        .replace('\\', "/");
    if clean.is_empty() {
        return None;
    }

    if let Some(package_name) = package_name_hint
        .map(str::trim)
        .filter(|package_name| is_valid_package_name(package_name))
        && let Some(rest) = package_subpath_from_logical_path(package_name, clean.as_str())
    {
        return Some((package_name.to_string(), format!("{package_name}/{rest}")));
    }

    let marker = "node_modules/";
    let index = clean.rfind(marker)?;
    let after = clean.get(index + marker.len()..)?.trim_start_matches('/');
    let mut segments = after.split('/').filter(|segment| !segment.is_empty());
    let first = segments.next()?;
    let package_name = if first.starts_with('@') {
        let second = segments.next()?;
        format!("{first}/{second}")
    } else {
        first.to_string()
    };
    if !is_valid_package_name(package_name.as_str()) {
        return None;
    }
    let rest = segments.collect::<Vec<_>>().join("/");
    if rest.is_empty() {
        return None;
    }
    Some((
        package_name.clone(),
        format!("{package_name}/{}", strip_source_extension(rest.as_str())),
    ))
}

fn package_subpath_from_logical_path<'a>(
    package_name: &str,
    logical_path: &'a str,
) -> Option<&'a str> {
    let package_path = package_name.trim_start_matches('@');
    for marker in [
        format!("node_modules/{package_name}/"),
        format!("node_modules/{package_path}/"),
        format!("/{package_name}/"),
        format!("/{package_path}/"),
        format!("{package_name}/"),
        format!("{package_path}/"),
    ] {
        if let Some(index) = logical_path.find(marker.as_str()) {
            let rest = logical_path.get(index + marker.len()..)?;
            if !rest.trim_matches('/').is_empty() {
                return Some(strip_source_extension(rest.trim_matches('/')));
            }
        }
    }
    None
}

pub(crate) fn package_source_load_scope(
    rows: &InputRows,
    requested_package_names: &[String],
    audit: &mut AuditReport,
) -> BTreeSet<String> {
    if !requested_package_names.is_empty() {
        return package_graph_component_scope(rows, requested_package_names);
    }

    let mut package_names = rows
        .modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
        .filter(|module| !has_accepted_external_attribution(rows, module.id))
        .filter_map(|module| module.package_name.clone())
        .collect::<BTreeSet<_>>();
    match package_import_names_from_sources(rows) {
        Ok(source_import_packages) => package_names.extend(source_import_packages),
        Err(source) => {
            audit.push(
                AuditFinding::error(
                    FindingCode::AstFactExtractionFailed,
                    format!(
                        "failed to parse source-backed package import sites: {}",
                        source.source
                    ),
                )
                .with_module(source.source_file_path),
            );
        }
    }
    package_names
}

pub(crate) fn package_names_from_reference_source_roots(
    roots: &[PathBuf],
    audit: &mut AuditReport,
) -> Result<BTreeSet<String>, MatchPackagesError> {
    Ok(package_names_and_versions_from_reference_source_roots(roots, audit)?.0)
}

/// Like [`package_names_from_reference_source_roots`], but also returns the
/// `package.json` version specifier for each discovered dependency name (when
/// present). Inlined third-party libraries leave no bundle module — and thus no
/// version hint — so the reference manifest is the only place their version can
/// come from for materialization.
pub(crate) fn package_names_and_versions_from_reference_source_roots(
    roots: &[PathBuf],
    audit: &mut AuditReport,
) -> Result<(BTreeSet<String>, BTreeMap<String, String>), MatchPackagesError> {
    let mut package_names = BTreeSet::new();
    let mut package_versions = BTreeMap::new();
    for root in roots {
        collect_reference_package_names(
            root.as_path(),
            &mut package_names,
            &mut package_versions,
            audit,
        )?;
    }
    Ok((package_names, package_versions))
}

fn collect_reference_package_names(
    path: &Path,
    package_names: &mut BTreeSet<String>,
    package_versions: &mut BTreeMap<String, String>,
    audit: &mut AuditReport,
) -> Result<(), MatchPackagesError> {
    let metadata =
        fs::metadata(path).map_err(|source| MatchPackagesError::ReadPackageSourceRoot {
            path: path.to_path_buf(),
            source,
        })?;
    if metadata.is_file() {
        collect_reference_package_names_from_file(path, package_names, package_versions, audit)?;
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }

    let entries =
        fs::read_dir(path).map_err(|source| MatchPackagesError::ReadPackageSourceRoot {
            path: path.to_path_buf(),
            source,
        })?;
    for entry in entries {
        let entry = entry.map_err(|source| MatchPackagesError::ReadPackageSourceRoot {
            path: path.to_path_buf(),
            source,
        })?;
        let entry_path = entry.path();
        if should_skip_reference_dir(entry_path.as_path()) {
            continue;
        }
        collect_reference_package_names(
            entry_path.as_path(),
            package_names,
            package_versions,
            audit,
        )?;
    }
    Ok(())
}

fn collect_reference_package_names_from_file(
    path: &Path,
    package_names: &mut BTreeSet<String>,
    package_versions: &mut BTreeMap<String, String>,
    audit: &mut AuditReport,
) -> Result<(), MatchPackagesError> {
    if path.file_name().and_then(|name| name.to_str()) == Some("package.json") {
        collect_reference_package_names_from_package_json(
            path,
            package_names,
            package_versions,
            audit,
        )?;
        return Ok(());
    }
    if !is_reference_source_file(path) {
        return Ok(());
    }
    let source =
        fs::read_to_string(path).map_err(|source| MatchPackagesError::ReadPackageSourceRoot {
            path: path.to_path_buf(),
            source,
        })?;
    let mut rows = InputRows::new(ProjectInput::new(1, "reference-source"));
    rows.source_files.push(SourceFileInput::new(
        1,
        path.to_string_lossy().into_owned(),
        Some(source),
    ));
    match package_import_names_from_sources(&rows) {
        Ok(import_package_names) => package_names.extend(import_package_names),
        Err(source) => {
            audit.push(
                AuditFinding::warning(
                    FindingCode::AstFactExtractionFailed,
                    format!(
                        "failed to parse reference source package import sites: {}",
                        source.source
                    ),
                )
                .with_module(source.source_file_path),
            );
        }
    }
    Ok(())
}

fn collect_reference_package_names_from_package_json(
    path: &Path,
    package_names: &mut BTreeSet<String>,
    package_versions: &mut BTreeMap<String, String>,
    audit: &mut AuditReport,
) -> Result<(), MatchPackagesError> {
    let content =
        fs::read_to_string(path).map_err(|source| MatchPackagesError::ReadPackageSourceRoot {
            path: path.to_path_buf(),
            source,
        })?;
    let value = match serde_json::from_str::<serde_json::Value>(content.as_str()) {
        Ok(value) => value,
        Err(source) => {
            audit.push(
                AuditFinding::warning(
                    FindingCode::UnparseablePackageSource,
                    format!("failed to parse reference package metadata: {source}"),
                )
                .with_module(path.to_string_lossy()),
            );
            return Ok(());
        }
    };
    // `dependencies` first, then the others: a runtime version specifier should
    // win over a dev/peer one for the same name, so insert runtime last would be
    // wrong — instead only fill a version if not already set by an earlier (more
    // authoritative) field.
    for field in [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ] {
        let Some(dependencies) = value.get(field).and_then(serde_json::Value::as_object) else {
            continue;
        };
        for (package_name, version) in dependencies {
            let package_name = package_name.trim();
            if !is_valid_package_name(package_name) {
                continue;
            }
            package_names.insert(package_name.to_string());
            if let Some(version) = version.as_str() {
                let version = version.trim();
                if !version.is_empty() {
                    package_versions
                        .entry(package_name.to_string())
                        .or_insert_with(|| version.to_string());
                }
            }
        }
    }
    Ok(())
}

fn should_skip_reference_dir(path: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(
            "node_modules"
                | ".git"
                | ".hg"
                | ".svn"
                | "target"
                | "dist"
                | "build"
                | "coverage"
                | ".next"
                | ".turbo"
        )
    )
}

fn is_reference_source_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "mts" | "cts")
    )
}

pub(crate) fn package_graph_component_scope(
    rows: &InputRows,
    requested_package_names: &[String],
) -> BTreeSet<String> {
    let requested = requested_package_names
        .iter()
        .map(|package_name| package_name.trim())
        .filter(|package_name| !package_name.is_empty())
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>();
    if requested.is_empty() {
        return BTreeSet::new();
    }

    let package_by_module = rows
        .modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
        .filter_map(|module| {
            module
                .package_name
                .as_deref()
                .map(str::trim)
                .filter(|package_name| !package_name.is_empty())
                .map(|package_name| (module.id, package_name.to_string()))
        })
        .collect::<BTreeMap<_, _>>();
    if package_by_module.is_empty() {
        return requested;
    }

    let mut neighbors = BTreeMap::<String, BTreeSet<String>>::new();
    for package_name in package_by_module.values() {
        neighbors.entry(package_name.clone()).or_default();
    }
    for dependency in &rows.dependencies {
        let ModuleDependencyTarget::Module(target_id) = dependency.target else {
            continue;
        };
        let Some(from_package) = package_by_module.get(&dependency.from_module_id) else {
            continue;
        };
        let Some(to_package) = package_by_module.get(&target_id) else {
            continue;
        };
        if from_package == to_package {
            continue;
        }
        neighbors
            .entry(from_package.clone())
            .or_default()
            .insert(to_package.clone());
        neighbors
            .entry(to_package.clone())
            .or_default()
            .insert(from_package.clone());
    }

    let mut scope = requested.clone();
    let mut stack = requested.into_iter().collect::<Vec<_>>();
    while let Some(package_name) = stack.pop() {
        for neighbor in neighbors
            .get(package_name.as_str())
            .into_iter()
            .flatten()
            .cloned()
        {
            if scope.insert(neighbor.clone()) {
                stack.push(neighbor);
            }
        }
    }
    scope
}

pub(crate) fn load_package_sources(
    connection: &mut Connection,
    rows: &InputRows,
    package_names: &BTreeSet<String>,
    package_source_roots: &[PathBuf],
    materialize_package_sources: bool,
    persist_materialized_package_sources: bool,
) -> Result<Vec<PackageSource>, MatchPackagesError> {
    Ok(load_package_sources_with_fingerprint_stats(
        connection,
        rows,
        package_names,
        package_source_roots,
        &BTreeMap::new(),
        materialize_package_sources,
        persist_materialized_package_sources,
    )?
    .sources)
}

pub(crate) fn load_package_sources_with_fingerprint_stats(
    connection: &mut Connection,
    rows: &InputRows,
    package_names: &BTreeSet<String>,
    package_source_roots: &[PathBuf],
    reference_versions: &BTreeMap<String, String>,
    materialize_package_sources: bool,
    persist_materialized_package_sources: bool,
) -> Result<LoadedPackageSources, MatchPackagesError> {
    let discovered_root_package_names = if package_source_roots.is_empty() {
        BTreeSet::new()
    } else {
        filter_discovered_package_names_by_source_text(
            rows,
            pkg_sources::discover_local_package_names(package_source_roots)?,
        )
    };
    let mut expanded_package_names = if package_names.is_empty() {
        discovered_root_package_names.clone()
    } else {
        expand_package_names_with_local_dependency_closure(package_names, package_source_roots)?
    };
    expanded_package_names.extend(discovered_root_package_names);

    if expanded_package_names.is_empty() {
        return Ok(LoadedPackageSources::default());
    }

    let has_package_source_cache = sqlite_table_exists(connection, "package_source_cache")
        .map_err(MatchPackagesError::QueryPackageSources)?;
    if !has_package_source_cache && package_source_roots.is_empty() && !materialize_package_sources
    {
        return Err(MatchPackagesError::MissingTable("package_source_cache"));
    }

    let stale_cache_versions = if has_package_source_cache && materialize_package_sources {
        stale_package_source_cache_versions(connection, &expanded_package_names)?
    } else {
        BTreeSet::new()
    };

    let mut package_sources = if has_package_source_cache {
        load_cached_package_sources(
            connection,
            &expanded_package_names,
            materialize_package_sources,
        )?
    } else {
        Vec::new()
    };
    package_sources.extend(load_package_sources_from_roots(
        &expanded_package_names,
        package_source_roots,
    )?);
    if materialize_package_sources {
        let materialized_sources = materialize_package_sources_from_hints(
            rows,
            &expanded_package_names,
            &package_sources,
            &stale_cache_versions,
            reference_versions,
        )?;
        if persist_materialized_package_sources && !materialized_sources.is_empty() {
            persist_package_source_cache(connection, &materialized_sources)?;
        }
        package_sources.extend(materialized_sources);
    }
    promote_package_sources_with_externalization_hints(
        connection,
        &expanded_package_names,
        &mut package_sources,
    )?;
    // Capture the inlined-library corpus (packages with no bundle module) BEFORE
    // the module-relevance filters below, which keep only sources some module
    // references and would otherwise strip every inlined library.
    let island_corpus = island_corpus_from_sources(rows, &package_sources);
    filter_package_sources_to_best_build_variants(rows, &mut package_sources);
    filter_package_sources_to_relevant_path_hints(rows, &mut package_sources);
    dedup_package_sources(&mut package_sources);
    // Attach globally-cached fingerprints (shared across projects) so the
    // matcher skips re-parsing sources whose fingerprint was already computed
    // on any prior run, on any project.
    let fingerprint_cache = attach_global_fingerprints(&mut package_sources);
    Ok(LoadedPackageSources {
        sources: package_sources,
        island_corpus,
        fingerprint_cache,
    })
}

/// Sources for packages that have no bundle module — the inlined-library corpus
/// for entry-island anchoring. Deduplicated by `(package, version, path)`.
fn island_corpus_from_sources(rows: &InputRows, sources: &[PackageSource]) -> Vec<PackageSource> {
    let module_package_names: BTreeSet<&str> = rows
        .modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
        .filter_map(|module| module.package_name.as_deref())
        .collect();
    let mut corpus: Vec<PackageSource> = sources
        .iter()
        .filter(|source| !module_package_names.contains(source.package_name.as_str()))
        .cloned()
        .collect();
    dedup_package_sources(&mut corpus);
    corpus
}

pub(crate) fn filter_package_sources_to_referenced_package_versions(
    rows: &InputRows,
    package_sources: &mut Vec<PackageSource>,
) -> usize {
    let referenced_versions = rows
        .modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
        .filter_map(|module| {
            let package_name = module.package_name.as_deref()?.trim();
            let package_version = module
                .package_version
                .as_deref()
                .map(str::trim)
                .filter(|version| Version::parse(version).is_ok())?;
            Some((package_name.to_string(), package_version.to_string()))
        })
        .fold(
            BTreeMap::<String, BTreeSet<String>>::new(),
            |mut versions, (package_name, package_version)| {
                versions
                    .entry(package_name)
                    .or_default()
                    .insert(package_version);
                versions
            },
        );
    if referenced_versions.is_empty() {
        return 0;
    }

    let before = package_sources.len();
    package_sources.retain(|source| {
        referenced_versions
            .get(source.package_name.as_str())
            .is_none_or(|versions| versions.contains(source.package_version.as_str()))
    });
    before.saturating_sub(package_sources.len())
}

fn expand_package_names_with_local_dependency_closure(
    seed_package_names: &BTreeSet<String>,
    package_source_roots: &[PathBuf],
) -> Result<BTreeSet<String>, MatchPackagesError> {
    if seed_package_names.is_empty() || package_source_roots.is_empty() {
        return Ok(seed_package_names.clone());
    }
    let mut expanded = seed_package_names.clone();
    let mut stack = seed_package_names.iter().cloned().collect::<Vec<_>>();
    while let Some(package_name) = stack.pop() {
        for root in package_source_roots {
            for package_dir in pkg_sources::package_dir_candidates(root.as_path(), &package_name) {
                let Some(metadata) = pkg_sources::local_package_metadata(package_dir.as_path())?
                else {
                    continue;
                };
                if metadata.name != package_name {
                    continue;
                }
                for dependency in local_runtime_dependency_names(package_dir.as_path())? {
                    if expanded.insert(dependency.clone()) {
                        stack.push(dependency);
                    }
                }
            }
        }
    }
    Ok(expanded)
}

fn local_runtime_dependency_names(
    package_dir: &Path,
) -> Result<BTreeSet<String>, MatchPackagesError> {
    let package_json_path = package_dir.join("package.json");
    let content = fs::read_to_string(package_json_path.as_path()).map_err(|source| {
        MatchPackagesError::ReadPackageSourceRoot {
            path: package_json_path.clone(),
            source,
        }
    })?;
    let value = serde_json::from_str::<serde_json::Value>(content.as_str()).map_err(|source| {
        MatchPackagesError::InvalidPackageMetadata {
            path: package_json_path.clone(),
            source,
        }
    })?;
    let mut names = BTreeSet::new();
    for field in ["dependencies", "optionalDependencies", "peerDependencies"] {
        let Some(dependencies) = value.get(field).and_then(serde_json::Value::as_object) else {
            continue;
        };
        for package_name in dependencies.keys() {
            let package_name = package_name.trim();
            if is_valid_package_name(package_name) {
                names.insert(package_name.to_string());
            }
        }
    }
    Ok(names)
}

fn filter_discovered_package_names_by_source_text(
    rows: &InputRows,
    package_names: BTreeSet<String>,
) -> BTreeSet<String> {
    if package_names.is_empty() {
        return package_names;
    }
    let haystack = rows
        .source_files
        .iter()
        .filter_map(|source_file| source_file.source.as_deref())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join("\n");
    if haystack.is_empty() {
        return package_names;
    }
    package_names
        .into_iter()
        .filter(|package_name| {
            let needle = package_name.to_ascii_lowercase();
            needle.len() >= 3 && haystack.contains(needle.as_str())
        })
        .collect()
}

fn load_package_sources_from_roots(
    package_names: &BTreeSet<String>,
    package_source_roots: &[PathBuf],
) -> Result<Vec<PackageSource>, MatchPackagesError> {
    let mut sources = Vec::new();
    for root in package_source_roots {
        for package_name in package_names {
            for package_dir in
                pkg_sources::package_dir_candidates(root.as_path(), package_name.as_str())
            {
                let Some(metadata) = pkg_sources::local_package_metadata(package_dir.as_path())?
                else {
                    continue;
                };
                if metadata.name != *package_name {
                    continue;
                }
                pkg_sources::collect_local_package_sources(
                    package_dir.as_path(),
                    &metadata,
                    &mut sources,
                )?;
            }
        }
    }
    Ok(sources)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_package(root: &Path, name: &str, version: &str, extra: &str) {
        let package_dir = name
            .split('/')
            .fold(root.join("node_modules"), |path, segment| {
                path.join(segment)
            });
        std::fs::create_dir_all(package_dir.as_path()).expect("package dir");
        std::fs::write(
            package_dir.join("package.json"),
            format!(
                r#"{{
  "name": "{name}",
  "version": "{version}",
  "main": "index.js"
  {extra}
}}"#
            ),
        )
        .expect("package json");
        std::fs::write(package_dir.join("index.js"), "module.exports = 1;\n").expect("source");
    }

    #[test]
    fn package_source_scope_expands_local_runtime_dependency_closure() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        write_package(
            tempdir.path(),
            "seed",
            "1.0.0",
            r#",
  "dependencies": {"dep": "^1.0.0"},
  "optionalDependencies": {"@scope/opt": "^1.0.0"}
"#,
        );
        write_package(
            tempdir.path(),
            "dep",
            "1.0.0",
            r#",
  "dependencies": {"leaf": "^1.0.0"}
"#,
        );
        write_package(tempdir.path(), "@scope/opt", "1.0.0", "");
        write_package(tempdir.path(), "leaf", "1.0.0", "");
        write_package(tempdir.path(), "unrelated", "1.0.0", "");

        let seed = BTreeSet::from(["seed".to_string()]);
        let expanded =
            expand_package_names_with_local_dependency_closure(&seed, &[tempdir.path().into()])
                .expect("expand");

        assert_eq!(
            expanded,
            BTreeSet::from([
                "@scope/opt".to_string(),
                "dep".to_string(),
                "leaf".to_string(),
                "seed".to_string()
            ])
        );
    }

    #[test]
    fn reference_source_roots_discover_imports_and_package_json_dependencies() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let src_dir = tempdir.path().join("src");
        std::fs::create_dir_all(src_dir.as_path()).expect("src dir");
        std::fs::write(
            src_dir.join("app.ts"),
            r#"
import React from "react";
import memoize from "lodash-es/memoize.js";
import scoped from "@scope/pkg/sub/path.js";
import fs from "node:fs";
const dynamic = import("zod");
const required = require("semver/functions/valid");
export { scoped };
void React;
void memoize;
void fs;
void dynamic;
void required;
"#,
        )
        .expect("source");
        std::fs::write(
            tempdir.path().join("package.json"),
            r#"{
  "dependencies": {"chalk": "^5.0.0"},
  "devDependencies": {"@types/node": "*"},
  "peerDependencies": {"react": "^18.0.0"},
  "optionalDependencies": {"left-pad": "^1.3.0"}
}"#,
        )
        .expect("package json");
        let skipped_dir = tempdir.path().join("node_modules").join("ignored");
        std::fs::create_dir_all(skipped_dir.as_path()).expect("skipped dir");
        std::fs::write(
            skipped_dir.join("index.js"),
            r#"import ignored from "should-not-scan"; void ignored;"#,
        )
        .expect("skipped source");

        let mut audit = AuditReport::default();
        let package_names =
            package_names_from_reference_source_roots(&[tempdir.path().into()], &mut audit)
                .expect("package names");

        assert_eq!(
            package_names,
            BTreeSet::from([
                "@scope/pkg".to_string(),
                "@types/node".to_string(),
                "chalk".to_string(),
                "left-pad".to_string(),
                "lodash-es".to_string(),
                "react".to_string(),
                "semver".to_string(),
                "zod".to_string()
            ])
        );
        assert!(audit.findings().is_empty());
    }

    #[test]
    fn reference_package_json_version_specifiers_are_captured() {
        // The version of an inlined library (no bundle module) can only come from
        // the reference manifest, so the scanner must surface name -> specifier.
        let tempdir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tempdir.path().join("package.json"),
            r#"{
  "dependencies": {"zod": "^3.25.64"},
  "devDependencies": {"react": "18.2.0", "blank": ""}
}"#,
        )
        .expect("package json");

        let mut audit = AuditReport::default();
        let (names, versions) = package_names_and_versions_from_reference_source_roots(
            &[tempdir.path().into()],
            &mut audit,
        )
        .expect("names and versions");

        assert!(names.contains("zod") && names.contains("react"));
        assert_eq!(versions.get("zod").map(String::as_str), Some("^3.25.64"));
        assert_eq!(versions.get("react").map(String::as_str), Some("18.2.0"));
        // An empty specifier contributes a name but no version.
        assert!(names.contains("blank"));
        assert!(!versions.contains_key("blank"));
    }

    #[test]
    fn island_corpus_keeps_only_sources_without_a_bundle_module() {
        use reverts_input::ModuleInput;
        use reverts_ir::ModuleId;
        use reverts_package_matcher::PackageSource;

        let mut rows = InputRows::new(ProjectInput::new(1, "p"));
        rows.modules.push(ModuleInput::package(
            ModuleId(1),
            "node_modules/node-pty/index.js",
            "node_modules/node-pty/index.js",
            "node-pty",
            Some("1.0.0".to_string()),
        ));

        let sources = vec![
            // Has a bundle module — belongs to the per-module matcher, not the island.
            PackageSource::external("node-pty", "1.0.0", "node-pty", "node-pty/index.js", "1;"),
            // Inlined library, no module — the island corpus.
            PackageSource::external("zod", "3.25.64", "zod", "zod/index.js", "2;"),
        ];

        let corpus = island_corpus_from_sources(&rows, &sources);

        assert_eq!(corpus.len(), 1, "only the no-module source: {corpus:?}");
        assert_eq!(corpus[0].package_name, "zod");
    }
}
