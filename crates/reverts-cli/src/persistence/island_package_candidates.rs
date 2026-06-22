//! Persist Agent-proposed third-party package names for the eager entry island
//! into `island_package_candidates`.
//!
//! The eager island inlines whole libraries with no module of their own, so the
//! deterministic matcher cannot even know which packages to fetch. An Agent
//! reads the island's evidence (string anchors, API shapes) and PROPOSES package
//! names; this table records those proposals. `match-packages` then treats an
//! accepted proposal as a materialization seed and the deterministic
//! fingerprint cascade confirms it — a wrong guess simply fails to match and
//! produces no anchor, so the Agent's judgement never bypasses the proof.
//!
//! This module owns the table SQL and its reader/writer.

use rusqlite::{Connection, params};

use crate::errors::MatchPackagesError;

/// Status of an Agent's island package-name proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IslandPackageCandidateStatus {
    Accepted,
    Rejected,
}

impl IslandPackageCandidateStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }
}

/// An Agent-proposed package name for the island, with an optional version hint
/// and the evidence that motivated it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IslandPackageCandidate {
    pub(crate) package_name: String,
    /// Optional version specifier the Agent believes applies (e.g. `^3.25.0`).
    /// `None` lets the matcher resolve `latest`.
    pub(crate) version_hint: Option<String>,
    pub(crate) evidence: String,
}

/// Insert or update one Agent proposal at the given status.
pub(crate) fn persist_island_package_candidate(
    connection: &mut Connection,
    project_id: i64,
    candidate: &IslandPackageCandidate,
    status: IslandPackageCandidateStatus,
) -> Result<(), MatchPackagesError> {
    ensure_table(connection)?;
    connection
        .execute(
            r"
            INSERT INTO island_package_candidates
                (project_id, package_name, version_hint, status, evidence,
                 created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'), datetime('now'))
            ON CONFLICT(project_id, package_name) DO UPDATE SET
                version_hint = excluded.version_hint,
                status = excluded.status,
                evidence = excluded.evidence,
                updated_at = datetime('now')
            ",
            params![
                project_id,
                candidate.package_name,
                candidate.version_hint,
                status.as_str(),
                candidate.evidence,
            ],
        )
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(())
}

/// Accepted Agent-proposed package names (with version hints) for a project, in
/// stable order. A pre-feature database (no table) yields nothing.
pub(crate) fn load_accepted_island_package_candidates(
    connection: &Connection,
    project_id: i64,
) -> Result<Vec<IslandPackageCandidate>, MatchPackagesError> {
    if !table_exists(connection)? {
        return Ok(Vec::new());
    }
    let mut statement = connection
        .prepare(
            r"
            SELECT package_name, version_hint, evidence
            FROM island_package_candidates
            WHERE project_id = ?1 AND status = 'accepted'
            ORDER BY package_name
            ",
        )
        .map_err(MatchPackagesError::WriteAttribution)?;
    let rows = statement
        .query_map(params![project_id], |row| {
            Ok(IslandPackageCandidate {
                package_name: row.get(0)?,
                version_hint: row.get::<_, Option<String>>(1)?,
                evidence: row.get(2)?,
            })
        })
        .map_err(MatchPackagesError::WriteAttribution)?;
    let mut candidates = Vec::new();
    for row in rows {
        candidates.push(row.map_err(MatchPackagesError::WriteAttribution)?);
    }
    Ok(candidates)
}

fn ensure_table(connection: &mut Connection) -> Result<(), MatchPackagesError> {
    connection
        .execute_batch(ISLAND_PACKAGE_CANDIDATES_CREATE_SQL)
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(())
}

fn table_exists(connection: &Connection) -> Result<bool, MatchPackagesError> {
    let exists: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='island_package_candidates'",
            [],
            |row| row.get(0),
        )
        .map_err(MatchPackagesError::WriteAttribution)?;
    Ok(exists > 0)
}

const ISLAND_PACKAGE_CANDIDATES_CREATE_SQL: &str = r"
CREATE TABLE IF NOT EXISTS island_package_candidates (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL,
    package_name TEXT NOT NULL,
    version_hint TEXT,
    status TEXT NOT NULL,
    evidence TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (project_id, package_name),
    CHECK (TRIM(package_name) != ''),
    CHECK (status IN ('accepted', 'rejected'))
);
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_candidates_round_trip_and_rejected_are_excluded() {
        let mut connection = Connection::open_in_memory().expect("open db");
        persist_island_package_candidate(
            &mut connection,
            7,
            &IslandPackageCandidate {
                package_name: "zod".to_string(),
                version_hint: Some("^3.25.64".to_string()),
                evidence: "ZodError, parseAsync string anchors in island".to_string(),
            },
            IslandPackageCandidateStatus::Accepted,
        )
        .expect("persist");
        persist_island_package_candidate(
            &mut connection,
            7,
            &IslandPackageCandidate {
                package_name: "left-pad".to_string(),
                version_hint: None,
                evidence: "weak guess".to_string(),
            },
            IslandPackageCandidateStatus::Rejected,
        )
        .expect("persist");

        let accepted = load_accepted_island_package_candidates(&connection, 7).expect("load");
        assert_eq!(accepted.len(), 1, "only accepted: {accepted:?}");
        assert_eq!(accepted[0].package_name, "zod");
        assert_eq!(accepted[0].version_hint.as_deref(), Some("^3.25.64"));
        // Other projects see nothing.
        assert!(
            load_accepted_island_package_candidates(&connection, 8)
                .expect("load")
                .is_empty()
        );
    }

    #[test]
    fn re_accepting_updates_version_and_status() {
        let mut connection = Connection::open_in_memory().expect("open db");
        let mut candidate = IslandPackageCandidate {
            package_name: "react".to_string(),
            version_hint: None,
            evidence: "createElement, useState".to_string(),
        };
        persist_island_package_candidate(
            &mut connection,
            1,
            &candidate,
            IslandPackageCandidateStatus::Rejected,
        )
        .expect("persist");
        assert!(
            load_accepted_island_package_candidates(&connection, 1)
                .expect("load")
                .is_empty()
        );

        candidate.version_hint = Some("18.2.0".to_string());
        persist_island_package_candidate(
            &mut connection,
            1,
            &candidate,
            IslandPackageCandidateStatus::Accepted,
        )
        .expect("persist");
        let accepted = load_accepted_island_package_candidates(&connection, 1).expect("load");
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].version_hint.as_deref(), Some("18.2.0"));
    }

    #[test]
    fn load_on_fresh_database_is_empty() {
        let connection = Connection::open_in_memory().expect("open db");
        assert!(
            load_accepted_island_package_candidates(&connection, 1)
                .expect("load")
                .is_empty()
        );
    }
}
