//! Resolve package-name modules to specific semver versions using
//! local package sources, project-supplied exact versions, and (as a
//! last resort) source-identity hashing or `npm` lookups. The resolved
//! versions are written back into the [`InputRows`] before matching so
//! the matcher only sees concrete `(name, version)` keys.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use reverts_input::{InputRows, ModuleInput};
use reverts_ir::hash::fnv1a_hex as stable_hash;
use reverts_ir::{ModuleId, ModuleKind, is_valid_package_name};
use reverts_js::normalize_source_for_pipeline;
use reverts_package_matcher::{
    PackageModuleSourceQuality, PackageSource, is_exact_package_version_hint,
    package_module_source_quality,
};
use semver::{BuildMetadata, Comparator, Op, Version, VersionReq};

use crate::errors::MatchPackagesError;
use crate::pkg_sources;

#[derive(Debug)]
pub(crate) struct PackageVersionResolutionPlan {
    available_versions: BTreeMap<String, BTreeSet<Version>>,
    project_exact_versions: BTreeMap<String, BTreeMap<Version, usize>>,
    source_identity_versions: BTreeMap<ModuleId, Version>,
}

impl PackageVersionResolutionPlan {
    pub(crate) fn build(
        rows: &InputRows,
        package_names: &BTreeSet<String>,
        existing_sources: &[PackageSource],
    ) -> Result<Self, MatchPackagesError> {
        let available_versions = exact_package_source_versions_by_package(existing_sources)?;
        let project_exact_versions = exact_project_version_counts_by_package(rows, package_names);
        let source_identity_versions = source_identity_versions_by_module(
            rows,
            existing_sources,
            package_names,
            &available_versions,
            &project_exact_versions,
        )?;
        Ok(Self {
            available_versions,
            project_exact_versions,
            source_identity_versions,
        })
    }

    fn materialization_hints(
        &self,
        rows: &InputRows,
        package_names: &BTreeSet<String>,
    ) -> BTreeSet<(String, String)> {
        rows.modules
            .iter()
            .filter(|module| module.kind == ModuleKind::Package)
            .filter_map(|module| {
                let package_name = scoped_package_name(module, package_names)?;
                let package_version = module.package_version.as_deref().map(str::trim);
                let Some(package_version) = package_version.filter(|version| !version.is_empty())
                else {
                    let resolved = self
                        .best_project_version_candidate(package_name, "latest", None)
                        .or_else(|| {
                            self.available_versions
                                .get(package_name)
                                .and_then(|versions| {
                                    best_matching_package_version_by_binary_search(
                                        "latest", versions,
                                    )
                                })
                        })?;
                    return (!self.package_source_versions_contain(package_name, &resolved))
                        .then(|| (package_name.to_string(), resolved.to_string()));
                };
                if is_exact_package_version_hint(package_version) {
                    return (!package_source_versions_contain(
                        &self.available_versions,
                        package_name,
                        package_version,
                    ))
                    .then(|| (package_name.to_string(), package_version.to_string()));
                }
                let resolved = self
                    .best_project_version_candidate(package_name, package_version, None)
                    .or_else(|| {
                        self.available_versions
                            .get(package_name)
                            .and_then(|versions| {
                                best_matching_package_version_by_binary_search(
                                    package_version,
                                    versions,
                                )
                            })
                    })?;
                (!self.package_source_versions_contain(package_name, &resolved))
                    .then(|| (package_name.to_string(), resolved.to_string()))
            })
            .collect()
    }

    fn stale_cache_materialization_hints(
        &self,
        rows: &InputRows,
        package_names: &BTreeSet<String>,
        stale_cache_versions: &BTreeSet<(String, String)>,
    ) -> BTreeSet<(String, String)> {
        if stale_cache_versions.is_empty() {
            return BTreeSet::new();
        }
        let project_resolved_versions = self.resolved_project_versions(rows, package_names);
        stale_cache_versions
            .iter()
            .filter_map(|(package_name, stale_version)| {
                let versions = self.available_versions.get(package_name)?;
                let resolved = if is_exact_package_version_hint(stale_version) {
                    Version::parse(stale_version).ok()
                } else {
                    self.best_project_version_candidate(package_name, stale_version, Some(versions))
                        .or_else(|| {
                            best_matching_package_version_by_binary_search(stale_version, versions)
                        })
                }?;
                project_resolved_versions
                    .get(package_name)
                    .is_some_and(|needed| needed.contains(&resolved))
                    .then(|| (package_name.clone(), resolved.to_string()))
            })
            .collect()
    }

    fn network_resolution_hints(
        &self,
        rows: &InputRows,
        package_names: &BTreeSet<String>,
    ) -> BTreeSet<(String, String)> {
        rows.modules
            .iter()
            .filter(|module| module.kind == ModuleKind::Package)
            .filter_map(|module| {
                let package_name = scoped_package_name(module, package_names)?;
                let requested_version = module
                    .package_version
                    .as_deref()
                    .map(str::trim)
                    .filter(|version| !version.is_empty());
                let Some(package_version) = requested_version else {
                    let has_project_candidate = self
                        .best_project_version_candidate(package_name, "latest", None)
                        .is_some();
                    let has_available_candidate = self
                        .available_versions
                        .get(package_name)
                        .is_some_and(|versions| {
                            best_matching_package_version_by_binary_search("latest", versions)
                                .is_some()
                        });
                    return (!has_project_candidate && !has_available_candidate)
                        .then(|| (package_name.to_string(), "latest".to_string()));
                };
                if is_exact_package_version_hint(package_version) {
                    return None;
                }
                let has_project_candidate = self
                    .best_project_version_candidate(package_name, package_version, None)
                    .is_some();
                let has_available_candidate = self
                    .available_versions
                    .get(package_name)
                    .is_some_and(|versions| {
                        best_matching_package_version_by_binary_search(package_version, versions)
                            .is_some()
                    });
                (!has_project_candidate && !has_available_candidate)
                    .then(|| (package_name.to_string(), package_version.to_string()))
            })
            .collect()
    }

    fn resolved_project_versions(
        &self,
        rows: &InputRows,
        package_names: &BTreeSet<String>,
    ) -> BTreeMap<String, BTreeSet<Version>> {
        let mut resolved = BTreeMap::<String, BTreeSet<Version>>::new();
        for module in &rows.modules {
            if module.kind != ModuleKind::Package {
                continue;
            }
            let Some(package_name) = scoped_package_name(module, package_names) else {
                continue;
            };
            let Some(versions) = self.available_versions.get(package_name) else {
                continue;
            };
            let requested_version = module
                .package_version
                .as_deref()
                .map(str::trim)
                .filter(|version| !version.is_empty())
                .unwrap_or("latest");
            let selected = if is_exact_package_version_hint(requested_version) {
                Version::parse(requested_version)
                    .ok()
                    .filter(|version| versions.contains(version))
            } else {
                self.best_project_version_candidate(package_name, requested_version, Some(versions))
                    .or_else(|| {
                        best_matching_package_version_by_binary_search(requested_version, versions)
                    })
            };
            if let Some(selected) = selected {
                resolved
                    .entry(package_name.to_string())
                    .or_default()
                    .insert(selected);
            }
        }
        resolved
    }

    fn apply_to_rows(&self, rows: &mut InputRows, package_names: &BTreeSet<String>) -> usize {
        let mut resolved = 0usize;
        for module in &mut rows.modules {
            if module.kind != ModuleKind::Package {
                continue;
            }
            let Some(package_name) = scoped_package_name(module, package_names).map(str::to_string)
            else {
                continue;
            };
            let requested_version = module
                .package_version
                .as_deref()
                .map(str::trim)
                .filter(|version| !version.is_empty());
            let Some(versions) = self.available_versions.get(package_name.as_str()) else {
                continue;
            };
            let source_identity_version = self.source_identity_versions.get(&module.id).cloned();
            let selected = match requested_version {
                Some(package_version) if is_exact_package_version_hint(package_version) => None,
                Some(package_version) => self
                    .best_project_version_candidate(
                        package_name.as_str(),
                        package_version,
                        Some(versions),
                    )
                    .or_else(|| {
                        source_identity_version
                            .filter(|version| version_hint_matches(package_version, version))
                    })
                    .or_else(|| {
                        best_matching_package_version_by_binary_search(package_version, versions)
                    }),
                None => source_identity_version
                    .or_else(|| {
                        self.best_project_version_candidate(
                            package_name.as_str(),
                            "latest",
                            Some(versions),
                        )
                    })
                    .or_else(|| best_matching_package_version_by_binary_search("latest", versions)),
            };
            if let Some(selected) = selected {
                module.package_version = Some(selected.to_string());
                resolved += 1;
            }
        }
        resolved
    }

    fn best_project_version_candidate(
        &self,
        package_name: &str,
        requested_version: &str,
        available_versions: Option<&BTreeSet<Version>>,
    ) -> Option<Version> {
        best_project_version_candidate(
            package_name,
            requested_version,
            &self.project_exact_versions,
            available_versions,
        )
    }

    fn package_source_versions_contain(
        &self,
        package_name: &str,
        package_version: &Version,
    ) -> bool {
        self.available_versions
            .get(package_name)
            .is_some_and(|versions| versions.contains(package_version))
    }
}

fn scoped_package_name<'a>(
    module: &'a ModuleInput,
    package_names: &BTreeSet<String>,
) -> Option<&'a str> {
    let package_name = module.package_name.as_deref()?.trim();
    (!package_name.is_empty()
        && is_valid_package_name(package_name)
        && (package_names.is_empty() || package_names.contains(package_name)))
    .then_some(package_name)
}

pub(crate) fn materialize_package_sources_from_hints(
    rows: &InputRows,
    package_names: &BTreeSet<String>,
    existing_sources: &[PackageSource],
    stale_cache_versions: &BTreeSet<(String, String)>,
    reference_versions: &BTreeMap<String, String>,
) -> Result<Vec<PackageSource>, MatchPackagesError> {
    let plan = PackageVersionResolutionPlan::build(rows, package_names, existing_sources)?;
    let mut hints = plan.materialization_hints(rows, package_names);
    hints.extend(plan.stale_cache_materialization_hints(rows, package_names, stale_cache_versions));
    let mut network_hints = plan.network_resolution_hints(rows, package_names);
    // Inlined libraries (zod, react, …) leave no bundle module, so the
    // module-driven hints above never name them. Their version lives only in the
    // reference `package.json`; resolve those too so the island anchoring pass
    // has a corpus to match against.
    network_hints.extend(reference_no_module_network_hints(
        rows,
        existing_sources,
        reference_versions,
    ));
    for (package_name, requested_version) in network_hints {
        match resolve_package_version_hint_from_network(
            package_name.as_str(),
            requested_version.as_str(),
        ) {
            Ok(Some(resolved_version)) => {
                hints.insert((package_name, resolved_version));
            }
            Ok(None) => {
                eprintln!(
                    "skipping package source materialization for {package_name}@{requested_version}: no matching npm version\n  \
                     next: the version hint does not exist on npm. Run `npm view {package_name} versions` for the real list, \
                     have an Agent pick the version that matches the inlined source (VERSION constant / API shape / peer-dep \
                     coherence with already-anchored packages), then re-propose it via \
                     `island-package-candidates --accept {package_name} --version <valid> --apply` and re-run match-packages."
                );
            }
            Err(error) => {
                eprintln!(
                    "skipping package source materialization for {package_name}@{requested_version}: {error}{}",
                    next_step_hint(package_name.as_str(), &error.to_string())
                );
            }
        }
    }
    if hints.is_empty() {
        return Ok(Vec::new());
    }

    let mut sources = Vec::new();
    for (package_name, package_version) in hints {
        if let Err(error) = materialize_one_package_source(
            package_name.as_str(),
            package_version.as_str(),
            &mut sources,
        ) {
            eprintln!(
                "skipping package source materialization for {package_name}@{package_version}: {error}{}",
                next_step_hint(package_name.as_str(), &error.to_string())
            );
        }
    }
    Ok(sources)
}

/// Append an actionable next-step hint to a materialization-skip warning, chosen
/// by the failure shape so an operator/Agent knows how to proceed instead of just
/// seeing a dead end.
fn next_step_hint(package_name: &str, error: &str) -> String {
    if error.contains("404") || error.contains("Connection Failed") {
        // Private / unpublished registry name (e.g. @ant/*, workspace:* deps).
        format!(
            "\n  next: '{package_name}' is not on the public registry (private/unpublished/workspace dep) — \
             expected. It cannot be externalized; if it is a recognized inlined library, relocate its island \
             cluster under vendor/ via `cluster-names --accept <fingerprint>=vendor/<name> --apply`."
        )
    } else if error.contains("did not contain a package") || error.contains("extract") {
        // Tarball shape the extractor cannot use (often @types/* type-only packages).
        format!(
            "\n  next: '{package_name}' tarball could not be extracted (often a types-only `@types/*` package \
             with no runtime source) — harmless; it is not a runtime dependency to externalize."
        )
    } else {
        format!(
            "\n  next: registry/network error for '{package_name}'. Retry; if it persists, confirm the version \
             exists (`npm view {package_name} versions`) and re-propose via `island-package-candidates`."
        )
    }
}

/// Network-resolution hints for reference-manifest packages that have NO bundle
/// module and no already-loaded source — i.e. libraries the bundler inlined into
/// the eager island. Each yields `(name, version_specifier)` to resolve and
/// download; a name already covered by a module hint or an existing source is
/// skipped so this never duplicates the module-driven path.
fn reference_no_module_network_hints(
    rows: &InputRows,
    existing_sources: &[PackageSource],
    reference_versions: &BTreeMap<String, String>,
) -> BTreeSet<(String, String)> {
    let module_package_names: BTreeSet<&str> = rows
        .modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
        .filter_map(|module| module.package_name.as_deref())
        .collect();
    let sourced_names: BTreeSet<&str> = existing_sources
        .iter()
        .map(|source| source.package_name.as_str())
        .collect();
    reference_versions
        .iter()
        .filter(|(name, version)| {
            is_valid_package_name(name)
                && !version.trim().is_empty()
                && !module_package_names.contains(name.as_str())
                && !sourced_names.contains(name.as_str())
        })
        .map(|(name, version)| (name.clone(), version.trim().to_string()))
        .collect()
}

#[cfg(test)]
pub(crate) fn stale_cache_version_hints_for_materialization(
    rows: &InputRows,
    package_names: &BTreeSet<String>,
    existing_sources: &[PackageSource],
    stale_cache_versions: &BTreeSet<(String, String)>,
) -> BTreeSet<(String, String)> {
    PackageVersionResolutionPlan::build(rows, package_names, existing_sources)
        .expect("package version resolution plan should build")
        .stale_cache_materialization_hints(rows, package_names, stale_cache_versions)
}

fn materialize_one_package_source(
    package_name: &str,
    package_version: &str,
    sources: &mut Vec<PackageSource>,
) -> Result<(), MatchPackagesError> {
    let packument = pkg_sources::registry::fetch_packument(package_name)?;
    let Some(dist) = packument.versions.get(package_version) else {
        return Err(MatchPackagesError::MaterializePackageSource {
            package_name: package_name.to_string(),
            package_version: package_version.to_string(),
            message: "version not present in registry packument".to_string(),
        });
    };
    let root = pkg_sources::cache::cache_root()?;
    let host = pkg_sources::cache::registry_host(&pkg_sources::registry::registry_base_url());
    let package_dir = pkg_sources::cache::ensure_package_source(
        &root,
        &host,
        package_name,
        package_version,
        dist,
        pkg_sources::registry::http_get,
    )?;
    let Some(metadata) = pkg_sources::local_package_metadata(package_dir.as_path())? else {
        return Err(MatchPackagesError::MaterializePackageSource {
            package_name: package_name.to_string(),
            package_version: package_version.to_string(),
            message: "cached package had no package.json".to_string(),
        });
    };
    if metadata.name != package_name || metadata.version != package_version {
        return Err(MatchPackagesError::MaterializePackageSource {
            package_name: package_name.to_string(),
            package_version: package_version.to_string(),
            message: format!(
                "cached package identity {}@{} did not match",
                metadata.name, metadata.version
            ),
        });
    }
    pkg_sources::collect_local_package_sources(package_dir.as_path(), &metadata, sources)
}

#[cfg(test)]
pub(crate) fn package_version_hints_for_materialization(
    rows: &InputRows,
    package_names: &BTreeSet<String>,
    existing_sources: &[PackageSource],
) -> BTreeSet<(String, String)> {
    PackageVersionResolutionPlan::build(rows, package_names, existing_sources)
        .expect("package version resolution plan should build")
        .materialization_hints(rows, package_names)
}

#[cfg(test)]
pub(crate) fn network_package_version_resolution_hints(
    rows: &InputRows,
    package_names: &BTreeSet<String>,
    existing_sources: &[PackageSource],
) -> BTreeSet<(String, String)> {
    PackageVersionResolutionPlan::build(rows, package_names, existing_sources)
        .expect("package version resolution plan should build")
        .network_resolution_hints(rows, package_names)
}

pub(crate) fn resolve_package_version_hints_to_available_sources(
    rows: &mut InputRows,
    package_sources: &[PackageSource],
    package_names: &BTreeSet<String>,
) -> Result<usize, MatchPackagesError> {
    let plan = PackageVersionResolutionPlan::build(rows, package_names, package_sources)?;
    Ok(plan.apply_to_rows(rows, package_names))
}

#[cfg(test)]
mod reference_no_module_hint_tests {
    use reverts_input::{ModuleInput, ProjectInput};
    use reverts_ir::ModuleId;

    use super::*;

    #[test]
    fn emits_hints_only_for_inlined_libraries_without_a_module_or_source() {
        let mut rows = InputRows::new(ProjectInput::new(1, "p"));
        // node-pty has a bundle module → handled by the module-driven path.
        rows.modules.push(ModuleInput::package(
            ModuleId(1),
            "node_modules/node-pty/index.js",
            "node_modules/node-pty/index.js",
            "node-pty",
            Some("1.0.0".to_string()),
        ));
        // semver already has a loaded source → no need to re-fetch.
        let existing = vec![PackageSource::external(
            "semver",
            "7.6.0",
            "semver",
            "semver/index.js",
            "1;",
        )];
        let reference_versions = BTreeMap::from([
            ("zod".to_string(), "^3.25.64".to_string()),
            ("node-pty".to_string(), "1.0.0".to_string()),
            ("semver".to_string(), "^7.0.0".to_string()),
            ("blank".to_string(), "  ".to_string()),
        ]);

        let hints = reference_no_module_network_hints(&rows, &existing, &reference_versions);

        // Only zod: node-pty has a module, semver has a source, blank has no version.
        assert_eq!(
            hints,
            BTreeSet::from([("zod".to_string(), "^3.25.64".to_string())])
        );
    }
}

fn source_identity_versions_by_module(
    rows: &InputRows,
    package_sources: &[PackageSource],
    package_names: &BTreeSet<String>,
    available_versions: &BTreeMap<String, BTreeSet<Version>>,
    project_exact_versions: &BTreeMap<String, BTreeMap<Version, usize>>,
) -> Result<BTreeMap<ModuleId, Version>, MatchPackagesError> {
    let index = PackageSourceIdentityIndex::build(package_sources)?;
    if index.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut versions_by_module = BTreeMap::<ModuleId, Version>::new();
    for module in rows
        .modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
    {
        let Some(package_name) = module.package_name.as_deref().map(str::trim) else {
            continue;
        };
        if package_name.is_empty()
            || !is_valid_package_name(package_name)
            || (!package_names.is_empty() && !package_names.contains(package_name))
            || !index.contains_package(package_name)
        {
            continue;
        }
        let Some(versions) = available_versions.get(package_name) else {
            continue;
        };
        if !module_needs_source_identity_version(
            module,
            package_name,
            versions,
            project_exact_versions,
        ) {
            continue;
        }
        let Some(slice) = rows.module_source_slice(module.id) else {
            continue;
        };
        if package_module_source_quality(module, slice.source_file_path, slice.source)
            == PackageModuleSourceQuality::Invalid
        {
            continue;
        }
        if let Some(version) = index.best_version_for_source(
            package_name,
            module.package_version.as_deref().map(str::trim),
            slice.source_file_path,
            slice.source,
        )? {
            versions_by_module.insert(module.id, version);
        }
    }
    Ok(versions_by_module)
}

fn module_needs_source_identity_version(
    module: &ModuleInput,
    package_name: &str,
    versions: &BTreeSet<Version>,
    project_exact_versions: &BTreeMap<String, BTreeMap<Version, usize>>,
) -> bool {
    let requested_version = module
        .package_version
        .as_deref()
        .map(str::trim)
        .filter(|version| !version.is_empty());
    match requested_version {
        Some(package_version) if is_exact_package_version_hint(package_version) => false,
        Some(package_version) => best_project_version_candidate(
            package_name,
            package_version,
            project_exact_versions,
            Some(versions),
        )
        .is_none(),
        None => true,
    }
}

#[derive(Debug, Default)]
struct PackageSourceIdentityIndex {
    versions_by_package_hash: BTreeMap<String, BTreeMap<String, BTreeMap<Version, usize>>>,
}

impl PackageSourceIdentityIndex {
    fn build(package_sources: &[PackageSource]) -> Result<Self, MatchPackagesError> {
        let mut versions_by_package_hash =
            BTreeMap::<String, BTreeMap<String, BTreeMap<Version, usize>>>::new();
        for package_source in package_sources {
            let package_version = package_source_version(package_source)?;
            let normalized = normalize_source_for_pipeline(
                package_source.source.as_str(),
                Some(Path::new(package_source.source_path.as_str())),
            )
            .map_err(|source| {
                package_source_normalization_error(
                    package_source.package_name.as_str(),
                    Some(package_source.package_version.as_str()),
                    package_source.source_path.as_str(),
                    source,
                )
            })?;
            let normalized_source_hash = stable_hash(normalized.as_bytes());
            *versions_by_package_hash
                .entry(package_source.package_name.clone())
                .or_default()
                .entry(normalized_source_hash)
                .or_default()
                .entry(package_version)
                .or_default() += 1;
        }
        Ok(Self {
            versions_by_package_hash,
        })
    }

    fn is_empty(&self) -> bool {
        self.versions_by_package_hash.is_empty()
    }

    fn contains_package(&self, package_name: &str) -> bool {
        self.versions_by_package_hash.contains_key(package_name)
    }

    fn best_version_for_source(
        &self,
        package_name: &str,
        package_version: Option<&str>,
        source_file_path: &str,
        source: &str,
    ) -> Result<Option<Version>, MatchPackagesError> {
        let normalized =
            match normalize_source_for_pipeline(source, Some(Path::new(source_file_path))) {
                Ok(normalized) => normalized,
                Err(source) => {
                    return Err(package_source_normalization_error(
                        package_name,
                        package_version,
                        source_file_path,
                        source,
                    ));
                }
            };
        let normalized_source_hash = stable_hash(normalized.as_bytes());
        let Some(versions_by_hash) = self.versions_by_package_hash.get(package_name) else {
            return Ok(None);
        };
        let Some(versions) = versions_by_hash.get(normalized_source_hash.as_str()) else {
            return Ok(None);
        };
        Ok(versions
            .iter()
            .max_by(|left, right| left.1.cmp(right.1).then_with(|| left.0.cmp(right.0)))
            .map(|(version, _count)| version.clone()))
    }
}

fn package_source_normalization_error(
    package_name: &str,
    package_version: Option<&str>,
    source_path: &str,
    source: reverts_js::JsError,
) -> MatchPackagesError {
    MatchPackagesError::NormalizePackageSource {
        package_name: package_name.to_string(),
        package_version: package_version.map(str::to_string),
        source_path: source_path.to_string(),
        source,
    }
}

fn exact_package_source_versions_by_package(
    package_sources: &[PackageSource],
) -> Result<BTreeMap<String, BTreeSet<Version>>, MatchPackagesError> {
    let mut versions = BTreeMap::<String, BTreeSet<Version>>::new();
    for source in package_sources {
        let version = package_source_version(source)?;
        versions
            .entry(source.package_name.clone())
            .or_default()
            .insert(version);
    }
    Ok(versions)
}

fn package_source_version(source: &PackageSource) -> Result<Version, MatchPackagesError> {
    Version::parse(source.package_version.as_str()).map_err(|_| {
        MatchPackagesError::InvalidPackageSourceVersion {
            package_name: source.package_name.clone(),
            package_version: source.package_version.clone(),
            source_path: source.source_path.clone(),
        }
    })
}

fn exact_project_version_counts_by_package(
    rows: &InputRows,
    package_names: &BTreeSet<String>,
) -> BTreeMap<String, BTreeMap<Version, usize>> {
    let mut versions = BTreeMap::<String, BTreeMap<Version, usize>>::new();
    for module in &rows.modules {
        if module.kind != ModuleKind::Package {
            continue;
        }
        let Some(package_name) = module.package_name.as_deref().map(str::trim) else {
            continue;
        };
        if package_name.is_empty()
            || !is_valid_package_name(package_name)
            || (!package_names.is_empty() && !package_names.contains(package_name))
        {
            continue;
        }
        let Some(package_version) = module.package_version.as_deref().map(str::trim) else {
            continue;
        };
        let Ok(version) = Version::parse(package_version) else {
            continue;
        };
        *versions
            .entry(package_name.to_string())
            .or_default()
            .entry(version)
            .or_default() += 1;
    }
    versions
}

fn package_source_versions_contain(
    versions_by_package: &BTreeMap<String, BTreeSet<Version>>,
    package_name: &str,
    package_version: &str,
) -> bool {
    let Ok(version) = Version::parse(package_version) else {
        return false;
    };
    versions_by_package
        .get(package_name)
        .is_some_and(|versions| versions.contains(&version))
}

fn best_project_version_candidate(
    package_name: &str,
    requested_version: &str,
    project_exact_versions: &BTreeMap<String, BTreeMap<Version, usize>>,
    available_versions: Option<&BTreeSet<Version>>,
) -> Option<Version> {
    let versions = project_exact_versions.get(package_name)?;
    let mut candidates = versions
        .iter()
        .filter(|(version, _count)| {
            available_versions.is_none_or(|available| available.contains(version))
                && version_hint_matches(requested_version, version)
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.1
            .cmp(right.1)
            .then_with(|| left.0.cmp(right.0))
            .reverse()
    });
    candidates
        .into_iter()
        .map(|(version, _count)| version.clone())
        .next()
}

fn version_hint_matches(requested_version: &str, version: &Version) -> bool {
    let requested_version = requested_version.trim();
    if requested_version.eq_ignore_ascii_case("latest") {
        return true;
    }
    VersionReq::parse(requested_version).is_ok_and(|requirement| requirement.matches(version))
}

pub(crate) fn best_matching_package_version_by_binary_search(
    requested_version: &str,
    versions: &BTreeSet<Version>,
) -> Option<Version> {
    if versions.is_empty() {
        return None;
    }
    if requested_version.trim().eq_ignore_ascii_case("latest") {
        return versions.iter().next_back().cloned();
    }
    let requirement = VersionReq::parse(requested_version.trim()).ok()?;
    let sorted_versions = versions.iter().cloned().collect::<Vec<_>>();
    let search_end = version_req_upper_bound(&requirement)
        .map(|(upper_bound, inclusive)| {
            if inclusive {
                sorted_versions.partition_point(|version| version <= &upper_bound)
            } else {
                sorted_versions.partition_point(|version| version < &upper_bound)
            }
        })
        .unwrap_or(sorted_versions.len());
    sorted_versions[..search_end]
        .iter()
        .rev()
        .find(|version| requirement.matches(version))
        .cloned()
}

fn version_req_upper_bound(requirement: &VersionReq) -> Option<(Version, bool)> {
    requirement
        .comparators
        .iter()
        .filter_map(comparator_upper_bound)
        .min_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)))
}

fn comparator_upper_bound(comparator: &Comparator) -> Option<(Version, bool)> {
    match comparator.op {
        Op::Exact | Op::Wildcard => partial_exact_upper_bound(comparator),
        Op::Tilde => {
            let minor = comparator.minor?;
            Some((Version::new(comparator.major, minor + 1, 0), false))
        }
        Op::Caret => Some(caret_upper_bound(comparator)),
        Op::Less => Some((partial_version_floor(comparator), false)),
        Op::LessEq => {
            if comparator.patch.is_some() {
                Some((partial_version_floor(comparator), true))
            } else {
                partial_exact_upper_bound(comparator)
            }
        }
        Op::Greater | Op::GreaterEq => None,
        _ => None,
    }
}

fn partial_exact_upper_bound(comparator: &Comparator) -> Option<(Version, bool)> {
    match (comparator.minor, comparator.patch) {
        (Some(minor), Some(patch)) if comparator.op == Op::Exact => Some((
            Version {
                major: comparator.major,
                minor,
                patch,
                pre: comparator.pre.clone(),
                build: BuildMetadata::EMPTY,
            },
            true,
        )),
        (Some(minor), _) => Some((Version::new(comparator.major, minor + 1, 0), false)),
        (None, _) => Some((Version::new(comparator.major + 1, 0, 0), false)),
    }
}

fn caret_upper_bound(comparator: &Comparator) -> (Version, bool) {
    let minor = comparator.minor.unwrap_or(0);
    let patch = comparator.patch.unwrap_or(0);
    if comparator.major > 0 {
        (Version::new(comparator.major + 1, 0, 0), false)
    } else if minor > 0 {
        (Version::new(0, minor + 1, 0), false)
    } else if comparator.patch.is_some() {
        (Version::new(0, 0, patch + 1), false)
    } else if comparator.minor.is_some() {
        (Version::new(0, 1, 0), false)
    } else {
        (Version::new(1, 0, 0), false)
    }
}

fn partial_version_floor(comparator: &Comparator) -> Version {
    Version {
        major: comparator.major,
        minor: comparator.minor.unwrap_or(0),
        patch: comparator.patch.unwrap_or(0),
        pre: comparator.pre.clone(),
        build: BuildMetadata::EMPTY,
    }
}

fn resolve_package_version_hint_from_network(
    package_name: &str,
    requested_version: &str,
) -> Result<Option<String>, MatchPackagesError> {
    let packument = pkg_sources::registry::fetch_packument(package_name)?;
    let versions = packument.available_versions();
    Ok(
        resolve_package_version_hint_from_versions(requested_version, &versions)
            .map(|version| version.to_string()),
    )
}

pub(crate) fn resolve_package_version_hint_from_versions(
    requested_version: &str,
    versions: &BTreeSet<Version>,
) -> Option<Version> {
    best_matching_package_version_by_binary_search(requested_version, versions)
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackageVersionResolutionEvidence {
    pub(crate) requested_version: Option<String>,
    pub(crate) resolved_version: String,
    pub(crate) reason: &'static str,
}

pub(crate) fn package_versions_by_module(rows: &InputRows) -> BTreeMap<ModuleId, Option<String>> {
    rows.modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
        .map(|module| {
            (
                module.id,
                module
                    .package_version
                    .as_deref()
                    .map(str::trim)
                    .filter(|version| !version.is_empty())
                    .map(ToOwned::to_owned),
            )
        })
        .collect()
}

pub(crate) fn package_version_resolution_evidence(
    before: &BTreeMap<ModuleId, Option<String>>,
    rows: &InputRows,
) -> BTreeMap<ModuleId, PackageVersionResolutionEvidence> {
    rows.modules
        .iter()
        .filter(|module| module.kind == ModuleKind::Package)
        .filter_map(|module| {
            let requested_version = before.get(&module.id).cloned().flatten();
            let resolved_version = module
                .package_version
                .as_deref()
                .map(str::trim)
                .filter(|version| !version.is_empty())?;
            if requested_version.as_deref() == Some(resolved_version) {
                return None;
            }
            Some((
                module.id,
                PackageVersionResolutionEvidence {
                    reason: package_version_resolution_reason(
                        requested_version.as_deref(),
                        resolved_version,
                    ),
                    requested_version,
                    resolved_version: resolved_version.to_string(),
                },
            ))
        })
        .collect()
}

fn package_version_resolution_reason(
    requested_version: Option<&str>,
    resolved_version: &str,
) -> &'static str {
    let Some(requested_version) = requested_version else {
        return "missing_hint_resolved_to_available_source";
    };
    if Version::parse(resolved_version)
        .ok()
        .is_some_and(|version| version_hint_matches(requested_version, &version))
    {
        "range_resolved_to_available_source"
    } else {
        "non_matching_version_resolution"
    }
}
