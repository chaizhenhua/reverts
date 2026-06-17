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
    InputBundle, ModuleDependencyTarget, PackageAttributionInput, PackageAttributionStatus,
    PackageEmissionMode,
};
use reverts_ir::ModuleId;
use reverts_package_matcher::package_source_normalized_hash;
use rusqlite::{Connection, OpenFlags};

use crate::{collect_sqlite_rows, sqlite_table_exists, sqlite_table_has_column};

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExternalizationHintProof {
    source_path: String,
    normalized_source_hash: String,
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
    promote_verified_externalization_hints(&connection, &mut bundle)?;
    Ok(bundle)
}

pub(crate) fn promote_verified_externalization_hints(
    connection: &Connection,
    bundle: &mut InputBundle,
) -> Result<usize, SqliteInputError> {
    let hints = load_externalization_hint_proofs(connection)?;
    if hints.is_empty() || bundle.package_attributions.is_empty() {
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
        if !attribution_is_promotable(attribution, &dependency_modules) {
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
            if package_source_normalized_hash(hint.source_path.as_str(), source.as_str()).as_deref()
                == Some(hint.normalized_source_hash.as_str())
            {
                attribution.resolved_file =
                    Some(format!("normalized-source-export:{}", hint.source_path));
                attribution.rejection_reason = None;
                promoted += 1;
                break;
            }
            if package_source_normalized_hash(module_path.as_str(), source.as_str()).as_deref()
                == Some(hint.normalized_source_hash.as_str())
            {
                attribution.resolved_file =
                    Some(format!("normalized-source-export:{}", hint.source_path));
                attribution.rejection_reason = None;
                promoted += 1;
                break;
            }
        }
    }
    Ok(promoted)
}

fn attribution_is_promotable(
    attribution: &PackageAttributionInput,
    dependency_modules: &BTreeSet<ModuleId>,
) -> bool {
    attribution.status == PackageAttributionStatus::Accepted
        && attribution.emission_mode == PackageEmissionMode::ExternalImport
        && !dependency_modules.contains(&attribution.module_id)
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
) -> Result<BTreeMap<HintKey, Vec<ExternalizationHintProof>>, SqliteInputError> {
    if !sqlite_table_exists(connection, "package_externalization_hints")? {
        return Ok(BTreeMap::new());
    }
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

    let mut statement = connection.prepare(
        r"
        SELECT package_name, package_version, entry_path, export_specifier,
               normalized_source_hash
          FROM package_externalization_hints
         WHERE TRIM(COALESCE(package_name, '')) != ''
           AND TRIM(COALESCE(package_version, '')) != ''
           AND TRIM(COALESCE(entry_path, '')) != ''
           AND TRIM(COALESCE(export_specifier, '')) != ''
           AND TRIM(COALESCE(normalized_source_hash, '')) != ''
        ",
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
        Ok((
            (
                package_name.clone(),
                package_version.clone(),
                export_specifier,
            ),
            ExternalizationHintProof {
                source_path: format!("{package_name}@{package_version}/{entry_path}"),
                normalized_source_hash,
            },
        ))
    })?;
    let mut hints = BTreeMap::<HintKey, Vec<ExternalizationHintProof>>::new();
    for (key, proof) in collect_sqlite_rows(rows)? {
        hints.entry(key).or_default().push(proof);
    }
    Ok(hints)
}

fn clean_hint_entry_path(package_name: &str, package_version: &str, entry_path: &str) -> String {
    entry_path
        .trim()
        .trim_matches('/')
        .strip_prefix(format!("{package_name}@{package_version}/").as_str())
        .unwrap_or(entry_path.trim().trim_matches('/'))
        .to_string()
}
