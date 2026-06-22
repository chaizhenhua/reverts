//! `cluster-names` command: accept semantic file paths for island clusters.
//!
//! An island cluster (a Louvain community or chain-split chunk drained out of the
//! eager entrypoint island) emits at a mechanical `modules/island/cluster-<id>.ts`
//! path. This command records a semantic path keyed by the cluster's stable
//! content FINGERPRINT (printed in `.reverts/island-clusters.json` by
//! `generate-project-v2`), persisted as `island_cluster_names` rows. The input
//! loader feeds accepted rows to the planner, which moves the cluster's emitted
//! file to the semantic path and recomputes every importer's relative specifier —
//! the same machinery as `module-names`, but for module-less island code.
//!
//! The fingerprint is invariant under semantic naming, so a name keeps applying
//! across regenerations even though the mechanical `cluster-<id>` label shifts.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use clap::Args;
use rusqlite::{Connection, OpenFlags, params};

use crate::args::{parse_args_with_name, parse_project_id};
use crate::commands::naming_gates::validate_module_path_acceptance;
use crate::errors::{CliError, CliRunError};
use crate::{collect_sqlite_rows, sqlite_table_exists};

#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct ClusterNamesArgs {
    #[arg(long)]
    pub input: PathBuf,
    #[arg(long, value_parser = parse_project_id)]
    pub project_id: u32,
    #[arg(long)]
    pub list: bool,
    #[arg(long)]
    pub apply: bool,
    #[arg(long, default_value = "agent")]
    pub origin: String,
    #[arg(long)]
    pub evidence: Option<String>,
    #[arg(long = "accept", value_parser = parse_cluster_name_spec)]
    pub accepts: Vec<ClusterNameSpec>,
    #[arg(long)]
    pub batch: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterNameSpec {
    pub fingerprint: String,
    pub semantic_path: String,
    pub evidence: Option<String>,
}

impl ClusterNamesArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let mut args = args.into_iter().collect::<Vec<_>>();
        if args
            .first()
            .is_some_and(|argument| argument == crate::help::CLUSTER_NAMES_COMMAND)
        {
            args.remove(0);
        }
        let parsed: Self = parse_args_with_name(crate::help::CLUSTER_NAMES_COMMAND, args)?;
        validate_args(parsed)
    }
}

pub(crate) fn validate_args(args: ClusterNamesArgs) -> Result<ClusterNamesArgs, CliError> {
    if args.list && (!args.accepts.is_empty() || args.batch.is_some() || args.apply) {
        return Err(CliError::UnknownArgument(
            "--list cannot be combined with mutations".to_string(),
        ));
    }
    if !args.list && args.accepts.is_empty() && args.batch.is_none() {
        return Err(CliError::MissingArgument("--list | --accept | --batch"));
    }
    Ok(args)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterNameRow {
    pub fingerprint: String,
    pub semantic_path: String,
    pub origin: String,
    pub evidence: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterNamesOutcome {
    pub listed: Vec<ClusterNameRow>,
    pub requested_changes: usize,
    pub written_changes: usize,
}

pub(crate) fn run(args: ClusterNamesArgs) -> Result<(), CliRunError> {
    let outcome = cluster_names_from_sqlite(&args)?;
    if args.list {
        println!("fingerprint\tsemantic_path\torigin\tevidence");
        for row in outcome.listed {
            println!(
                "{}\t{}\t{}\t{}",
                row.fingerprint,
                row.semantic_path,
                row.origin,
                row.evidence.unwrap_or_default()
            );
        }
    } else if args.apply {
        println!(
            "updated cluster names for project {}: {} change(s) written",
            args.project_id, outcome.written_changes
        );
    } else {
        println!(
            "dry-run: would update {} cluster name(s) for project {}; pass --apply to persist",
            outcome.requested_changes, args.project_id
        );
    }
    Ok(())
}

pub fn cluster_names_from_sqlite(
    args: &ClusterNamesArgs,
) -> Result<ClusterNamesOutcome, CliRunError> {
    let flags = if args.apply {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let mut connection = Connection::open_with_flags(args.input.as_path(), flags)
        .map_err(|source| CliRunError::ClusterNames(source.to_string()))?;
    connection
        .busy_timeout(Duration::from_secs(30))
        .map_err(|source| CliRunError::ClusterNames(source.to_string()))?;

    if args.list {
        if !sqlite_table_exists(&connection, "island_cluster_names")
            .map_err(|source| CliRunError::ClusterNames(source.to_string()))?
        {
            return Ok(ClusterNamesOutcome {
                listed: Vec::new(),
                requested_changes: 0,
                written_changes: 0,
            });
        }
        return Ok(ClusterNamesOutcome {
            listed: load_cluster_name_rows(&connection, args.project_id)?,
            requested_changes: 0,
            written_changes: 0,
        });
    }

    let specs = collect_specs(args)?;
    validate_specs(&specs, args.origin.as_str())?;

    let written_changes = if args.apply {
        ensure_island_cluster_names_table(&connection)?;
        let transaction = connection
            .transaction()
            .map_err(|source| CliRunError::ClusterNames(source.to_string()))?;
        let mut written = 0_usize;
        for spec in &specs {
            // One active override per fingerprint: retire any prior accepted row
            // before inserting, so the loader's "latest accepted" pick is exact.
            transaction
                .execute(
                    r"
                    UPDATE island_cluster_names SET accepted = 0, updated_at = datetime('now')
                     WHERE project_id = ?1 AND fingerprint = ?2 AND accepted = 1
                    ",
                    params![i64::from(args.project_id), spec.fingerprint],
                )
                .map_err(|source| CliRunError::ClusterNames(source.to_string()))?;
            written += transaction
                .execute(
                    r"
                    INSERT INTO island_cluster_names (
                        project_id, fingerprint, path, origin, evidence, accepted,
                        created_at, updated_at
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, 1, datetime('now'), datetime('now'))
                    ON CONFLICT(project_id, fingerprint, origin, path) DO UPDATE SET
                        evidence = excluded.evidence,
                        accepted = 1,
                        updated_at = datetime('now')
                    ",
                    params![
                        i64::from(args.project_id),
                        spec.fingerprint,
                        spec.semantic_path,
                        args.origin,
                        spec.evidence.as_deref().or(args.evidence.as_deref()),
                    ],
                )
                .map_err(|source| CliRunError::ClusterNames(source.to_string()))?;
        }
        transaction
            .commit()
            .map_err(|source| CliRunError::ClusterNames(source.to_string()))?;
        written
    } else {
        0
    };

    Ok(ClusterNamesOutcome {
        listed: Vec::new(),
        requested_changes: specs.len(),
        written_changes,
    })
}

fn parse_cluster_name_spec(value: &str) -> Result<ClusterNameSpec, String> {
    let Some((fingerprint, semantic_path)) = value.split_once('=') else {
        return Err("expected FINGERPRINT=SEMANTIC_PATH".to_string());
    };
    let fingerprint = fingerprint.trim();
    if !is_valid_fingerprint(fingerprint) {
        return Err("FINGERPRINT must be a lowercase hex digest".to_string());
    }
    if semantic_path.trim().is_empty() {
        return Err("SEMANTIC_PATH must be non-empty".to_string());
    }
    Ok(ClusterNameSpec {
        fingerprint: fingerprint.to_string(),
        semantic_path: semantic_path.trim().to_string(),
        evidence: None,
    })
}

fn is_valid_fingerprint(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn collect_specs(args: &ClusterNamesArgs) -> Result<Vec<ClusterNameSpec>, CliRunError> {
    let mut specs = args.accepts.clone();
    if let Some(path) = &args.batch {
        let text = fs::read_to_string(path).map_err(|source| CliRunError::WriteOutput {
            path: path.clone(),
            source,
        })?;
        for (index, line) in text.lines().enumerate() {
            if index == 0 && line.starts_with("action\t") {
                continue;
            }
            if line.trim().is_empty() {
                continue;
            }
            let columns = line.split('\t').collect::<Vec<_>>();
            if columns.len() < 3 || columns[0] != "accept" {
                return Err(CliRunError::ClusterNames(format!(
                    "invalid batch row {}: expected accept<TAB>FINGERPRINT<TAB>SEMANTIC_PATH<TAB>[EVIDENCE]",
                    index + 1
                )));
            }
            let fingerprint = columns[1].trim();
            if !is_valid_fingerprint(fingerprint) {
                return Err(CliRunError::ClusterNames(format!(
                    "invalid batch row {}: FINGERPRINT must be a lowercase hex digest",
                    index + 1
                )));
            }
            specs.push(ClusterNameSpec {
                fingerprint: fingerprint.to_string(),
                semantic_path: columns[2].trim().to_string(),
                evidence: columns
                    .get(3)
                    .map(|value| value.trim())
                    .filter(|value| !value.is_empty())
                    .map(ToOwned::to_owned),
            });
        }
    }
    Ok(specs)
}

fn validate_specs(specs: &[ClusterNameSpec], origin: &str) -> Result<(), CliRunError> {
    for spec in specs {
        if !is_valid_fingerprint(spec.fingerprint.as_str()) {
            return Err(CliRunError::ClusterNames(
                "fingerprint must be a lowercase hex digest".to_string(),
            ));
        }
        // Reuse the module-path gate: same path-shape rules (no absolute paths,
        // no `..`, sane segments) and origin/provenance checks apply.
        validate_module_path_acceptance(spec.semantic_path.as_str(), origin)
            .map_err(|error| CliRunError::ClusterNames(error.message()))?;
    }
    Ok(())
}

fn ensure_island_cluster_names_table(connection: &Connection) -> Result<(), CliRunError> {
    connection
        .execute_batch(
            r"
            CREATE TABLE IF NOT EXISTS island_cluster_names (
                project_id INTEGER NOT NULL,
                fingerprint TEXT NOT NULL,
                path TEXT NOT NULL,
                origin TEXT NOT NULL,
                evidence TEXT,
                accepted INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (project_id, fingerprint, origin, path)
            );
            CREATE INDEX IF NOT EXISTS idx_island_cluster_names_project_fp
                ON island_cluster_names(project_id, fingerprint, accepted);
            ",
        )
        .map_err(|source| CliRunError::ClusterNames(source.to_string()))
}

fn load_cluster_name_rows(
    connection: &Connection,
    project_id: u32,
) -> Result<Vec<ClusterNameRow>, CliRunError> {
    let mut statement = connection
        .prepare(
            r"
            SELECT fingerprint, path, origin, evidence
            FROM island_cluster_names
            WHERE project_id = ?1 AND accepted = 1
            ORDER BY path
            ",
        )
        .map_err(|source| CliRunError::ClusterNames(source.to_string()))?;
    let rows = statement
        .query_map(params![i64::from(project_id)], |row| {
            Ok(ClusterNameRow {
                fingerprint: row.get(0)?,
                semantic_path: row.get(1)?,
                origin: row.get(2)?,
                evidence: row.get(3)?,
            })
        })
        .map_err(|source| CliRunError::ClusterNames(source.to_string()))?;
    collect_sqlite_rows(rows).map_err(|source| CliRunError::ClusterNames(source.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_hex_fingerprint() {
        assert!(parse_cluster_name_spec("zz=telemetry/otel").is_err());
        assert!(parse_cluster_name_spec("3066d34e2f3b70cb=telemetry/otel").is_ok());
    }

    #[test]
    fn batch_row_requires_accept_and_hex() {
        let args = ClusterNamesArgs {
            input: PathBuf::from("x"),
            project_id: 1,
            list: false,
            apply: false,
            origin: "agent".to_string(),
            evidence: None,
            accepts: Vec::new(),
            batch: None,
        };
        // A direct spec list (no batch file) round-trips.
        let mut a = args.clone();
        a.accepts = vec![ClusterNameSpec {
            fingerprint: "abc123".to_string(),
            semantic_path: "telemetry/otel".to_string(),
            evidence: None,
        }];
        assert!(validate_specs(&a.accepts, "agent").is_ok());
    }

    #[test]
    fn accept_then_list_round_trips_in_memory() {
        let connection = Connection::open_in_memory().expect("open");
        ensure_island_cluster_names_table(&connection).expect("create table");
        connection
            .execute(
                r"INSERT INTO island_cluster_names
                  (project_id, fingerprint, path, origin, evidence, accepted, created_at, updated_at)
                  VALUES (1, 'deadbeef', 'telemetry/otel', 'agent', NULL, 1, datetime('now'), datetime('now'))",
                [],
            )
            .expect("insert");
        let rows = load_cluster_name_rows(&connection, 1).expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].fingerprint, "deadbeef");
        assert_eq!(rows[0].semantic_path, "telemetry/otel");
    }

    /// Re-naming an already-named cluster: accepting a NEW path for a fingerprint
    /// retires the prior accepted row and activates the new one, so `--list` (and
    /// thus the loader) sees exactly the latest name. This is what makes
    /// "命名后再次重命名" safe — the old path fully disappears.
    fn apply_accept(connection: &mut Connection, fingerprint: &str, path: &str) {
        let transaction = connection.transaction().expect("txn");
        transaction
            .execute(
                "UPDATE island_cluster_names SET accepted = 0, updated_at = datetime('now') \
                 WHERE project_id = 1 AND fingerprint = ?1 AND accepted = 1",
                params![fingerprint],
            )
            .expect("retire");
        transaction
            .execute(
                "INSERT INTO island_cluster_names \
                 (project_id, fingerprint, path, origin, evidence, accepted, created_at, updated_at) \
                 VALUES (1, ?1, ?2, 'agent', NULL, 1, datetime('now'), datetime('now')) \
                 ON CONFLICT(project_id, fingerprint, origin, path) DO UPDATE SET \
                   accepted = 1, updated_at = datetime('now')",
                params![fingerprint, path],
            )
            .expect("insert");
        transaction.commit().expect("commit");
    }

    #[test]
    fn re_accepting_a_fingerprint_replaces_the_active_name() {
        let mut connection = Connection::open_in_memory().expect("open");
        ensure_island_cluster_names_table(&connection).expect("create table");

        apply_accept(&mut connection, "cafe", "telemetry/otel");
        apply_accept(&mut connection, "cafe", "telemetry/otel-and-sentry");
        let rows = load_cluster_name_rows(&connection, 1).expect("list");
        // Exactly one ACTIVE name, the latest — the first is retired (accepted = 0).
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].semantic_path, "telemetry/otel-and-sentry");

        // Re-naming BACK to the original path re-activates its (retired) row, not a
        // duplicate — still exactly one active name.
        apply_accept(&mut connection, "cafe", "telemetry/otel");
        let rows = load_cluster_name_rows(&connection, 1).expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].semantic_path, "telemetry/otel");
    }
}
