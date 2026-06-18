//! Read-only input enrichment for generation-time package source elimination.
//!
//! `package_attributions` can contain conservative, suggestion-only external
//! rows. When a separately verified `package_externalization_hints` row proves
//! that a dependency-free module body is normalized-source-equivalent to a
//! package cache entry, generation can safely treat that attribution as a strong
//! adapter proof without mutating SQLite.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use reverts_input::sqlite::{SqliteInputError, load_project_bundle_from_connection};
use reverts_input::{
    InputBundle, ModuleDependencyTarget, ModuleInput, PackageAttributionInput,
    PackageAttributionStatus, PackageEmissionMode,
};
use reverts_ir::{ModuleId, ModuleKind, is_valid_package_name, split_bare_specifier};
use reverts_package::{package_specifier_is_public, resolve_package_deep_import_specifier};
use reverts_package_matcher::{
    package_source_exported_members, package_source_normalized_hash,
    package_source_normalized_hashes,
};
use rusqlite::{Connection, OpenFlags};

use crate::{collect_sqlite_rows, sqlite_table_exists, sqlite_table_has_column};

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExternalizationHintProof {
    source_path: String,
    normalized_source_hashes: BTreeSet<String>,
    public_members: BTreeSet<String>,
}

type HintKey = (String, String, String);

pub(crate) fn load_project_bundle_with_verified_externalization_hints(
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
    promote_verified_externalization_hints(&connection, &mut bundle)?;
    Ok(bundle)
}

/// Per-package materialization evidence: the cached root `package.json` source
/// and whether the package ships a root `index.{js,json,node}`. A package is
/// keyed here only if it was actually fetched (so it exists + is installable);
/// the manifest then lets us verify a *specifier* is a public export before
/// externalizing it (so we never emit `import 'pkg/non-public-subpath'`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MaterializedPackageManifest {
    pub(crate) package_json_source: String,
    pub(crate) has_root_index: bool,
    /// Relative file paths the package actually ships (from the cache). Used to
    /// resolve a bare CJS subpath to the real on-disk file — Node ESM never
    /// auto-appends extensions, so a no-`exports` deep import must name the exact
    /// file (`lodash/_baseUnary.js`, not `lodash/_baseUnary`).
    pub(crate) entry_paths: BTreeSet<String>,
}

impl MaterializedPackageManifest {
    pub(crate) fn new(
        package_json_source: impl Into<String>,
        has_root_index: bool,
        entry_paths: BTreeSet<String>,
    ) -> Self {
        Self {
            package_json_source: package_json_source.into(),
            has_root_index,
            entry_paths,
        }
    }
}

pub(crate) type MaterializedPackageKey = (String, String);
pub(crate) type MaterializedPackageManifests =
    BTreeMap<MaterializedPackageKey, MaterializedPackageManifest>;

fn materialized_package_manifests(
    connection: &Connection,
) -> Result<MaterializedPackageManifests, SqliteInputError> {
    MaterializedPackageManifestRepository { connection }.load()
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

struct MaterializedPackageManifestRepository<'a> {
    connection: &'a Connection,
}

impl MaterializedPackageManifestRepository<'_> {
    fn load(&self) -> Result<MaterializedPackageManifests, SqliteInputError> {
        if !sqlite_table_exists(self.connection, "package_source_cache")? {
            return Ok(BTreeMap::new());
        }
        for required in [
            "package_name",
            "package_version",
            "entry_path",
            "source_content",
        ] {
            if !sqlite_table_has_column(self.connection, "package_source_cache", required)? {
                return Ok(BTreeMap::new());
            }
        }
        let mut manifests = BTreeMap::<MaterializedPackageKey, String>::new();
        let mut statement = self.connection.prepare(
            "SELECT package_name, package_version, source_content FROM package_source_cache \
             WHERE entry_path = 'package.json' \
               AND TRIM(COALESCE(package_name, '')) != '' \
               AND TRIM(COALESCE(package_version, '')) != ''",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for (name, version, manifest) in collect_sqlite_rows(rows)? {
            let name = name.trim();
            let version = version.trim();
            if !name.is_empty() && !version.is_empty() {
                manifests.insert((name.to_string(), version.to_string()), manifest);
            }
        }
        let mut entry_paths = BTreeMap::<MaterializedPackageKey, BTreeSet<String>>::new();
        let mut paths_statement = self.connection.prepare(
            "SELECT package_name, package_version, entry_path FROM package_source_cache \
             WHERE TRIM(COALESCE(package_name, '')) != '' \
               AND TRIM(COALESCE(package_version, '')) != '' \
               AND TRIM(COALESCE(entry_path, '')) != ''",
        )?;
        let rows = paths_statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for (name, version, entry_path) in collect_sqlite_rows(rows)? {
            let name = name.trim();
            let version = version.trim();
            if !name.is_empty() && !version.is_empty() {
                entry_paths
                    .entry((name.to_string(), version.to_string()))
                    .or_default()
                    .insert(entry_path.trim().to_string());
            }
        }
        Ok(manifests
            .into_iter()
            .map(|(key, manifest)| {
                let paths = entry_paths.remove(&key).unwrap_or_default();
                let has_index = ["index.js", "index.json", "index.node"]
                    .iter()
                    .any(|index| paths.contains(*index));
                (
                    key,
                    MaterializedPackageManifest::new(manifest, has_index, paths),
                )
            })
            .collect())
    }
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
        let manifest = self
            .materialized_packages
            .get(&(module_package_name.to_string(), package_version.to_string()))?;
        let export_specifier = detected_package_export_specifier(
            module_package_name,
            module.semantic_path.as_str(),
            module.original_name.as_str(),
        )?;
        // Publicness gate: only externalize when the detected specifier is a real
        // public export of the package (per its `package.json` exports map).
        // Otherwise the bare import (e.g. `axios/exports`) crashes at load with
        // ERR_PACKAGE_PATH_NOT_EXPORTED. Non-public detections stay vendored.
        if !package_specifier_is_public(
            manifest.package_json_source.as_str(),
            module_package_name,
            export_specifier.as_str(),
            manifest.has_root_index,
        ) {
            return None;
        }
        // Specifier-resolution gate: a no-`exports` (CJS) deep import must name
        // the exact on-disk file — Node ESM never auto-appends extensions. Append
        // the real extension from the cache (`lodash/_baseUnary` ->
        // `lodash/_baseUnary.js`); if no real file backs it, it is not safely
        // externalizable, so keep it vendored.
        let export_specifier = resolve_package_deep_import_specifier(
            manifest.package_json_source.as_str(),
            module_package_name,
            export_specifier.as_str(),
            &manifest.entry_paths,
        )?;

        Some(DetectedPackageExternalizationDecision {
            package_version: package_version.to_string(),
            subpath: detected_package_subpath(export_specifier.as_str()),
            resolved_file: format!(
                "forced-external:semantic-path:{module_package_name}@{package_version}/{}",
                module.semantic_path.trim().trim_matches('/')
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

pub(crate) fn promote_verified_externalization_hints(
    connection: &Connection,
    bundle: &mut InputBundle,
) -> Result<usize, SqliteInputError> {
    let relevant_hint_keys = promotable_hint_keys(bundle);
    if relevant_hint_keys.is_empty() {
        return Ok(0);
    }
    let hints = load_externalization_hint_proofs(connection, &relevant_hint_keys)?;
    if hints.is_empty() {
        return Ok(0);
    }
    let dependency_modules = bundle
        .dependencies
        .iter()
        .filter_map(|dependency| match dependency.target {
            ModuleDependencyTarget::Module(_) => Some(dependency.from_module_id),
            ModuleDependencyTarget::Package { .. } => None,
        })
        .collect::<BTreeSet<_>>();
    let module_paths = bundle
        .modules
        .iter()
        .map(|module| (module.id, module.semantic_path.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut module_source = BTreeMap::<ModuleId, String>::new();
    for module in &bundle.modules {
        if let Some(slice) = bundle.module_source_slice(module.id) {
            module_source.insert(module.id, slice.source.to_string());
        }
    }

    let mut promoted = 0usize;
    for attribution in &mut bundle.package_attributions {
        if !attribution_is_hint_promotable(attribution) {
            continue;
        }
        let Some(package_version) = attribution.package_version.as_deref() else {
            continue;
        };
        let Some(export_specifier) = attribution.export_specifier.as_deref() else {
            continue;
        };
        let key = (
            attribution.package_name.clone(),
            package_version.to_string(),
            export_specifier.to_string(),
        );
        let Some(candidate_hints) = hints.get(&key) else {
            continue;
        };
        let Some(source) = module_source.get(&attribution.module_id) else {
            continue;
        };
        let Some(module_path) = module_paths.get(&attribution.module_id) else {
            continue;
        };
        for hint in candidate_hints {
            if !dependency_modules.contains(&attribution.module_id)
                && module_source_matches_hint_hashes(source.as_str(), module_path.as_str(), hint)
            {
                attribution.resolved_file =
                    Some(format!("normalized-source-export:{}", hint.source_path));
                attribution.rejection_reason = None;
                promoted += 1;
                break;
            }
            if source_public_members_are_proven_by_hint(
                source.as_str(),
                module_path.as_str(),
                &hint.public_members,
            ) {
                attribution.resolved_file = Some(format!(
                    "forced-external:export-members:public-members:{}:{}",
                    hint.public_members
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(","),
                    hint.source_path
                ));
                attribution.rejection_reason = None;
                promoted += 1;
                break;
            }
        }
    }
    Ok(promoted)
}

fn promotable_hint_keys(bundle: &InputBundle) -> BTreeSet<HintKey> {
    bundle
        .package_attributions
        .iter()
        .filter(|attribution| attribution_is_hint_promotable(attribution))
        .filter_map(|attribution| {
            let package_version = attribution.package_version.as_deref()?;
            let export_specifier = attribution.export_specifier.as_deref()?;
            Some((
                attribution.package_name.clone(),
                package_version.to_string(),
                export_specifier.to_string(),
            ))
        })
        .collect()
}

fn attribution_is_hint_promotable(attribution: &PackageAttributionInput) -> bool {
    attribution.status == PackageAttributionStatus::Accepted
        && attribution.emission_mode == PackageEmissionMode::ExternalImport
        && !attribution_has_worker_asset_hint(attribution)
        && !attribution_has_strong_source_proof(attribution)
}

fn attribution_has_worker_asset_hint(attribution: &PackageAttributionInput) -> bool {
    [
        attribution.export_specifier.as_deref(),
        attribution.resolved_file.as_deref(),
        attribution.subpath.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|value| value.to_ascii_lowercase().contains(".worker"))
}

fn attribution_has_strong_source_proof(attribution: &PackageAttributionInput) -> bool {
    attribution.resolved_file.as_deref().is_some_and(|value| {
        value.starts_with("normalized-source-export:")
            || value.starts_with("exact-hint:")
            || value.starts_with("forced-external:export-members:")
    })
}

fn load_externalization_hint_proofs(
    connection: &Connection,
    relevant_hint_keys: &BTreeSet<HintKey>,
) -> Result<BTreeMap<HintKey, Vec<ExternalizationHintProof>>, SqliteInputError> {
    if relevant_hint_keys.is_empty() {
        return Ok(BTreeMap::new());
    }
    if !sqlite_table_exists(connection, "package_externalization_hints")? {
        return Ok(BTreeMap::new());
    }
    let has_public_members_json = sqlite_table_has_column(
        connection,
        "package_externalization_hints",
        "public_members_json",
    )?;
    for required in [
        "package_name",
        "package_version",
        "entry_path",
        "export_specifier",
        "normalized_source_hash",
    ] {
        if !sqlite_table_has_column(connection, "package_externalization_hints", required)? {
            return Ok(BTreeMap::new());
        }
    }

    let public_members_expr = if has_public_members_json {
        "public_members_json"
    } else {
        "'[]'"
    };
    let mut statement = connection.prepare(
        format!(
            r"
        SELECT package_name, package_version, entry_path, export_specifier,
               normalized_source_hash, {public_members_expr}
          FROM package_externalization_hints
         WHERE TRIM(COALESCE(package_name, '')) != ''
           AND TRIM(COALESCE(package_version, '')) != ''
           AND TRIM(COALESCE(entry_path, '')) != ''
           AND TRIM(COALESCE(export_specifier, '')) != ''
           AND TRIM(COALESCE(normalized_source_hash, '')) != ''
        "
        )
        .as_str(),
    )?;
    let rows = statement.query_map([], |row| {
        let package_name = row.get::<_, String>(0)?.trim().to_string();
        let package_version = row.get::<_, String>(1)?.trim().to_string();
        let entry_path = clean_hint_entry_path(
            package_name.as_str(),
            package_version.as_str(),
            row.get::<_, String>(2)?.as_str(),
        );
        let export_specifier = row.get::<_, String>(3)?.trim().to_string();
        let normalized_source_hash = row.get::<_, String>(4)?.trim().to_string();
        let public_members = parse_public_members(row.get::<_, Option<String>>(5)?.as_deref());
        let normalized_source_hashes = BTreeSet::from([normalized_source_hash]);
        Ok((
            (
                package_name.clone(),
                package_version.clone(),
                export_specifier,
            ),
            ExternalizationHintProof {
                source_path: format!("{package_name}@{package_version}/{entry_path}"),
                normalized_source_hashes,
                public_members,
            },
        ))
    })?;
    let mut hints = BTreeMap::<HintKey, Vec<ExternalizationHintProof>>::new();
    for (key, proof) in collect_sqlite_rows(rows)? {
        if !relevant_hint_keys.contains(&key) {
            continue;
        }
        hints.entry(key).or_default().push(proof);
    }
    enrich_hint_proofs_with_package_source_alternate_hashes(connection, &mut hints)?;
    Ok(hints)
}

fn enrich_hint_proofs_with_package_source_alternate_hashes(
    connection: &Connection,
    hints: &mut BTreeMap<HintKey, Vec<ExternalizationHintProof>>,
) -> Result<(), SqliteInputError> {
    if hints.is_empty() || !sqlite_table_exists(connection, "package_source_cache")? {
        return Ok(());
    }
    for required in [
        "package_name",
        "package_version",
        "entry_path",
        "source_content",
    ] {
        if !sqlite_table_has_column(connection, "package_source_cache", required)? {
            return Ok(());
        }
    }
    let mut statement = connection.prepare(
        r"
        SELECT source_content
          FROM package_source_cache
         WHERE package_name = ?1
           AND package_version = ?2
           AND entry_path = ?3
         LIMIT 1
        ",
    )?;
    for ((package_name, package_version, _export_specifier), proofs) in hints {
        for proof in proofs {
            let entry_path = clean_hint_entry_path(
                package_name.as_str(),
                package_version.as_str(),
                proof.source_path.as_str(),
            );
            let mut rows =
                statement.query((package_name.as_str(), package_version.as_str(), entry_path))?;
            let Some(row) = rows.next()? else {
                continue;
            };
            let source_content = row.get::<_, String>(0)?;
            let primary_hash =
                package_source_normalized_hash(proof.source_path.as_str(), source_content.as_str());
            if primary_hash
                .as_deref()
                .is_none_or(|hash| !proof.normalized_source_hashes.contains(hash))
            {
                continue;
            }
            proof
                .normalized_source_hashes
                .extend(package_source_normalized_hashes(
                    proof.source_path.as_str(),
                    source_content.as_str(),
                ));
        }
    }
    Ok(())
}

fn module_source_matches_hint_hashes(
    source: &str,
    module_path: &str,
    hint: &ExternalizationHintProof,
) -> bool {
    let mut hashes = package_source_normalized_hashes(hint.source_path.as_str(), source);
    hashes.extend(package_source_normalized_hashes(module_path, source));
    !hashes.is_disjoint(&hint.normalized_source_hashes)
}

fn source_has_commonjs_named_exports(source: &str) -> bool {
    let compact = source
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect::<String>();
    compact.contains("exports.") || compact.contains(".exports={")
}

fn source_public_members_are_proven_by_hint(
    source: &str,
    module_path: &str,
    hint_public_members: &BTreeSet<String>,
) -> bool {
    if hint_public_members.is_empty() || !source_has_commonjs_named_exports(source) {
        return false;
    }
    let source_public_members = package_source_exported_members(module_path, source)
        .into_iter()
        .filter(|member| is_identifier_like(member.as_str()))
        .collect::<BTreeSet<_>>();
    !source_public_members.is_empty() && source_public_members.is_subset(hint_public_members)
}

fn parse_public_members(value: Option<&str>) -> BTreeSet<String> {
    value
        .and_then(|json| serde_json::from_str::<Vec<String>>(json).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|member| is_identifier_like(member.as_str()))
        .collect()
}

fn is_identifier_like(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first == '$' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch == '$' || ch.is_ascii_alphanumeric())
}

fn clean_hint_entry_path(package_name: &str, package_version: &str, entry_path: &str) -> String {
    entry_path
        .trim()
        .trim_matches('/')
        .strip_prefix(format!("{package_name}@{package_version}/").as_str())
        .unwrap_or(entry_path.trim().trim_matches('/'))
        .to_string()
}
