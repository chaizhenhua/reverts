//! Persist function-level `PackageAttributionInput` rows into
//! `package_function_attributions`. The matcher writes one row per
//! function span it can attribute to a specific package surface.
//!
//! This module also owns the table's `CREATE` / `CREATE INDEX` SQL and
//! the alt-tier migration (the table's `CHECK` constraint on `tier` was
//! extended with the `*_alternate` variants — older databases need their
//! table re-created with the new constraint).

use std::collections::BTreeMap;

use reverts_input::{InputRows, PackageAttributionInput};
use reverts_ir::ModuleId;
use rusqlite::{Connection, OptionalExtension, params};

use crate::errors::MatchPackagesError;

pub(crate) fn persist_function_attributions(
    connection: &mut Connection,
    rows: &InputRows,
    attributions: &[PackageAttributionInput],
) -> Result<usize, MatchPackagesError> {
    if attributions.is_empty() {
        return Ok(0);
    }
    ensure_package_function_attributions_table(connection)?;

    let modules_by_id: BTreeMap<ModuleId, &str> = rows
        .modules
        .iter()
        .map(|m| (m.id, m.original_name.as_str()))
        .collect();

    let transaction = connection
        .transaction()
        .map_err(MatchPackagesError::WriteAttribution)?;
    let mut written = 0;

    for attribution in attributions {
        let Some(function_span) = attribution.function_span else {
            // Function-level attribution requires a span; matcher code only
            // emits rows with `with_function_span(...)`, so this is a programmer
            // error rather than user input — surface it instead of skipping.
            return Err(MatchPackagesError::InvalidAttribution {
                module_id: attribution.module_id,
                message: "function attribution missing function_span".to_string(),
            });
        };
        let Some(confidence) = attribution.confidence.as_ref() else {
            return Err(MatchPackagesError::InvalidAttribution {
                module_id: attribution.module_id,
                message: "function attribution missing confidence".to_string(),
            });
        };
        let module_original_name = modules_by_id.get(&attribution.module_id).copied().ok_or(
            MatchPackagesError::MissingModuleForAttribution {
                module_id: attribution.module_id,
            },
        )?;
        let package_version = attribution.package_version.as_deref().ok_or(
            MatchPackagesError::InvalidAttribution {
                module_id: attribution.module_id,
                message: "function attribution missing package version".to_string(),
            },
        )?;
        let export_specifier = attribution.export_specifier.as_deref().ok_or(
            MatchPackagesError::InvalidAttribution {
                module_id: attribution.module_id,
                message: "function attribution missing export specifier".to_string(),
            },
        )?;
        let matched_axes_json = serde_json::Value::Array(
            confidence
                .matched_axes
                .iter()
                .map(|a| serde_json::Value::String(a.as_str().to_string()))
                .collect(),
        )
        .to_string();
        let matched_alternate = confidence.matched_alternate.map(|p| p.as_str().to_string());

        transaction
            .execute(
                r"
                INSERT INTO package_function_attributions
                    (module_id, module_original_name, package_name, package_version,
                     export_specifier, function_span_start, function_span_end,
                     tier, matched_alternate, matched_axes_json,
                     top_score, runner_up_score, margin,
                     created_at, updated_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                        datetime('now'), datetime('now'))
                ON CONFLICT(module_id, function_span_start, function_span_end) DO UPDATE SET
                    module_original_name = excluded.module_original_name,
                    package_name = excluded.package_name,
                    package_version = excluded.package_version,
                    export_specifier = excluded.export_specifier,
                    tier = excluded.tier,
                    matched_alternate = excluded.matched_alternate,
                    matched_axes_json = excluded.matched_axes_json,
                    top_score = excluded.top_score,
                    runner_up_score = excluded.runner_up_score,
                    margin = excluded.margin,
                    updated_at = datetime('now')
                ",
                params![
                    i64::from(attribution.module_id.0),
                    module_original_name,
                    attribution.package_name.as_str(),
                    package_version,
                    export_specifier,
                    i64::from(function_span.start),
                    i64::from(function_span.end),
                    confidence.tier.as_str(),
                    matched_alternate,
                    matched_axes_json,
                    confidence.top_score,
                    confidence.runner_up_score,
                    confidence.margin,
                ],
            )
            .map_err(MatchPackagesError::WriteAttribution)?;
        written += 1;
    }

    transaction
        .commit()
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(written)
}

fn ensure_package_function_attributions_table(
    connection: &mut Connection,
) -> Result<(), MatchPackagesError> {
    connection
        .execute_batch(PACKAGE_FUNCTION_ATTRIBUTIONS_CREATE_SQL)
        .map_err(MatchPackagesError::WriteAttribution)?;
    if package_function_attributions_requires_alt_tier_migration(connection)
        .map_err(MatchPackagesError::WriteAttribution)?
    {
        migrate_package_function_attributions_alt_tier(connection)?;
    }
    connection
        .execute_batch(PACKAGE_FUNCTION_ATTRIBUTIONS_INDEX_SQL)
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(())
}

/// True when the existing `package_function_attributions` table was
/// created before any of the alt-tier names were added, i.e. its
/// CHECK constraint does not list one of the expected values.
/// Detected by peeking at the persisted `sql` text in `sqlite_master`.
fn package_function_attributions_requires_alt_tier_migration(
    connection: &Connection,
) -> rusqlite::Result<bool> {
    let sql: Option<String> = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='package_function_attributions'",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    Ok(sql
        .map(|s| {
            !s.contains("structural_anchored_alternate")
                || !s.contains("feature_similarity_alternate")
                || !s.contains("structural_only_alternate")
        })
        .unwrap_or(false))
}

fn migrate_package_function_attributions_alt_tier(
    connection: &mut Connection,
) -> Result<(), MatchPackagesError> {
    let transaction = connection
        .transaction()
        .map_err(MatchPackagesError::WriteAttribution)?;
    transaction
        .execute_batch(
            r"
            ALTER TABLE package_function_attributions
                RENAME TO package_function_attributions__reverts_old;
            ",
        )
        .map_err(MatchPackagesError::WriteAttribution)?;
    transaction
        .execute_batch(PACKAGE_FUNCTION_ATTRIBUTIONS_CREATE_SQL)
        .map_err(MatchPackagesError::WriteAttribution)?;
    transaction
        .execute_batch(
            r"
            INSERT INTO package_function_attributions
            SELECT * FROM package_function_attributions__reverts_old;
            DROP TABLE package_function_attributions__reverts_old;
            ",
        )
        .map_err(MatchPackagesError::WriteAttribution)?;
    transaction
        .commit()
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(())
}

const PACKAGE_FUNCTION_ATTRIBUTIONS_CREATE_SQL: &str = r"
CREATE TABLE IF NOT EXISTS package_function_attributions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    module_id INTEGER NOT NULL,
    module_original_name TEXT NOT NULL,
    package_name TEXT NOT NULL,
    package_version TEXT NOT NULL,
    export_specifier TEXT NOT NULL,
    function_span_start INTEGER NOT NULL,
    function_span_end INTEGER NOT NULL,
    tier TEXT NOT NULL,
    matched_alternate TEXT,
    matched_axes_json TEXT NOT NULL,
    top_score REAL NOT NULL,
    runner_up_score REAL NOT NULL,
    margin REAL NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (module_id, function_span_start, function_span_end),
    FOREIGN KEY (module_id) REFERENCES modules(id) ON DELETE CASCADE,
    CHECK (TRIM(module_original_name) != ''),
    CHECK (TRIM(package_name) != ''),
    CHECK (TRIM(package_version) != ''),
    CHECK (TRIM(export_specifier) != ''),
    CHECK (function_span_start <= function_span_end),
    CHECK (tier IN (
        'exact',
        'exact_alternate',
        'structural_anchored',
        'structural_anchored_alternate',
        'feature_similarity',
        'feature_similarity_alternate',
        'structural_only',
        'structural_only_alternate'
    )),
    CHECK (margin >= 0.0 AND margin <= 1.0)
);
";

const PACKAGE_FUNCTION_ATTRIBUTIONS_INDEX_SQL: &str = r"
CREATE INDEX IF NOT EXISTS idx_package_function_attributions_module
    ON package_function_attributions(module_id);
CREATE INDEX IF NOT EXISTS idx_package_function_attributions_package
    ON package_function_attributions(package_name, package_version);
CREATE INDEX IF NOT EXISTS idx_package_function_attributions_tier
    ON package_function_attributions(tier);
";
