//! Read-only input enrichment for generation-time package source elimination.
//!
//! Generation never reinterprets `package_externalization_hints`: those proofs
//! must be applied by `match-packages` and persisted as package attributions
//! before this loader runs. The only enrichment here is deterministic detection
//! of package modules whose cached package manifest proves a public external
//! specifier.

use std::collections::BTreeMap;
use std::path::Path;

use reverts_input::sqlite::{SqliteInputError, load_project_bundle_from_connection};
use reverts_input::{
    InputBundle, ModuleInput, PackageAttributionInput, PackageAttributionStatus,
    PackageEmissionMode,
};
use reverts_ir::{ModuleKind, is_valid_package_name, split_bare_specifier};
use reverts_package::{ExternalImportProofPath, package_source_path};
use rusqlite::{Connection, OpenFlags};

use crate::persistence::source_cache::{
    MaterializedPackageManifests, load_package_source_cache_view,
};

pub(crate) fn load_project_bundle_with_package_externalization(
    path: impl AsRef<Path>,
    project_id: u32,
) -> Result<InputBundle, SqliteInputError> {
    let path = path.as_ref();
    let connection =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|source| {
            SqliteInputError::OpenDatabase {
                path: path.to_path_buf(),
                source,
            }
        })?;
    let mut bundle = load_project_bundle_from_connection(&connection, project_id)?;
    let materialized_packages = materialized_package_manifests(&connection)?;
    promote_detected_package_modules(&mut bundle, &materialized_packages);
    Ok(bundle)
}

fn materialized_package_manifests(
    connection: &Connection,
) -> Result<MaterializedPackageManifests, SqliteInputError> {
    Ok(load_package_source_cache_view(connection)?)
}

/// Load the cached per-package `package.json` manifests from `path`. The
/// final `package.json` coherence prune needs the dependency graph these
/// carry, which the generation pipeline (working only from the `InputBundle`)
/// cannot see.
pub(crate) fn load_materialized_package_manifests(
    path: impl AsRef<Path>,
) -> Result<MaterializedPackageManifests, SqliteInputError> {
    let path = path.as_ref();
    let connection =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|source| {
            SqliteInputError::OpenDatabase {
                path: path.to_path_buf(),
                source,
            }
        })?;
    materialized_package_manifests(&connection)
}

pub(crate) fn promote_detected_package_modules(
    bundle: &mut InputBundle,
    materialized_packages: &MaterializedPackageManifests,
) -> usize {
    let modules_by_id = bundle
        .modules
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let package_versions = detected_package_versions_by_name(bundle);
    let policy = DetectedPackageExternalizationPolicy {
        materialized_packages,
        package_versions: &package_versions,
    };
    let mut promoted = 0usize;
    for attribution in &mut bundle.package_attributions {
        if detected_package_attribution_is_already_externalized(attribution) {
            continue;
        }
        let Some(module) = modules_by_id.get(&attribution.module_id).copied() else {
            continue;
        };
        let Some(decision) = policy.decide(module, attribution) else {
            continue;
        };
        apply_detected_package_externalization_decision(attribution, decision);
        promoted += 1;
    }
    promoted
}

struct DetectedPackageExternalizationPolicy<'a> {
    materialized_packages: &'a MaterializedPackageManifests,
    package_versions: &'a BTreeMap<String, String>,
}

struct DetectedPackageExternalizationDecision {
    package_version: String,
    export_specifier: String,
    subpath: Option<String>,
    resolved_file: String,
}

impl DetectedPackageExternalizationPolicy<'_> {
    fn decide(
        &self,
        module: &ModuleInput,
        attribution: &PackageAttributionInput,
    ) -> Option<DetectedPackageExternalizationDecision> {
        if module.kind != ModuleKind::Package {
            return None;
        }
        let module_package_name = module.package_name.as_deref().map(str::trim)?;
        if module_package_name.is_empty() || module_package_name != attribution.package_name {
            return None;
        }
        let package_version = module
            .package_version
            .as_deref()
            .map(str::trim)
            .filter(|version| !version.is_empty())
            .or_else(|| {
                self.package_versions
                    .get(module_package_name)
                    .map(String::as_str)
            })?;
        // Existence gate: only externalize a package version that was actually
        // materialized (fetched). Promoting an unresolved package/version — e.g.
        // a 404 version baked into the input bundle — would emit a bare
        // import and an uninstallable `package.json` dependency. Keep it
        // vendored instead.
        let export_specifier = detected_package_export_specifier(
            module_package_name,
            module.semantic_path.as_str(),
            module.original_name.as_str(),
        )?;
        // Publicness gate: only externalize when the detected specifier is a real
        // public export of the package (per its `package.json` exports map).
        // Otherwise the bare import (e.g. `axios/exports`) crashes at load with
        // ERR_PACKAGE_PATH_NOT_EXPORTED. Non-public detections stay vendored.
        if !self.materialized_packages.package_specifier_is_public(
            module_package_name,
            package_version,
            export_specifier.as_str(),
        ) {
            return None;
        }
        // Specifier-resolution gate: a no-`exports` (CJS) deep import must name
        // the exact on-disk file — Node ESM never auto-appends extensions. Append
        // the real extension from the cache (`lodash/_baseUnary` ->
        // `lodash/_baseUnary.js`); if no real file backs it, it is not safely
        // externalizable, so keep it vendored.
        let export_specifier = self.materialized_packages.resolve_deep_import_specifier(
            module_package_name,
            package_version,
            export_specifier.as_str(),
        )?;

        Some(DetectedPackageExternalizationDecision {
            package_version: package_version.to_string(),
            subpath: detected_package_subpath(export_specifier.as_str()),
            resolved_file: ExternalImportProofPath::semantic_path(
                package_source_path(
                    module_package_name,
                    package_version,
                    module.semantic_path.trim().trim_matches('/'),
                )
                .as_str(),
            ),
            export_specifier,
        })
    }
}

fn detected_package_attribution_is_already_externalized(
    attribution: &PackageAttributionInput,
) -> bool {
    attribution.status == PackageAttributionStatus::Accepted
        && attribution.emission_mode == PackageEmissionMode::ExternalImport
        && attribution.export_specifier.is_some()
        && attribution.package_version.is_some()
}

fn apply_detected_package_externalization_decision(
    attribution: &mut PackageAttributionInput,
    decision: DetectedPackageExternalizationDecision,
) {
    attribution.package_version = Some(decision.package_version);
    attribution.subpath = decision.subpath;
    attribution.resolved_file = Some(decision.resolved_file);
    attribution.export_specifier = Some(decision.export_specifier);
    attribution.emission_mode = PackageEmissionMode::ExternalImport;
    attribution.status = PackageAttributionStatus::Accepted;
    attribution.rejection_reason = None;
}

fn detected_package_versions_by_name(bundle: &InputBundle) -> BTreeMap<String, String> {
    let mut counts = BTreeMap::<String, BTreeMap<String, usize>>::new();
    for module in &bundle.modules {
        if module.kind != ModuleKind::Package {
            continue;
        }
        let Some(package_name) = module.package_name.as_deref().map(str::trim) else {
            continue;
        };
        let Some(package_version) = module.package_version.as_deref().map(str::trim) else {
            continue;
        };
        if package_name.is_empty() || package_version.is_empty() {
            continue;
        }
        *counts
            .entry(package_name.to_string())
            .or_default()
            .entry(package_version.to_string())
            .or_default() += 1;
    }
    counts
        .into_iter()
        .filter_map(|(package_name, versions)| {
            versions
                .into_iter()
                .max_by(|(left_version, left_count), (right_version, right_count)| {
                    left_count
                        .cmp(right_count)
                        .then_with(|| left_version.cmp(right_version))
                })
                .map(|(version, _)| (package_name, version))
        })
        .collect()
}

fn detected_package_export_specifier(
    package_name: &str,
    semantic_path: &str,
    original_name: &str,
) -> Option<String> {
    let package_name = package_name.trim();
    if !is_valid_package_name(package_name) {
        return None;
    }
    let semantic_path = clean_detected_semantic_path(semantic_path);
    let specifier = if semantic_path.is_empty()
        || semantic_path == "index"
        || normalize_package_path_segment(semantic_path.as_str())
            == normalize_package_path_segment(original_name)
        || detected_semantic_path_is_package_root(package_name, semantic_path.as_str())
    {
        package_name.to_string()
    } else if semantic_path == package_name
        || semantic_path.starts_with(&format!("{package_name}/"))
    {
        semantic_path
    } else if let Some(relative) =
        semantic_path.strip_prefix(format!("{}/", package_alias_segment(package_name)).as_str())
    {
        format!("{package_name}/{relative}")
    } else {
        format!("{package_name}/{semantic_path}")
    };
    specifier_resolves_package(specifier.as_str(), package_name).then_some(specifier)
}

fn clean_detected_semantic_path(semantic_path: &str) -> String {
    let trimmed = semantic_path.trim().trim_matches('/');
    let without_modules = trimmed.strip_prefix("modules/").unwrap_or(trimmed);
    let without_extension = without_modules
        .strip_suffix(".ts")
        .or_else(|| without_modules.strip_suffix(".js"))
        .or_else(|| without_modules.strip_suffix(".mjs"))
        .or_else(|| without_modules.strip_suffix(".cjs"))
        .unwrap_or(without_modules);
    let without_generated_prefix = without_extension
        .split_once('-')
        .filter(|(prefix, _)| prefix.chars().all(|ch| ch.is_ascii_digit()))
        .map(|(_, rest)| rest)
        .unwrap_or(without_extension);
    without_generated_prefix.trim_matches('/').to_string()
}

fn detected_semantic_path_is_package_root(package_name: &str, semantic_path: &str) -> bool {
    let package_alias = package_alias_segment(package_name);
    let normalized_path = normalize_package_path_segment(semantic_path);
    normalized_path == normalize_package_path_segment(package_name)
        || normalized_path == normalize_package_path_segment(package_alias.as_str())
}

fn package_alias_segment(package_name: &str) -> String {
    package_name
        .trim_start_matches('@')
        .rsplit('/')
        .next()
        .unwrap_or(package_name)
        .to_string()
}

fn normalize_package_path_segment(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn specifier_resolves_package(specifier: &str, package_name: &str) -> bool {
    split_bare_specifier(specifier)
        .is_some_and(|(resolved_package, _)| resolved_package == package_name)
}

fn detected_package_subpath(specifier: &str) -> Option<String> {
    split_bare_specifier(specifier).and_then(|(_, subpath)| subpath.map(|value| value.to_string()))
}
