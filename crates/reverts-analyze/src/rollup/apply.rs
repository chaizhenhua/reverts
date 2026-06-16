//! Materialize oracle verdicts into a `package_attributions` table.
//!
//! `apply_rollup_projections` runs inside a caller-supplied transaction and:
//!
//! 1. Flips every `status='rejected'` row whose module the oracle judges
//!    externalizable (via [`crate::rollup::projection::project`]) to
//!    `status='accepted'`, `emission_mode='external_import'`, stamped with the
//!    current [`reverts_input::PACKAGE_ATTRIBUTION_EXTERNAL_IMPORT_POLICY_VERSION`].
//! 2. Backfills `package_surfaces` rows for every accepted external-import
//!    attribution that lacks a matching `(project_id, export_specifier)` row
//!    — covering both rows we just flipped and any pre-existing accepted
//!    external imports whose surfaces were never persisted.
//!
//! Both writes are `INSERT OR IGNORE` / status-guarded `UPDATE` so the
//! operation is idempotent: re-running after a successful apply touches zero
//! rows. The function returns `(attributions_updated, surfaces_inserted)`.

use std::error::Error;
use std::fmt;

use reverts_input::PACKAGE_ATTRIBUTION_EXTERNAL_IMPORT_POLICY_VERSION;
use rusqlite::{Transaction, params};

use crate::rollup::db::Snapshot;
use crate::rollup::oracle::{Oracle, OracleVerdict};
use crate::rollup::projection::{ProjectionKind, project};

/// A single rolled-up projection ready to be materialized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollupRow {
    pub module_id: i64,
    pub package_name: String,
    pub package_version: String,
    pub top_specifier: String,
}

/// Summary of what `apply_rollup_projections` wrote inside the transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ApplyOutcome {
    pub attributions_updated: usize,
    pub surfaces_inserted: usize,
    pub candidate_modules: usize,
}

#[derive(Debug)]
pub enum ApplyError {
    OracleVerdictRegression {
        package_name: String,
        package_version: String,
    },
    Sqlite(rusqlite::Error),
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApplyError::OracleVerdictRegression {
                package_name,
                package_version,
            } => write!(
                f,
                "oracle verdict regression for {package_name}@{package_version}: \
                 projection produced a rollup candidate but the oracle no longer reports \
                 it as externalizable; the snapshot and oracle disagree"
            ),
            ApplyError::Sqlite(e) => write!(f, "sqlite error: {e}"),
        }
    }
}

impl Error for ApplyError {}

impl From<rusqlite::Error> for ApplyError {
    fn from(e: rusqlite::Error) -> Self {
        ApplyError::Sqlite(e)
    }
}

/// Collect every projected rolled-up row that has a concrete version.
/// Modules without a recorded `package_version` are skipped because the
/// `package_attributions` schema requires `package_version` to be non-empty
/// when `status='accepted'`.
#[must_use]
pub fn collect_rollups(snapshot: &Snapshot, oracle: &Oracle) -> Vec<RollupRow> {
    let mut out = Vec::new();
    for proj in project(snapshot, oracle) {
        if let ProjectionKind::RolledUp { top_specifier } = proj.kind
            && let Some(version) = proj.package_version
        {
            out.push(RollupRow {
                module_id: proj.module_id,
                package_name: proj.package_name,
                package_version: version,
                top_specifier,
            });
        }
    }
    out
}

/// Apply the rollup projections to `package_attributions` and backfill
/// `package_surfaces`. The caller owns the transaction and is responsible
/// for committing or rolling back.
///
/// `now_iso8601` is the timestamp string to stamp into the `updated_at`
/// column and into surface `created_at`/`updated_at`; callers should pass
/// an ISO-8601 UTC string so the format matches the rest of the table.
pub fn apply_rollup_projections(
    tx: &Transaction<'_>,
    snapshot: &Snapshot,
    oracle: &Oracle,
    now_iso8601: &str,
) -> Result<ApplyOutcome, ApplyError> {
    let plan = collect_rollups(snapshot, oracle);

    // Sanity-check: every row in `plan` came from a `ProjectionKind::RolledUp`
    // verdict, which means the oracle reported `Externalizable` at projection
    // time. Re-query here so a future refactor that decouples `project()`
    // from the oracle can't silently smuggle in non-externalizable rows.
    for row in &plan {
        match oracle.lookup(&row.package_name, &row.package_version) {
            Some(OracleVerdict::Externalizable { .. }) => {}
            _ => {
                return Err(ApplyError::OracleVerdictRegression {
                    package_name: row.package_name.clone(),
                    package_version: row.package_version.clone(),
                });
            }
        }
    }

    let mut stmt = tx.prepare(
        "UPDATE package_attributions
         SET status='accepted',
             emission_mode='external_import',
             export_specifier=?1,
             package_version=?2,
             package_subpath=NULL,
             resolved_file=NULL,
             rejection_reason=NULL,
             external_import_policy_version=?3,
             evidence_json=COALESCE(evidence_json,'{}'),
             updated_at=?4
         WHERE module_id=?5
           AND status='rejected'",
    )?;
    let mut attributions_updated = 0usize;
    for row in &plan {
        let n = stmt.execute(params![
            row.top_specifier,
            row.package_version,
            PACKAGE_ATTRIBUTION_EXTERNAL_IMPORT_POLICY_VERSION,
            now_iso8601,
            row.module_id,
        ])?;
        attributions_updated += n;
    }
    drop(stmt);

    let evidence = format!(
        "{{\"matcher\":\"rollup_apply\",\"policy_version\":{}}}",
        PACKAGE_ATTRIBUTION_EXTERNAL_IMPORT_POLICY_VERSION
    );
    let surfaces_inserted = tx.execute(
        "INSERT OR IGNORE INTO package_surfaces
            (project_id, package_name, package_version, export_specifier, status,
             evidence_json, created_at, updated_at)
         SELECT DISTINCT pf.project_id,
                pa.package_name,
                COALESCE(pa.package_version, '*'),
                pa.export_specifier,
                'accepted',
                ?1,
                ?2, ?2
         FROM package_attributions pa
         JOIN modules m ON m.id = pa.module_id
         JOIN project_files pf ON pf.file_id = m.file_id
         WHERE pa.status='accepted'
           AND pa.emission_mode='external_import'
           AND pa.export_specifier IS NOT NULL
           AND TRIM(pa.export_specifier) != ''",
        params![evidence, now_iso8601],
    )?;

    Ok(ApplyOutcome {
        attributions_updated,
        surfaces_inserted,
        candidate_modules: plan.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rollup::db::load_snapshot;
    use crate::rollup::oracle::{OracleConfig, build_oracle};
    use rusqlite::Connection;

    /// Build the bare-minimum schema this module touches: `package_attributions`,
    /// `package_surfaces`, `modules`, `project_files`, plus the externalization-
    /// hints / projects / source_files tables `load_snapshot` reads.
    fn minimal_schema(conn: &Connection) {
        conn.execute_batch(
            r"
            CREATE TABLE projects (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL
            );
            CREATE TABLE source_files (
                id INTEGER PRIMARY KEY,
                file_path TEXT NOT NULL
            );
            CREATE TABLE modules (
                id INTEGER PRIMARY KEY,
                file_id INTEGER,
                original_name TEXT NOT NULL,
                module_category TEXT NOT NULL DEFAULT 'package',
                package_name TEXT,
                package_version TEXT
            );
            CREATE TABLE project_files (
                project_id INTEGER NOT NULL,
                file_id INTEGER NOT NULL,
                PRIMARY KEY (project_id, file_id)
            );
            CREATE TABLE package_attributions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                module_id INTEGER NOT NULL UNIQUE,
                module_original_name TEXT NOT NULL DEFAULT 'm',
                package_name TEXT NOT NULL,
                package_version TEXT,
                package_subpath TEXT,
                resolved_file TEXT,
                export_specifier TEXT,
                emission_mode TEXT NOT NULL,
                status TEXT NOT NULL,
                evidence_json TEXT,
                rejection_reason TEXT,
                external_import_policy_version INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT '',
                updated_at TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE package_surfaces (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                project_id INTEGER NOT NULL,
                package_name TEXT NOT NULL,
                package_version TEXT NOT NULL,
                export_specifier TEXT NOT NULL,
                status TEXT NOT NULL,
                evidence_json TEXT,
                created_at TEXT NOT NULL DEFAULT '',
                updated_at TEXT NOT NULL DEFAULT '',
                UNIQUE (project_id, export_specifier)
            );
            CREATE TABLE package_externalization_hints (
                package_name TEXT NOT NULL,
                package_version TEXT NOT NULL,
                export_specifier TEXT NOT NULL,
                public_members_json TEXT,
                PRIMARY KEY (package_name, package_version, export_specifier)
            );
            ",
        )
        .expect("schema");
    }

    /// Seed one project with:
    /// - A top-level module for `lodash` that is **already accepted external_import**
    ///   (provides the "top-level hint" the projection needs)
    /// - Two closure-owned `lodash` modules currently rejected as `application_source`
    fn seed_lodash_project(conn: &Connection) {
        conn.execute_batch(
            r#"
            INSERT INTO projects (id, name) VALUES (1, 'fixture');
            INSERT INTO source_files (id, file_path) VALUES (10, '/tmp/lodash.js');
            INSERT INTO project_files (project_id, file_id) VALUES (1, 10);

            INSERT INTO modules (id, file_id, original_name, module_category, package_name, package_version)
            VALUES
                (100, 10, 'lodash/index', 'package', 'lodash', '4.17.21'),
                (101, 10, 'lodash/_baseMerge', 'package', 'lodash', '4.17.21'),
                (102, 10, 'lodash/_baseClone', 'package', 'lodash', '4.17.21');

            INSERT INTO package_attributions
                (module_id, module_original_name, package_name, package_version, export_specifier,
                 emission_mode, status, evidence_json,
                 external_import_policy_version, created_at, updated_at)
            VALUES
                (100, 'lodash/index', 'lodash', '4.17.21', 'lodash', 'external_import', 'accepted',
                 '{"external_import_proof":"matched_package_source"}', 1, 'now', 'now'),
                (101, 'lodash/_baseMerge', 'lodash', '4.17.21', NULL, 'application_source', 'rejected',
                 '{"match_strategy":"dependency_closure_ownership","external_importable":false}',
                 0, 'now', 'now'),
                (102, 'lodash/_baseClone', 'lodash', '4.17.21', NULL, 'application_source', 'rejected',
                 '{"match_strategy":"dependency_closure_ownership","external_importable":false}',
                 0, 'now', 'now');
            UPDATE package_attributions SET rejection_reason='closure-owned' WHERE status='rejected';

            -- Oracle externalizability depends on a top-level hint
            -- (export_specifier == package_name) for the (name, version) pair.
            -- Hint MUST enumerate at least one public member or the oracle
            -- now rejects externalization as "planner would still adapter-wrap"
            -- — see crates/reverts-analyze/src/rollup/oracle.rs.
            INSERT INTO package_externalization_hints
                (package_name, package_version, export_specifier, public_members_json)
            VALUES ('lodash', '4.17.21', 'lodash', '["merge","cloneDeep","_root"]');
            "#,
        )
        .expect("seed");
    }

    fn run_apply(conn: &mut Connection) -> ApplyOutcome {
        let snapshot = load_snapshot(conn).expect("load snapshot");
        let oracle = build_oracle(&snapshot, OracleConfig::default());
        let tx = conn.transaction().expect("begin tx");
        let outcome = apply_rollup_projections(&tx, &snapshot, &oracle, "2025-01-01T00:00:00Z")
            .expect("apply ok");
        tx.commit().expect("commit");
        outcome
    }

    #[test]
    fn applies_flips_rejected_closure_modules_to_accepted_external_import() {
        let mut conn = Connection::open_in_memory().unwrap();
        minimal_schema(&conn);
        seed_lodash_project(&conn);

        let outcome = run_apply(&mut conn);

        assert_eq!(outcome.attributions_updated, 2, "both closure rows flip");
        let accepted_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM package_attributions
                 WHERE status='accepted' AND emission_mode='external_import'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(accepted_count, 3, "1 pre-existing + 2 newly flipped");

        let policy: i64 = conn
            .query_row(
                "SELECT external_import_policy_version FROM package_attributions WHERE module_id=101",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            policy, PACKAGE_ATTRIBUTION_EXTERNAL_IMPORT_POLICY_VERSION,
            "flipped row must stamp the current policy constant"
        );
    }

    #[test]
    fn applies_backfills_a_surface_row_for_the_top_level_specifier() {
        let mut conn = Connection::open_in_memory().unwrap();
        minimal_schema(&conn);
        seed_lodash_project(&conn);

        let outcome = run_apply(&mut conn);

        // 3 accepted external-import attributions all point at the same
        // (project=1, specifier='lodash') pair → 1 surface row.
        assert_eq!(outcome.surfaces_inserted, 1);
        let surface_specifier: String = conn
            .query_row(
                "SELECT export_specifier FROM package_surfaces WHERE project_id=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(surface_specifier, "lodash");
    }

    #[test]
    fn applies_is_idempotent_when_rerun_against_the_already_flipped_database() {
        let mut conn = Connection::open_in_memory().unwrap();
        minimal_schema(&conn);
        seed_lodash_project(&conn);

        let first = run_apply(&mut conn);
        let second = run_apply(&mut conn);

        assert_eq!(first.attributions_updated, 2);
        assert_eq!(
            second.attributions_updated, 0,
            "no rejected rows left to flip"
        );
        assert_eq!(second.surfaces_inserted, 0, "INSERT OR IGNORE skips dupes");
    }

    #[test]
    fn applies_does_not_touch_accepted_rows_outside_the_rollup_plan() {
        let mut conn = Connection::open_in_memory().unwrap();
        minimal_schema(&conn);
        seed_lodash_project(&conn);
        // Add a second project with an already-accepted external attribution for `react`.
        conn.execute_batch(
            r"
            INSERT INTO source_files (id, file_path) VALUES (20, '/tmp/react.js');
            INSERT INTO modules (id, file_id, original_name, module_category, package_name, package_version)
                VALUES (200, 20, 'react/index', 'package', 'react', '18.0.0');
            INSERT INTO projects (id, name) VALUES (2, 'fixture-react');
            INSERT INTO project_files (project_id, file_id) VALUES (2, 20);
            INSERT INTO package_attributions
                (module_id, module_original_name, package_name, package_version, export_specifier,
                 emission_mode, status, external_import_policy_version, created_at, updated_at)
            VALUES
                (200, 'react/index', 'react', '18.0.0', 'react', 'external_import', 'accepted', 1, 'now', 'now');
            ",
        )
        .unwrap();

        let timestamp_before: String = conn
            .query_row(
                "SELECT updated_at FROM package_attributions WHERE module_id=200",
                [],
                |r| r.get(0),
            )
            .unwrap();
        run_apply(&mut conn);
        let timestamp_after: String = conn
            .query_row(
                "SELECT updated_at FROM package_attributions WHERE module_id=200",
                [],
                |r| r.get(0),
            )
            .unwrap();

        assert_eq!(
            timestamp_before, timestamp_after,
            "an already-accepted row outside the rollup plan must not be re-stamped"
        );
        // But the surface backfill MAY add a row for react if none existed — verify
        // that's the only delta:
        let react_surface_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM package_surfaces WHERE project_id=2",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            react_surface_count, 1,
            "surface backfilled for pre-existing accepted row"
        );
    }
}
