//! Persist per-binding package anchors recovered from a scope-hoisted bundle's
//! eager entry island into `package_island_anchors`.
//!
//! Unlike [`super::function_attributions`], an island anchor is NOT keyed by a
//! model `module_id`: the eager bindings it describes were flattened into the
//! synthesized entry-island file and have no module of their own. They are keyed
//! by `(source_file_id, binding_name)` instead — the stable coordinates the
//! later generate stage can join against to drop a library binding from the
//! naming denominator (and, eventually, to externalize it).
//!
//! This module owns the table's `CREATE` / `CREATE INDEX` SQL and its
//! reader/writer; the matcher produces the rows, the generator consumes them.

use rusqlite::{Connection, params};

use crate::errors::MatchPackagesError;

/// One eager island binding anchored to a package surface.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IslandPackageAnchor {
    pub(crate) source_file_id: u32,
    /// Bundle-local (typically minified) binding name.
    pub(crate) binding_name: String,
    pub(crate) package_name: String,
    pub(crate) package_version: String,
    pub(crate) export_specifier: String,
    /// Absolute byte range of the matched function within the bundle source.
    pub(crate) function_span_start: u32,
    pub(crate) function_span_end: u32,
    /// Cascade tier string (e.g. `structural_anchored`).
    pub(crate) tier: String,
    /// Whether the matched package source is safe to emit as an external import.
    pub(crate) external_importable: bool,
    pub(crate) top_score: f64,
    pub(crate) runner_up_score: f64,
    pub(crate) margin: f64,
}

/// Replace this project's island anchors with `anchors`.
///
/// Anchors are a pure, deterministic function of the bundle source plus the
/// package corpus, so a re-run fully supersedes the previous set — delete then
/// insert keeps the table from accumulating stale rows when the corpus changes.
pub(crate) fn persist_island_anchors(
    connection: &mut Connection,
    project_id: i64,
    anchors: &[IslandPackageAnchor],
) -> Result<usize, MatchPackagesError> {
    ensure_package_island_anchors_table(connection)?;

    let transaction = connection
        .transaction()
        .map_err(MatchPackagesError::WriteAttribution)?;
    transaction
        .execute(
            "DELETE FROM package_island_anchors WHERE project_id = ?1",
            params![project_id],
        )
        .map_err(MatchPackagesError::WriteAttribution)?;

    let mut written = 0;
    for anchor in anchors {
        transaction
            .execute(
                r"
                INSERT INTO package_island_anchors
                    (project_id, source_file_id, binding_name, package_name,
                     package_version, export_specifier, function_span_start,
                     function_span_end, tier, external_importable,
                     top_score, runner_up_score, margin, created_at, updated_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                        datetime('now'), datetime('now'))
                ON CONFLICT(project_id, source_file_id, binding_name) DO UPDATE SET
                    package_name = excluded.package_name,
                    package_version = excluded.package_version,
                    export_specifier = excluded.export_specifier,
                    function_span_start = excluded.function_span_start,
                    function_span_end = excluded.function_span_end,
                    tier = excluded.tier,
                    external_importable = excluded.external_importable,
                    top_score = excluded.top_score,
                    runner_up_score = excluded.runner_up_score,
                    margin = excluded.margin,
                    updated_at = datetime('now')
                ",
                params![
                    project_id,
                    i64::from(anchor.source_file_id),
                    anchor.binding_name,
                    anchor.package_name,
                    anchor.package_version,
                    anchor.export_specifier,
                    i64::from(anchor.function_span_start),
                    i64::from(anchor.function_span_end),
                    anchor.tier,
                    i64::from(anchor.external_importable),
                    anchor.top_score,
                    anchor.runner_up_score,
                    anchor.margin,
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

fn ensure_package_island_anchors_table(
    connection: &mut Connection,
) -> Result<(), MatchPackagesError> {
    connection
        .execute_batch(PACKAGE_ISLAND_ANCHORS_CREATE_SQL)
        .map_err(MatchPackagesError::WriteAttribution)?;
    connection
        .execute_batch(PACKAGE_ISLAND_ANCHORS_INDEX_SQL)
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(())
}

const PACKAGE_ISLAND_ANCHORS_CREATE_SQL: &str = r"
CREATE TABLE IF NOT EXISTS package_island_anchors (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL,
    source_file_id INTEGER NOT NULL,
    binding_name TEXT NOT NULL,
    package_name TEXT NOT NULL,
    package_version TEXT NOT NULL,
    export_specifier TEXT NOT NULL,
    function_span_start INTEGER NOT NULL,
    function_span_end INTEGER NOT NULL,
    tier TEXT NOT NULL,
    external_importable INTEGER NOT NULL,
    top_score REAL NOT NULL,
    runner_up_score REAL NOT NULL,
    margin REAL NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (project_id, source_file_id, binding_name),
    CHECK (TRIM(binding_name) != ''),
    CHECK (TRIM(package_name) != ''),
    CHECK (TRIM(package_version) != ''),
    CHECK (TRIM(export_specifier) != ''),
    CHECK (function_span_start <= function_span_end),
    CHECK (external_importable IN (0, 1)),
    CHECK (margin >= 0.0 AND margin <= 1.0)
);
";

const PACKAGE_ISLAND_ANCHORS_INDEX_SQL: &str = r"
CREATE INDEX IF NOT EXISTS idx_package_island_anchors_project
    ON package_island_anchors(project_id);
CREATE INDEX IF NOT EXISTS idx_package_island_anchors_package
    ON package_island_anchors(package_name, package_version);
";

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor(source_file_id: u32, binding: &str, package: &str) -> IslandPackageAnchor {
        IslandPackageAnchor {
            source_file_id,
            binding_name: binding.to_string(),
            package_name: package.to_string(),
            package_version: "3.25.64".to_string(),
            export_specifier: package.to_string(),
            function_span_start: 100,
            function_span_end: 250,
            tier: "structural_anchored".to_string(),
            external_importable: true,
            top_score: 0.9,
            runner_up_score: 0.2,
            margin: 0.7,
        }
    }

    /// (binding_name, package_name) rows for a project, in stable order.
    fn anchored(connection: &Connection, project_id: i64) -> Vec<(String, String)> {
        let mut statement = connection
            .prepare(
                "SELECT binding_name, package_name FROM package_island_anchors \
                 WHERE project_id = ?1 ORDER BY source_file_id, binding_name",
            )
            .unwrap();
        statement
            .query_map(params![project_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn persists_island_anchors_scoped_to_their_project() {
        let mut connection = Connection::open_in_memory().unwrap();
        let anchors = vec![anchor(1, "Cb", "zod"), anchor(1, "Dx", "zod")];

        let written = persist_island_anchors(&mut connection, 7, &anchors).unwrap();
        assert_eq!(written, 2);

        assert_eq!(
            anchored(&connection, 7),
            vec![
                ("Cb".to_string(), "zod".to_string()),
                ("Dx".to_string(), "zod".to_string()),
            ]
        );
        // A different project sees nothing.
        assert!(anchored(&connection, 8).is_empty());
    }

    #[test]
    fn re_persisting_supersedes_the_previous_set() {
        let mut connection = Connection::open_in_memory().unwrap();
        persist_island_anchors(
            &mut connection,
            7,
            &[anchor(1, "Cb", "zod"), anchor(1, "Dx", "zod")],
        )
        .unwrap();

        // A re-run with a smaller, corrected set fully replaces the prior rows.
        let written =
            persist_island_anchors(&mut connection, 7, &[anchor(1, "Cb", "semver")]).unwrap();
        assert_eq!(written, 1);

        assert_eq!(
            anchored(&connection, 7),
            vec![("Cb".to_string(), "semver".to_string())]
        );
    }
}
