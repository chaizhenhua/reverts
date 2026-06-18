//! Persistent cache of matcher [`PackageSource`] records: schema
//! creation, on-the-fly migrations (so older SQLite snapshots gain the
//! current columns and primary key shape), and the
//! [`persist_package_source_cache`] writer. The cache lets repeated
//! `match-packages` runs skip re-downloading or re-walking node
//! packages whose source content has not changed.

use std::collections::{BTreeMap, BTreeSet};

use reverts_ir::hash::fnv1a_hex as stable_hash;
use reverts_package_matcher::{PackageSource, package_source_entry_path};
use rusqlite::{Connection, params, params_from_iter};

use crate::errors::MatchPackagesError;
use crate::{package_export_specifier, sqlite_placeholders, sqlite_table_has_column};

pub(crate) const PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION: i64 = 4;

const PACKAGE_SOURCE_CACHE_CREATE_SQL: &str = r"
CREATE TABLE IF NOT EXISTS package_source_cache (
    package_name TEXT NOT NULL,
    package_version TEXT NOT NULL,
    entry_path TEXT NOT NULL,
    source_content TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    external_importable INTEGER NOT NULL DEFAULT 1,
    external_import_policy_version INTEGER NOT NULL DEFAULT 0,
    export_specifier TEXT NOT NULL DEFAULT '',
    fetched_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    PRIMARY KEY (package_name, package_version, entry_path)
);
";

const PACKAGE_SOURCE_CACHE_INDEX_SQL: &str = r"
CREATE INDEX IF NOT EXISTS idx_package_cache_expires
    ON package_source_cache(expires_at);
";

pub(crate) fn persist_package_source_cache(
    connection: &mut Connection,
    package_sources: &[PackageSource],
) -> Result<usize, MatchPackagesError> {
    ensure_package_source_cache_table(connection)?;
    let transaction = connection
        .transaction()
        .map_err(MatchPackagesError::WritePackageSourceCache)?;
    let mut written = 0;
    for source in package_sources {
        let entry_path = package_source_cache_entry_path(source);
        let content_hash = stable_hash(source.source.as_bytes());
        written += transaction
            .execute(
                r"
                INSERT INTO package_source_cache
                    (package_name, package_version, entry_path, source_content,
                     content_hash, external_importable, external_import_policy_version,
                     export_specifier, fetched_at, expires_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, datetime('now'), datetime('now', '+30 days'))
                ON CONFLICT(package_name, package_version, entry_path) DO UPDATE SET
                    source_content = excluded.source_content,
                    content_hash = excluded.content_hash,
                    external_importable = excluded.external_importable,
                    external_import_policy_version = excluded.external_import_policy_version,
                    export_specifier = excluded.export_specifier,
                    fetched_at = excluded.fetched_at,
                    expires_at = excluded.expires_at
                ",
                params![
                    source.package_name.as_str(),
                    source.package_version.as_str(),
                    entry_path.as_str(),
                    source.source.as_str(),
                    content_hash.as_str(),
                    if source.external_importable {
                        1_i64
                    } else {
                        0_i64
                    },
                    PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION,
                    source.export_specifier.as_str(),
                ],
            )
            .map_err(MatchPackagesError::WritePackageSourceCache)?;
    }
    transaction
        .commit()
        .map_err(MatchPackagesError::WritePackageSourceCache)?;
    Ok(written)
}

fn ensure_package_source_cache_table(
    connection: &mut Connection,
) -> Result<(), MatchPackagesError> {
    connection
        .execute_batch(PACKAGE_SOURCE_CACHE_CREATE_SQL)
        .map_err(MatchPackagesError::WritePackageSourceCache)?;
    if package_source_cache_needs_entry_path_pk_migration(connection)
        .map_err(MatchPackagesError::WritePackageSourceCache)?
    {
        migrate_package_source_cache_entry_path_primary_key(connection)?;
    }
    if !sqlite_table_has_column(connection, "package_source_cache", "external_importable")
        .map_err(MatchPackagesError::WritePackageSourceCache)?
    {
        connection
            .execute_batch(
                r"
                ALTER TABLE package_source_cache
                    ADD COLUMN external_importable INTEGER NOT NULL DEFAULT 1;
                ",
            )
            .map_err(MatchPackagesError::WritePackageSourceCache)?;
    }
    if !sqlite_table_has_column(
        connection,
        "package_source_cache",
        "external_import_policy_version",
    )
    .map_err(MatchPackagesError::WritePackageSourceCache)?
    {
        connection
            .execute_batch(
                r"
                ALTER TABLE package_source_cache
                    ADD COLUMN external_import_policy_version INTEGER NOT NULL DEFAULT 0;
                ",
            )
            .map_err(MatchPackagesError::WritePackageSourceCache)?;
    }
    if !sqlite_table_has_column(connection, "package_source_cache", "export_specifier")
        .map_err(MatchPackagesError::WritePackageSourceCache)?
    {
        connection
            .execute_batch(
                r"
                ALTER TABLE package_source_cache
                    ADD COLUMN export_specifier TEXT NOT NULL DEFAULT '';
                ",
            )
            .map_err(MatchPackagesError::WritePackageSourceCache)?;
    }
    connection
        .execute_batch(PACKAGE_SOURCE_CACHE_INDEX_SQL)
        .map_err(MatchPackagesError::WritePackageSourceCache)?;
    Ok(())
}

fn package_source_cache_needs_entry_path_pk_migration(
    connection: &Connection,
) -> rusqlite::Result<bool> {
    let mut statement = connection.prepare("PRAGMA table_info(package_source_cache)")?;
    let columns = statement.query_map([], |row| {
        Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
    })?;
    let mut primary_key_columns = BTreeMap::<i64, String>::new();
    for column in columns {
        let (name, primary_key_ordinal) = column?;
        if primary_key_ordinal > 0 {
            primary_key_columns.insert(primary_key_ordinal, name);
        }
    }
    let ordered_primary_key = primary_key_columns.into_values().collect::<Vec<_>>();
    Ok(ordered_primary_key
        != vec![
            "package_name".to_string(),
            "package_version".to_string(),
            "entry_path".to_string(),
        ])
}

fn migrate_package_source_cache_entry_path_primary_key(
    connection: &mut Connection,
) -> Result<(), MatchPackagesError> {
    let has_external_importable =
        sqlite_table_has_column(connection, "package_source_cache", "external_importable")
            .map_err(MatchPackagesError::WritePackageSourceCache)?;
    let external_importable_expr = if has_external_importable {
        "external_importable"
    } else {
        "1"
    };
    let has_external_import_policy_version = sqlite_table_has_column(
        connection,
        "package_source_cache",
        "external_import_policy_version",
    )
    .map_err(MatchPackagesError::WritePackageSourceCache)?;
    let external_import_policy_version_expr = if has_external_import_policy_version {
        "external_import_policy_version"
    } else {
        "0"
    };
    let has_export_specifier =
        sqlite_table_has_column(connection, "package_source_cache", "export_specifier")
            .map_err(MatchPackagesError::WritePackageSourceCache)?;
    let export_specifier_expr = if has_export_specifier {
        "export_specifier"
    } else {
        "''"
    };
    let transaction = connection
        .transaction()
        .map_err(MatchPackagesError::WritePackageSourceCache)?;
    transaction
        .execute_batch(
            r"
            ALTER TABLE package_source_cache RENAME TO package_source_cache__reverts_old;
            ",
        )
        .map_err(MatchPackagesError::WritePackageSourceCache)?;
    transaction
        .execute_batch(PACKAGE_SOURCE_CACHE_CREATE_SQL)
        .map_err(MatchPackagesError::WritePackageSourceCache)?;
    transaction
        .execute_batch(
            format!(
                r"
            INSERT OR IGNORE INTO package_source_cache (
                package_name,
                package_version,
                entry_path,
                source_content,
                content_hash,
                external_importable,
                external_import_policy_version,
                export_specifier,
                fetched_at,
                expires_at
            )
            SELECT
                package_name,
                package_version,
                entry_path,
                source_content,
                content_hash,
                {external_importable_expr},
                {external_import_policy_version_expr},
                {export_specifier_expr},
                fetched_at,
                expires_at
              FROM package_source_cache__reverts_old
             WHERE TRIM(COALESCE(package_name, '')) != ''
               AND TRIM(COALESCE(package_version, '')) != ''
               AND TRIM(COALESCE(entry_path, '')) != ''
               AND TRIM(COALESCE(source_content, '')) != '';
            DROP TABLE package_source_cache__reverts_old;
            "
            )
            .as_str(),
        )
        .map_err(MatchPackagesError::WritePackageSourceCache)?;
    transaction
        .commit()
        .map_err(MatchPackagesError::WritePackageSourceCache)?;
    Ok(())
}

pub(crate) fn package_source_cache_entry_path(source: &PackageSource) -> String {
    package_source_entry_path(source)
}

pub(crate) fn package_source_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PackageSource> {
    let package_name = row.get::<_, String>(0)?;
    let package_version = row.get::<_, String>(1)?;
    let entry_path = row.get::<_, String>(2)?;
    let source = row.get::<_, String>(3)?;
    let external_importable = row.get::<_, i64>(4)? != 0;
    let cached_export_specifier = row.get::<_, String>(5)?;
    let export_specifier = if cached_export_specifier.trim().is_empty() {
        package_export_specifier(package_name.as_str(), entry_path.as_str())
    } else {
        cached_export_specifier.trim().to_string()
    };
    let source_path = format!("{package_name}@{package_version}/{entry_path}");
    if external_importable {
        Ok(PackageSource::external(
            package_name,
            package_version,
            export_specifier,
            source_path,
            source,
        ))
    } else {
        Ok(PackageSource::source_only(
            package_name,
            package_version,
            export_specifier,
            source_path,
            source,
        ))
    }
}

pub(crate) fn stale_package_source_cache_versions(
    connection: &Connection,
    package_names: &BTreeSet<String>,
) -> Result<BTreeSet<(String, String)>, MatchPackagesError> {
    let has_external_import_policy_version_column = sqlite_table_has_column(
        connection,
        "package_source_cache",
        "external_import_policy_version",
    )
    .map_err(MatchPackagesError::QueryPackageSources)?;
    let has_export_specifier_column =
        sqlite_table_has_column(connection, "package_source_cache", "export_specifier")
            .map_err(MatchPackagesError::QueryPackageSources)?;
    let stale_predicate = if has_external_import_policy_version_column
        && has_export_specifier_column
    {
        format!(
            "external_import_policy_version != {PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION} OR TRIM(COALESCE(export_specifier, '')) = ''"
        )
    } else {
        "1".to_string()
    };
    let mut sql = format!(
        r"
        SELECT DISTINCT package_name, package_version
          FROM package_source_cache
         WHERE TRIM(COALESCE(package_name, '')) != ''
           AND TRIM(COALESCE(package_version, '')) != ''
           AND TRIM(COALESCE(source_content, '')) != ''
           AND ({stale_predicate})
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
    let rows = if package_names.is_empty() {
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(MatchPackagesError::QueryPackageSources)?
            .collect::<rusqlite::Result<Vec<(String, String)>>>()
    } else {
        statement
            .query_map(params_from_iter(package_names.iter()), |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .map_err(MatchPackagesError::QueryPackageSources)?
            .collect::<rusqlite::Result<Vec<(String, String)>>>()
    }
    .map_err(MatchPackagesError::QueryPackageSources)?;
    Ok(rows.into_iter().collect())
}
