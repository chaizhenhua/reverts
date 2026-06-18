//! Persistent cache of matcher [`PackageSource`] records: schema
//! creation, on-the-fly migrations (so older SQLite snapshots gain the
//! current columns and primary key shape), and the
//! [`persist_package_source_cache`] writer. The cache lets repeated
//! `match-packages` runs skip re-downloading or re-walking node
//! packages whose source content has not changed.

use std::collections::{BTreeMap, BTreeSet};

use reverts_ir::hash::fnv1a_hex as stable_hash;
use reverts_package::{PackageSourceCacheView, clean_package_entry_path, package_source_path};
use reverts_package_matcher::{PackageSource, package_source_entry_path};
use rusqlite::{Connection, params, params_from_iter};

use crate::errors::MatchPackagesError;
use crate::{
    collect_sqlite_rows, package_export_specifier, sqlite_placeholders, sqlite_table_exists,
    sqlite_table_has_column,
};

pub(crate) const PACKAGE_SOURCE_CACHE_EXTERNAL_IMPORT_POLICY_VERSION: i64 = 4;

pub(crate) type MaterializedPackageManifests = PackageSourceCacheView;

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

pub(crate) fn load_package_source_cache_view(
    connection: &Connection,
) -> rusqlite::Result<MaterializedPackageManifests> {
    if !sqlite_table_exists(connection, "package_source_cache")? {
        return Ok(PackageSourceCacheView::default());
    }
    for required in [
        "package_name",
        "package_version",
        "entry_path",
        "source_content",
    ] {
        if !sqlite_table_has_column(connection, "package_source_cache", required)? {
            return Ok(PackageSourceCacheView::default());
        }
    }
    let mut cache = PackageSourceCacheView::default();
    let mut statement = connection.prepare(
        "SELECT package_name, package_version, entry_path, source_content \
           FROM package_source_cache \
          WHERE TRIM(COALESCE(entry_path, '')) != '' \
            AND TRIM(COALESCE(package_name, '')) != '' \
            AND TRIM(COALESCE(package_version, '')) != ''",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for (name, version, entry_path, source_content) in collect_sqlite_rows(rows)? {
        cache.insert_source(
            name.as_str(),
            version.as_str(),
            entry_path.as_str(),
            source_content,
        );
    }
    Ok(cache)
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
    let source_path = package_source_path(
        package_name.as_str(),
        package_version.as_str(),
        entry_path.as_str(),
    );
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

#[derive(Debug, Clone, Copy)]
struct PackageSourceCacheColumns {
    has_external_importable: bool,
    has_external_import_policy_version: bool,
    has_export_specifier: bool,
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
        })
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

pub(crate) fn load_cached_package_sources(
    connection: &Connection,
    package_names: &BTreeSet<String>,
    materialize_package_sources: bool,
) -> Result<Vec<PackageSource>, MatchPackagesError> {
    let columns = PackageSourceCacheColumns::load(connection)?;
    let cache_import_policy_predicate =
        columns.cache_import_policy_predicate(materialize_package_sources);
    let external_importable_expr = columns.external_importable_select_expr();
    let export_specifier_expr = columns.export_specifier_select_expr();
    let mut sql = String::from(
        format!(
            r"
        SELECT package_name, package_version, entry_path, source_content,
               {external_importable_expr},
               {export_specifier_expr}
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
