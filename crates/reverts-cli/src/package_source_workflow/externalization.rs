//! Externalization-hint reads and package-source promotion.

use std::collections::BTreeSet;

use reverts_ir::hash::fnv1a_hex as stable_hash;
use reverts_ir::split_bare_specifier;
use reverts_package::is_node_builtin;
use reverts_package_matcher::{
    PackageSource, package_source_entry_path, package_source_exported_members,
    package_source_normalized_hash,
};
use rusqlite::{Connection, params_from_iter};

use crate::errors::MatchPackagesError;
use crate::persistence::externalization_hints::{
    PACKAGE_EXTERNALIZATION_HINT_POLICY_VERSION, PackageExternalizationHint,
};
use crate::persistence::source_cache::PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION;
use crate::{
    collect_sqlite_rows, sqlite_placeholders, sqlite_table_exists, sqlite_table_has_column,
};

use super::clean_package_entry_path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalizationHintCandidate {
    pub(crate) package_name: String,
    pub(crate) package_version: String,
    pub(crate) entry_path: String,
    pub(crate) source_content: String,
    pub(crate) content_hash: String,
    pub(crate) export_specifier: String,
    pub(crate) external_importable: bool,
}

pub(crate) fn externalization_hint_candidates_from_cache(
    connection: &Connection,
    package_names: &BTreeSet<String>,
    limit: Option<u32>,
) -> Result<Vec<ExternalizationHintCandidate>, MatchPackagesError> {
    for required in [
        "external_importable",
        "external_import_policy_version",
        "export_specifier",
    ] {
        if !sqlite_table_has_column(connection, "package_source_cache", required)
            .map_err(MatchPackagesError::QueryPackageSources)?
        {
            return Ok(Vec::new());
        }
    }

    let mut sql = format!(
        r"
        SELECT package_name, package_version, entry_path, source_content,
               content_hash, export_specifier, external_importable
          FROM package_source_cache
         WHERE TRIM(COALESCE(package_name, '')) != ''
           AND TRIM(COALESCE(package_version, '')) != ''
           AND TRIM(COALESCE(entry_path, '')) != ''
           AND TRIM(COALESCE(source_content, '')) != ''
           AND TRIM(COALESCE(content_hash, '')) != ''
           AND TRIM(COALESCE(export_specifier, '')) != ''
           AND external_import_policy_version = {PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION}
        "
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
    if let Some(limit) = limit {
        use std::fmt::Write as _;
        let _ = write!(sql, " LIMIT {limit}");
    }

    let mut statement = connection
        .prepare(sql.as_str())
        .map_err(MatchPackagesError::QueryPackageSources)?;
    let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<ExternalizationHintCandidate> {
        Ok(ExternalizationHintCandidate {
            package_name: row.get::<_, String>(0)?.trim().to_string(),
            package_version: row.get::<_, String>(1)?.trim().to_string(),
            entry_path: clean_package_entry_path(row.get::<_, String>(2)?.as_str()),
            source_content: row.get(3)?,
            content_hash: row.get::<_, String>(4)?.trim().to_string(),
            export_specifier: row.get::<_, String>(5)?.trim().to_string(),
            external_importable: row.get::<_, i64>(6)? != 0,
        })
    };
    if package_names.is_empty() {
        let rows = statement
            .query_map([], map_row)
            .map_err(MatchPackagesError::QueryPackageSources)?;
        collect_sqlite_rows(rows).map_err(MatchPackagesError::QueryPackageSources)
    } else {
        let rows = statement
            .query_map(params_from_iter(package_names.iter()), map_row)
            .map_err(MatchPackagesError::QueryPackageSources)?;
        collect_sqlite_rows(rows).map_err(MatchPackagesError::QueryPackageSources)
    }
}
pub(crate) fn promote_package_sources_with_externalization_hints(
    connection: &Connection,
    package_names: &BTreeSet<String>,
    package_sources: &mut Vec<PackageSource>,
) -> Result<usize, MatchPackagesError> {
    let hints = load_package_externalization_hints(connection, package_names)?;
    if hints.is_empty() || package_sources.is_empty() {
        return Ok(0);
    }
    let mut promoted = Vec::new();
    for hint in hints {
        if hint
            .proof_policy_version
            .is_some_and(|version| version != PACKAGE_EXTERNALIZATION_HINT_POLICY_VERSION)
        {
            continue;
        }
        if !hint_export_specifier_matches_package(
            hint.package_name.as_str(),
            hint.export_specifier.as_str(),
        ) {
            continue;
        }
        if hint.content_hash.is_none()
            && hint.normalized_source_hash.is_none()
            && hint.public_members.is_empty()
        {
            continue;
        }
        let Some(source) = package_sources.iter().find(|source| {
            source.package_name == hint.package_name
                && source.package_version == hint.package_version
                && package_source_entry_path(source) == hint.entry_path
        }) else {
            continue;
        };
        if source.external_importable {
            continue;
        }
        let direct_specifier_match = source.export_specifier == hint.export_specifier;
        if !direct_specifier_match && hint.public_members.is_empty() {
            continue;
        }
        if let Some(content_hash) = hint.content_hash.as_deref()
            && stable_hash(source.source.as_bytes()) != content_hash
        {
            continue;
        }
        if let Some(normalized_source_hash) = hint.normalized_source_hash.as_deref()
            && package_source_normalized_hash(source.source_path.as_str(), source.source.as_str())
                .as_deref()
                != Some(normalized_source_hash)
        {
            continue;
        }
        if !hint.public_members.is_empty() {
            let exported_members = package_source_exported_members(
                source.source_path.as_str(),
                source.source.as_str(),
            );
            if !hint.public_members.is_subset(&exported_members) {
                continue;
            }
        }
        promoted.push(PackageSource::external(
            source.package_name.as_str(),
            source.package_version.as_str(),
            hint.export_specifier.as_str(),
            source.source_path.as_str(),
            source.source.as_str(),
        ));
    }
    let promoted_len = promoted.len();
    package_sources.extend(promoted);
    Ok(promoted_len)
}

fn load_package_externalization_hints(
    connection: &Connection,
    package_names: &BTreeSet<String>,
) -> Result<Vec<PackageExternalizationHint>, MatchPackagesError> {
    if !sqlite_table_exists(connection, "package_externalization_hints")
        .map_err(MatchPackagesError::QueryPackageSources)?
    {
        return Ok(Vec::new());
    }
    let has_content_hash =
        sqlite_table_has_column(connection, "package_externalization_hints", "content_hash")
            .map_err(MatchPackagesError::QueryPackageSources)?;
    let has_normalized_source_hash = sqlite_table_has_column(
        connection,
        "package_externalization_hints",
        "normalized_source_hash",
    )
    .map_err(MatchPackagesError::QueryPackageSources)?;
    let has_public_members = sqlite_table_has_column(
        connection,
        "package_externalization_hints",
        "public_members_json",
    )
    .map_err(MatchPackagesError::QueryPackageSources)?;
    let has_policy_version = sqlite_table_has_column(
        connection,
        "package_externalization_hints",
        "proof_policy_version",
    )
    .map_err(MatchPackagesError::QueryPackageSources)?;
    for required in [
        "package_name",
        "package_version",
        "entry_path",
        "export_specifier",
    ] {
        if !sqlite_table_has_column(connection, "package_externalization_hints", required)
            .map_err(MatchPackagesError::QueryPackageSources)?
        {
            return Ok(Vec::new());
        }
    }
    let content_hash_expr = if has_content_hash {
        "content_hash"
    } else {
        "NULL"
    };
    let normalized_hash_expr = if has_normalized_source_hash {
        "normalized_source_hash"
    } else {
        "NULL"
    };
    let public_members_expr = if has_public_members {
        "public_members_json"
    } else {
        "NULL"
    };
    let policy_version_expr = if has_policy_version {
        "proof_policy_version"
    } else {
        "NULL"
    };
    let mut sql = format!(
        r"
        SELECT package_name, package_version, entry_path, export_specifier,
               {content_hash_expr}, {normalized_hash_expr},
               {public_members_expr}, {policy_version_expr}
          FROM package_externalization_hints
         WHERE TRIM(COALESCE(package_name, '')) != ''
           AND TRIM(COALESCE(package_version, '')) != ''
           AND TRIM(COALESCE(entry_path, '')) != ''
           AND TRIM(COALESCE(export_specifier, '')) != ''
        "
    );
    if !package_names.is_empty() {
        use std::fmt::Write as _;
        let _ = write!(
            sql,
            " AND package_name IN ({})",
            sqlite_placeholders(package_names.len())
        );
    }
    let mut statement = connection
        .prepare(sql.as_str())
        .map_err(MatchPackagesError::QueryPackageSources)?;
    let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<PackageExternalizationHint> {
        let package_name = row.get::<_, String>(0)?.trim().to_string();
        let package_version = row.get::<_, String>(1)?.trim().to_string();
        let raw_entry_path = row.get::<_, String>(2)?;
        Ok(PackageExternalizationHint {
            package_name: package_name.clone(),
            package_version: package_version.clone(),
            entry_path: clean_package_entry_path(
                hint_entry_path(
                    package_name.as_str(),
                    package_version.as_str(),
                    raw_entry_path.as_str(),
                )
                .trim()
                .trim_matches('/'),
            ),
            export_specifier: row.get::<_, String>(3)?.trim().to_string(),
            content_hash: row
                .get::<_, Option<String>>(4)?
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            normalized_source_hash: row
                .get::<_, Option<String>>(5)?
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            public_members: parse_public_members_hint(row.get::<_, Option<String>>(6)?.as_deref()),
            proof_policy_version: row.get::<_, Option<i64>>(7)?,
        })
    };
    if package_names.is_empty() {
        let rows = statement
            .query_map([], map_row)
            .map_err(MatchPackagesError::QueryPackageSources)?;
        collect_sqlite_rows(rows).map_err(MatchPackagesError::QueryPackageSources)
    } else {
        let rows = statement
            .query_map(params_from_iter(package_names.iter()), map_row)
            .map_err(MatchPackagesError::QueryPackageSources)?;
        collect_sqlite_rows(rows).map_err(MatchPackagesError::QueryPackageSources)
    }
}

fn hint_entry_path<'a>(package_name: &str, package_version: &str, entry_path: &'a str) -> &'a str {
    let clean = entry_path.trim();
    clean
        .strip_prefix(format!("{package_name}@{package_version}/").as_str())
        .unwrap_or(clean)
}

fn parse_public_members_hint(value: Option<&str>) -> BTreeSet<String> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return BTreeSet::new();
    };
    serde_json::from_str::<Vec<String>>(value)
        .unwrap_or_default()
        .into_iter()
        .map(|member| member.trim().to_string())
        .filter(|member| !member.is_empty())
        .collect()
}

pub(crate) fn hint_export_specifier_matches_package(
    package_name: &str,
    export_specifier: &str,
) -> bool {
    split_bare_specifier(export_specifier).is_some_and(|(specifier_package, _subpath)| {
        specifier_package == package_name && !is_node_builtin(specifier_package.as_str())
    })
}
