//! `param-names` command: accept explicit semantic names for function PARAMETERS
//! of decompiled functions, keyed by (file, function name, parameter index).
//! Consumed by `generate` (via `semantic_function_param_names`) and applied by
//! the emitter's function-param pass before emission. Batch-only: the agent
//! naming workflow produces a TSV of accepts.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use clap::Args;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::args::{parse_args_with_name, parse_project_id};
use crate::commands::naming_gates::{NamingGateMode, validate_name_acceptance};
use crate::errors::{CliError, CliRunError};

#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct ParamNamesArgs {
    #[arg(long)]
    pub input: PathBuf,
    #[arg(long, value_parser = parse_project_id)]
    pub project_id: u32,
    #[arg(long)]
    pub apply: bool,
    #[arg(long, default_value = "agent")]
    pub origin: String,
    #[arg(long)]
    pub evidence: Option<String>,
    #[arg(long)]
    pub batch: PathBuf,
}

impl ParamNamesArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let mut args = args.into_iter().collect::<Vec<_>>();
        if args
            .first()
            .is_some_and(|argument| argument == crate::help::PARAM_NAMES_COMMAND)
        {
            args.remove(0);
        }
        parse_args_with_name(crate::help::PARAM_NAMES_COMMAND, args)
    }
}

struct ParamNameSpec {
    file_path: String,
    function_name: String,
    param_index: u32,
    semantic_name: String,
    evidence: Option<String>,
}

pub(crate) fn run(args: ParamNamesArgs) -> Result<(), CliRunError> {
    let (requested, written) = param_names_from_sqlite(&args)?;
    if args.apply {
        println!(
            "updated function-parameter names for project {}: {written} change(s) written",
            args.project_id
        );
    } else {
        println!(
            "dry-run: would update {requested} function-parameter name(s) for project {}; pass --apply to persist",
            args.project_id
        );
    }
    Ok(())
}

fn param_names_from_sqlite(args: &ParamNamesArgs) -> Result<(usize, usize), CliRunError> {
    let flags = if args.apply {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let connection = Connection::open_with_flags(args.input.as_path(), flags)
        .map_err(|source| CliRunError::ParamNames(source.to_string()))?;
    connection
        .busy_timeout(Duration::from_secs(30))
        .map_err(|source| CliRunError::ParamNames(source.to_string()))?;
    ensure_project_exists(&connection, args.project_id)?;

    let specs = parse_batch(args.batch.as_path())?;
    for spec in &specs {
        if spec.file_path.trim().is_empty()
            || spec.function_name.trim().is_empty()
            || spec.semantic_name.trim().is_empty()
        {
            return Err(CliRunError::ParamNames(
                "file_path, function_name, and semantic_name must be non-empty".to_string(),
            ));
        }
        validate_name_acceptance(
            // Parameters have no minified "original" to gate on, so pass an empty
            // original: that keeps `semantic != original`, which is what makes the
            // vocabulary gate actually run (it is skipped for unchanged names).
            "",
            spec.semantic_name.as_str(),
            args.origin.as_str(),
            spec.evidence.as_deref().or(args.evidence.as_deref()),
            NamingGateMode::LocalBinding,
        )
        .map_err(|error| CliRunError::ParamNames(error.message()))?;
    }

    if !args.apply {
        return Ok((specs.len(), 0));
    }
    ensure_table(&connection)?;
    let mut written = 0usize;
    for spec in &specs {
        written += connection
            .execute(
                r"
                INSERT INTO semantic_function_param_names (
                    project_id, file_path, function_name, param_index, semantic_name,
                    origin, accepted, created_at, updated_at
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, datetime('now'), datetime('now'))
                ON CONFLICT(project_id, file_path, function_name, param_index) DO UPDATE SET
                    semantic_name = excluded.semantic_name,
                    origin = excluded.origin,
                    accepted = 1,
                    updated_at = datetime('now')
                ",
                params![
                    i64::from(args.project_id),
                    spec.file_path,
                    spec.function_name,
                    i64::from(spec.param_index),
                    spec.semantic_name,
                    args.origin,
                ],
            )
            .map_err(|source| CliRunError::ParamNames(source.to_string()))?;
    }
    Ok((specs.len(), written))
}

fn parse_batch(path: &std::path::Path) -> Result<Vec<ParamNameSpec>, CliRunError> {
    let text = fs::read_to_string(path).map_err(|source| {
        CliRunError::ParamNames(format!("cannot read batch {}: {source}", path.display()))
    })?;
    let mut specs = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if index == 0 && line.starts_with("action\t") {
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        let cols = line.split('\t').collect::<Vec<_>>();
        if cols.len() < 5 || cols[0] != "accept" {
            return Err(CliRunError::ParamNames(format!(
                "invalid batch row {}: expected accept<TAB>FILE<TAB>FUNCTION<TAB>PARAM_INDEX<TAB>SEMANTIC<TAB>[EVIDENCE]",
                index + 1
            )));
        }
        let param_index = cols[3].parse::<u32>().map_err(|_| {
            CliRunError::ParamNames(format!(
                "invalid batch row {}: PARAM_INDEX must be a non-negative integer",
                index + 1
            ))
        })?;
        specs.push(ParamNameSpec {
            file_path: cols[1].to_string(),
            function_name: cols[2].to_string(),
            param_index,
            semantic_name: cols[4].to_string(),
            evidence: cols
                .get(5)
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned),
        });
    }
    Ok(specs)
}

fn ensure_project_exists(connection: &Connection, project_id: u32) -> Result<(), CliRunError> {
    let exists: Option<i64> = connection
        .query_row(
            "SELECT id FROM projects WHERE id = ?1",
            params![i64::from(project_id)],
            |row| row.get(0),
        )
        .optional()
        .map_err(|source| CliRunError::ParamNames(source.to_string()))?;
    if exists.is_none() {
        return Err(CliRunError::ParamNames(format!(
            "project {project_id} not found"
        )));
    }
    Ok(())
}

fn ensure_table(connection: &Connection) -> Result<(), CliRunError> {
    connection
        .execute_batch(
            r"
            CREATE TABLE IF NOT EXISTS semantic_function_param_names (
                project_id INTEGER NOT NULL,
                file_path TEXT NOT NULL,
                function_name TEXT NOT NULL,
                param_index INTEGER NOT NULL,
                semantic_name TEXT NOT NULL,
                origin TEXT,
                accepted INTEGER NOT NULL DEFAULT 1,
                created_at TEXT,
                updated_at TEXT,
                PRIMARY KEY (project_id, file_path, function_name, param_index)
            )
            ",
        )
        .map_err(|source| CliRunError::ParamNames(source.to_string()))
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn create_db(db: &std::path::Path) {
        let connection = Connection::open(db).expect("open db");
        connection
            .execute_batch(
                r"
                CREATE TABLE projects (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                INSERT INTO projects (id, name) VALUES (1, 'fixture');
                ",
            )
            .expect("schema");
    }

    fn args(db: &std::path::Path, batch: &std::path::Path, apply: bool) -> ParamNamesArgs {
        ParamNamesArgs {
            input: db.to_path_buf(),
            project_id: 1,
            apply,
            origin: "agent".to_string(),
            evidence: None,
            batch: batch.to_path_buf(),
        }
    }

    #[test]
    fn applies_param_names_from_batch() {
        let temp = tempdir().expect("temp dir");
        let db = temp.path().join("project.sqlite");
        create_db(&db);
        let batch = temp.path().join("batch.tsv");
        fs::write(
            &batch,
            "accept\tsession/manager.ts\thandleRequest\t0\trequestOptions\trequest options payload\n",
        )
        .expect("write batch");

        let (requested, written) =
            param_names_from_sqlite(&args(&db, &batch, true)).expect("apply");
        assert_eq!(requested, 1);
        assert_eq!(written, 1);

        let connection = Connection::open(&db).expect("reopen");
        let name: String = connection
            .query_row(
                "SELECT semantic_name FROM semantic_function_param_names \
                 WHERE function_name = 'handleRequest' AND param_index = 0",
                [],
                |row| row.get(0),
            )
            .expect("row");
        assert_eq!(name, "requestOptions");
    }

    #[test]
    fn dry_run_writes_nothing() {
        let temp = tempdir().expect("temp dir");
        let db = temp.path().join("project.sqlite");
        create_db(&db);
        let batch = temp.path().join("batch.tsv");
        fs::write(
            &batch,
            "accept\tsession/manager.ts\thandleRequest\t0\trequestOptions\trequest options\n",
        )
        .expect("write batch");

        let (requested, written) =
            param_names_from_sqlite(&args(&db, &batch, false)).expect("dry run");
        assert_eq!(requested, 1);
        assert_eq!(written, 0);
    }

    #[test]
    fn rejects_name_unsupported_by_evidence() {
        let temp = tempdir().expect("temp dir");
        let db = temp.path().join("project.sqlite");
        create_db(&db);
        let batch = temp.path().join("batch.tsv");
        // "zebra" is neither in the vocabulary nor in the evidence.
        fs::write(
            &batch,
            "accept\tsession/manager.ts\thandleRequest\t0\tzebraQuokka\trequest options\n",
        )
        .expect("write batch");

        let result = param_names_from_sqlite(&args(&db, &batch, true));
        assert!(matches!(result, Err(CliRunError::ParamNames(_))));
    }

    #[test]
    fn rejects_malformed_batch_row() {
        let temp = tempdir().expect("temp dir");
        let db = temp.path().join("project.sqlite");
        create_db(&db);
        let batch = temp.path().join("batch.tsv");
        fs::write(&batch, "accept\tsession/manager.ts\thandleRequest\n").expect("write batch");

        let result = param_names_from_sqlite(&args(&db, &batch, true));
        assert!(matches!(result, Err(CliRunError::ParamNames(_))));
    }
}
