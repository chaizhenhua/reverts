//! Package-source loading and matching support for CLI use-cases.
//!
//! This module contains the package source/cache adapter logic used by
//! `match-packages` and externalization-hints commands. Keeping it out of the
//! root CLI facade makes the command layer thinner without introducing a new
//! storage crate yet.

mod externalization;

pub(crate) use externalization::{
    externalization_hint_candidates_from_cache, hint_export_specifier_matches_package,
    promote_package_sources_with_externalization_hints,
};

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use reverts_input::{InputRows, ModuleDependencyTarget, PackageEmissionMode};
use reverts_ir::{ModuleKind, is_valid_package_name};
use reverts_observe::{AuditFinding, AuditReport, FindingCode};
use reverts_package_matcher::{
    PackageModuleSourceQuality, PackageSource, clean_package_semantic_path_hint,
    has_accepted_external_attribution, is_exact_package_version_hint,
    package_import_names_from_sources, package_module_source_quality, strip_source_extension,
};
use rusqlite::{Connection, params_from_iter};
use semver::Version;

use crate::errors::MatchPackagesError;
use crate::persistence::source_cache::{
    PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION, package_source_from_row,
    persist_package_source_cache, stale_package_source_cache_versions,
};
use crate::pkg_sources;
use crate::pkg_sources::filtering::{
    dedup_package_sources, filter_package_sources_to_best_build_variants,
    filter_package_sources_to_relevant_path_hints,
};
use crate::pkg_sources::version_resolution::materialize_package_sources_from_hints;
use crate::{
    collect_sqlite_rows, sqlite_placeholders, sqlite_table_exists, sqlite_table_has_column,
};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PackageModuleSourceQualityCounts {
    pub(crate) trusted: usize,
    pub(crate) weak: usize,
    pub(crate) invalid: usize,
    pub(crate) missing: usize,
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
    if package_names.is_empty() {
        return Ok(Vec::new());
    }

    let has_package_source_cache = sqlite_table_exists(connection, "package_source_cache")
        .map_err(MatchPackagesError::QueryPackageSources)?;
    if !has_package_source_cache && package_source_roots.is_empty() && !materialize_package_sources
    {
        return Err(MatchPackagesError::MissingTable("package_source_cache"));
    }

    let stale_cache_versions = if has_package_source_cache && materialize_package_sources {
        stale_package_source_cache_versions(connection, package_names)?
    } else {
        BTreeSet::new()
    };

    let mut package_sources = if has_package_source_cache {
        load_cached_package_sources(connection, package_names, materialize_package_sources)?
    } else {
        Vec::new()
    };
    package_sources.extend(load_package_sources_from_roots(
        package_names,
        package_source_roots,
    )?);
    if materialize_package_sources {
        let materialized_sources = materialize_package_sources_from_hints(
            rows,
            package_names,
            &package_sources,
            &stale_cache_versions,
        )?;
        if persist_materialized_package_sources && !materialized_sources.is_empty() {
            persist_package_source_cache(connection, &materialized_sources)?;
        }
        package_sources.extend(materialized_sources);
    }
    promote_package_sources_with_externalization_hints(
        connection,
        package_names,
        &mut package_sources,
    )?;
    filter_package_sources_to_best_build_variants(rows, &mut package_sources);
    filter_package_sources_to_relevant_path_hints(rows, &mut package_sources);
    dedup_package_sources(&mut package_sources);
    Ok(package_sources)
}

#[derive(Debug, Clone, Copy)]
struct PackageSourceCacheColumns {
    has_external_importable: bool,
    has_external_import_policy_version: bool,
    has_export_specifier: bool,
    has_fingerprint: bool,
}

impl PackageSourceCacheColumns {
    fn load(connection: &Connection) -> Result<Self, MatchPackagesError> {
        Ok(Self {
            has_external_importable: sqlite_table_has_column(
                connection,
                "package_source_cache",
                "external_importable",
            )
            .map_err(MatchPackagesError::QueryPackageSources)?,
            has_external_import_policy_version: sqlite_table_has_column(
                connection,
                "package_source_cache",
                "external_import_policy_version",
            )
            .map_err(MatchPackagesError::QueryPackageSources)?,
            has_export_specifier: sqlite_table_has_column(
                connection,
                "package_source_cache",
                "export_specifier",
            )
            .map_err(MatchPackagesError::QueryPackageSources)?,
            has_fingerprint: sqlite_table_has_column(
                connection,
                "package_source_cache",
                "fingerprint_json",
            )
            .map_err(MatchPackagesError::QueryPackageSources)?,
        })
    }

    /// SELECT expression yielding the cached fingerprint JSON (or `NULL` on
    /// older snapshots without the column), as the 7th projected column.
    fn fingerprint_select_expr(self) -> &'static str {
        if self.has_fingerprint {
            "fingerprint_json"
        } else {
            "NULL"
        }
    }

    fn has_current_external_import_policy(self) -> bool {
        self.has_external_import_policy_version && self.has_export_specifier
    }

    fn cache_import_policy_predicate(self, materialize_package_sources: bool) -> String {
        if materialize_package_sources {
            self.current_import_policy_predicate()
        } else {
            "1".to_string()
        }
    }

    fn current_import_policy_predicate(self) -> String {
        if self.has_current_external_import_policy() {
            format!(
                "external_import_policy_version = {PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION}"
            )
        } else {
            "0".to_string()
        }
    }

    fn external_importable_select_expr(self) -> String {
        if self.has_external_importable && self.has_current_external_import_policy() {
            format!(
                "CASE WHEN external_import_policy_version = {PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION} THEN external_importable ELSE 0 END"
            )
        } else {
            "0".to_string()
        }
    }

    fn export_specifier_select_expr(self) -> String {
        if self.has_current_external_import_policy() {
            format!(
                "CASE WHEN external_import_policy_version = {PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION} AND TRIM(COALESCE(export_specifier, '')) != '' THEN export_specifier ELSE '' END"
            )
        } else {
            "''".to_string()
        }
    }
}

fn load_cached_package_sources(
    connection: &Connection,
    package_names: &BTreeSet<String>,
    materialize_package_sources: bool,
) -> Result<Vec<PackageSource>, MatchPackagesError> {
    let columns = PackageSourceCacheColumns::load(connection)?;
    let cache_import_policy_predicate =
        columns.cache_import_policy_predicate(materialize_package_sources);
    let external_importable_expr = columns.external_importable_select_expr();
    let export_specifier_expr = columns.export_specifier_select_expr();
    let fingerprint_expr = columns.fingerprint_select_expr();
    let mut sql = String::from(
        format!(
            r"
        SELECT package_name, package_version, entry_path, source_content,
               {external_importable_expr},
               {export_specifier_expr},
               {fingerprint_expr}
          FROM package_source_cache
         WHERE TRIM(COALESCE(package_name, '')) != ''
           AND TRIM(COALESCE(package_version, '')) != ''
           AND TRIM(COALESCE(entry_path, '')) != ''
           AND TRIM(COALESCE(source_content, '')) != ''
           AND ({cache_import_policy_predicate})
        "
        )
        .as_str(),
    );
    if !package_names.is_empty() {
        use std::fmt::Write as _;
        let _ = write!(
            sql,
            " AND package_name IN ({})",
            sqlite_placeholders(package_names.len())
        );
    }
    sql.push_str(" ORDER BY package_name, package_version, entry_path");

    let mut statement = connection
        .prepare(sql.as_str())
        .map_err(MatchPackagesError::QueryPackageSources)?;
    if package_names.is_empty() {
        let rows = statement
            .query_map([], package_source_from_row)
            .map_err(MatchPackagesError::QueryPackageSources)?;
        collect_sqlite_rows(rows).map_err(MatchPackagesError::QueryPackageSources)
    } else {
        let rows = statement
            .query_map(
                params_from_iter(package_names.iter()),
                package_source_from_row,
            )
            .map_err(MatchPackagesError::QueryPackageSources)?;
        collect_sqlite_rows(rows).map_err(MatchPackagesError::QueryPackageSources)
    }
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

pub(crate) fn package_export_specifier(package_name: &str, entry_path: &str) -> String {
    let clean_path = clean_package_entry_path(entry_path);
    if clean_path.is_empty() || clean_path == "." {
        package_name.to_string()
    } else {
        format!("{package_name}/{clean_path}")
    }
}

pub(crate) fn clean_package_entry_path(entry_path: &str) -> String {
    entry_path
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
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
