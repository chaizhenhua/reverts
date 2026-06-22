//! `module-names` command: accept semantic file paths for first-party modules.
//!
//! Module semantic paths are persisted as `module_path_overrides` rows and
//! consumed by `generate` (via the input loader) to place each
//! module's emitted file and to recompute every importing file's relative
//! specifier. This is the agent-facing writer of `module_path_overrides` — the
//! mechanism that turns minified module file names (e.g. `247-esbuild-rbr.ts`)
//! into readable paths (e.g. `feature/markdown-inline-renderer`).

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
pub struct ModuleNamesArgs {
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
    #[arg(long = "accept", value_parser = parse_module_name_spec)]
    pub accepts: Vec<ModuleNameSpec>,
    #[arg(long)]
    pub batch: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleNameSpec {
    pub module_id: u32,
    pub semantic_path: String,
    pub evidence: Option<String>,
}

impl ModuleNamesArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let mut args = args.into_iter().collect::<Vec<_>>();
        if args
            .first()
            .is_some_and(|argument| argument == crate::help::MODULE_NAMES_COMMAND)
        {
            args.remove(0);
        }
        let parsed: Self = parse_args_with_name(crate::help::MODULE_NAMES_COMMAND, args)?;
        validate_args(parsed)
    }
}

pub(crate) fn validate_args(args: ModuleNamesArgs) -> Result<ModuleNamesArgs, CliError> {
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
pub struct ModuleNameRow {
    pub module_id: u32,
    pub semantic_path: String,
    pub origin: String,
    pub evidence: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleNamesOutcome {
    pub listed: Vec<ModuleNameRow>,
    pub requested_changes: usize,
    pub written_changes: usize,
}

pub(crate) fn run(args: ModuleNamesArgs) -> Result<(), CliRunError> {
    let outcome = module_names_from_sqlite(&args)?;
    if args.list {
        println!("module_id\tsemantic_path\torigin\tevidence");
        for row in outcome.listed {
            println!(
                "{}\t{}\t{}\t{}",
                row.module_id,
                row.semantic_path,
                row.origin,
                row.evidence.unwrap_or_default()
            );
        }
    } else if args.apply {
        println!(
            "updated module names for project {}: {} change(s) written",
            args.project_id, outcome.written_changes
        );
    } else {
        println!(
            "dry-run: would update {} module name(s) for project {}; pass --apply to persist",
            outcome.requested_changes, args.project_id
        );
    }
    Ok(())
}

pub fn module_names_from_sqlite(args: &ModuleNamesArgs) -> Result<ModuleNamesOutcome, CliRunError> {
    let flags = if args.apply {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let mut connection = Connection::open_with_flags(args.input.as_path(), flags)
        .map_err(|source| CliRunError::ModuleNames(source.to_string()))?;
    connection
        .busy_timeout(Duration::from_secs(30))
        .map_err(|source| CliRunError::ModuleNames(source.to_string()))?;

    if args.list {
        if !sqlite_table_exists(&connection, "module_path_overrides")
            .map_err(|source| CliRunError::ModuleNames(source.to_string()))?
        {
            return Ok(ModuleNamesOutcome {
                listed: Vec::new(),
                requested_changes: 0,
                written_changes: 0,
            });
        }
        return Ok(ModuleNamesOutcome {
            listed: load_module_name_rows(&connection, args.project_id)?,
            requested_changes: 0,
            written_changes: 0,
        });
    }

    let specs = collect_specs(args)?;
    validate_specs(&specs, args.origin.as_str())?;

    let written_changes = if args.apply {
        ensure_module_path_overrides_table(&connection)?;
        let transaction = connection
            .transaction()
            .map_err(|source| CliRunError::ModuleNames(source.to_string()))?;
        let mut written = 0_usize;
        for spec in &specs {
            // One active override per module: retire any prior accepted row for
            // this module before inserting the new one, so the loader's
            // "latest accepted" pick is unambiguous.
            transaction
                .execute(
                    r"
                    UPDATE module_path_overrides SET accepted = 0, updated_at = datetime('now')
                     WHERE project_id = ?1 AND module_id = ?2 AND accepted = 1
                    ",
                    params![i64::from(args.project_id), i64::from(spec.module_id)],
                )
                .map_err(|source| CliRunError::ModuleNames(source.to_string()))?;
            written += transaction
                .execute(
                    r"
                    INSERT INTO module_path_overrides (
                        project_id, module_id, path, origin, evidence, accepted,
                        created_at, updated_at
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, 1, datetime('now'), datetime('now'))
                    ON CONFLICT(project_id, module_id, origin, path) DO UPDATE SET
                        evidence = excluded.evidence,
                        accepted = 1,
                        updated_at = datetime('now')
                    ",
                    params![
                        i64::from(args.project_id),
                        i64::from(spec.module_id),
                        spec.semantic_path,
                        args.origin,
                        spec.evidence.as_deref().or(args.evidence.as_deref()),
                    ],
                )
                .map_err(|source| CliRunError::ModuleNames(source.to_string()))?;
        }
        transaction
            .commit()
            .map_err(|source| CliRunError::ModuleNames(source.to_string()))?;
        written
    } else {
        0
    };

    Ok(ModuleNamesOutcome {
        listed: Vec::new(),
        requested_changes: specs.len(),
        written_changes,
    })
}

fn parse_module_name_spec(value: &str) -> Result<ModuleNameSpec, String> {
    let Some((module_id, semantic_path)) = value.split_once('=') else {
        return Err("expected MODULE_ID=SEMANTIC_PATH".to_string());
    };
    let module_id = module_id
        .trim()
        .parse::<u32>()
        .map_err(|_| "MODULE_ID must be a positive integer".to_string())?;
    if semantic_path.trim().is_empty() {
        return Err("SEMANTIC_PATH must be non-empty".to_string());
    }
    Ok(ModuleNameSpec {
        module_id,
        semantic_path: semantic_path.trim().to_string(),
        evidence: None,
    })
}

fn collect_specs(args: &ModuleNamesArgs) -> Result<Vec<ModuleNameSpec>, CliRunError> {
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
                return Err(CliRunError::ModuleNames(format!(
                    "invalid batch row {}: expected accept<TAB>MODULE_ID<TAB>SEMANTIC_PATH<TAB>[EVIDENCE]",
                    index + 1
                )));
            }
            let module_id = columns[1].trim().parse::<u32>().map_err(|_| {
                CliRunError::ModuleNames(format!(
                    "invalid batch row {}: MODULE_ID must be a positive integer",
                    index + 1
                ))
            })?;
            specs.push(ModuleNameSpec {
                module_id,
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

fn validate_specs(specs: &[ModuleNameSpec], origin: &str) -> Result<(), CliRunError> {
    for spec in specs {
        if spec.module_id == 0 {
            return Err(CliRunError::ModuleNames(
                "module_id must be a positive integer".to_string(),
            ));
        }
        validate_module_path_acceptance(spec.semantic_path.as_str(), origin)
            .map_err(|error| CliRunError::ModuleNames(error.message()))?;
    }
    Ok(())
}

fn ensure_module_path_overrides_table(connection: &Connection) -> Result<(), CliRunError> {
    connection
        .execute_batch(
            r"
            CREATE TABLE IF NOT EXISTS module_path_overrides (
                project_id INTEGER NOT NULL,
                module_id INTEGER NOT NULL,
                path TEXT NOT NULL,
                origin TEXT NOT NULL,
                evidence TEXT,
                accepted INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (project_id, module_id, origin, path)
            );
            CREATE INDEX IF NOT EXISTS idx_module_path_overrides_project_module
                ON module_path_overrides(project_id, module_id, accepted);
            ",
        )
        .map_err(|source| CliRunError::ModuleNames(source.to_string()))
}

fn load_module_name_rows(
    connection: &Connection,
    project_id: u32,
) -> Result<Vec<ModuleNameRow>, CliRunError> {
    let mut statement = connection
        .prepare(
            r"
            SELECT module_id, path, origin, evidence
            FROM module_path_overrides
            WHERE project_id = ?1 AND accepted = 1
            ORDER BY module_id, updated_at DESC
            ",
        )
        .map_err(|source| CliRunError::ModuleNames(source.to_string()))?;
    let rows = statement
        .query_map(params![i64::from(project_id)], |row| {
            Ok(ModuleNameRow {
                module_id: u32::try_from(row.get::<_, i64>(0)?).unwrap_or(0),
                semantic_path: row.get::<_, String>(1)?,
                origin: row.get::<_, String>(2)?,
                evidence: row.get::<_, Option<String>>(3)?,
            })
        })
        .map_err(|source| CliRunError::ModuleNames(source.to_string()))?;
    collect_sqlite_rows(rows).map_err(|source| CliRunError::ModuleNames(source.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let path = temp.path().join("input.sqlite");
        let connection = Connection::open(path.as_path()).expect("open sqlite");
        connection
            .execute_batch(
                r"
                CREATE TABLE projects (id INTEGER PRIMARY KEY);
                INSERT INTO projects (id) VALUES (1);
                ",
            )
            .expect("seed schema");
        drop(connection);
        (temp, path)
    }

    fn accept_args(path: &std::path::Path, specs: Vec<ModuleNameSpec>) -> ModuleNamesArgs {
        ModuleNamesArgs {
            input: path.to_path_buf(),
            project_id: 1,
            list: false,
            apply: true,
            origin: "agent".to_string(),
            evidence: Some(
                "module export surface and call sites name it as a markdown renderer".to_string(),
            ),
            accepts: specs,
            batch: None,
        }
    }

    #[test]
    fn accept_writes_single_active_override_per_module() {
        let (_temp, path) = db();
        let args = accept_args(
            &path,
            vec![ModuleNameSpec {
                module_id: 247,
                semantic_path: "feature/markdown-inline-renderer".to_string(),
                evidence: None,
            }],
        );
        let outcome = module_names_from_sqlite(&args).expect("first accept");
        assert_eq!(outcome.written_changes, 1);

        // Re-accepting a different path for the same module retires the old one,
        // so the loader's "latest accepted" pick stays single-valued.
        let args2 = accept_args(
            &path,
            vec![ModuleNameSpec {
                module_id: 247,
                semantic_path: "feature/markdown-renderer".to_string(),
                evidence: None,
            }],
        );
        module_names_from_sqlite(&args2).expect("second accept");

        let connection = Connection::open(path.as_path()).expect("open");
        let active: Vec<String> = {
            let mut stmt = connection
                .prepare(
                    "SELECT path FROM module_path_overrides WHERE module_id = 247 AND accepted = 1",
                )
                .expect("prepare");
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .expect("query");
            rows.map(|row| row.expect("row")).collect()
        };
        assert_eq!(active, vec!["feature/markdown-renderer".to_string()]);
    }

    #[test]
    fn rejects_unsafe_module_path() {
        let (_temp, path) = db();
        let args = accept_args(
            &path,
            vec![ModuleNameSpec {
                module_id: 1,
                semantic_path: "../escape".to_string(),
                evidence: None,
            }],
        );
        let error = module_names_from_sqlite(&args).expect_err("dot-dot path must be rejected");
        assert!(error.to_string().contains("module-names"));
    }

    #[test]
    fn dry_run_does_not_write() {
        let (_temp, path) = db();
        let mut args = accept_args(
            &path,
            vec![ModuleNameSpec {
                module_id: 1,
                semantic_path: "feature/thing".to_string(),
                evidence: None,
            }],
        );
        args.apply = false;
        let outcome = module_names_from_sqlite(&args).expect("dry run");
        assert_eq!(outcome.requested_changes, 1);
        assert_eq!(outcome.written_changes, 0);
        let connection = Connection::open(path.as_path()).expect("open");
        assert!(
            !crate::sqlite_table_exists(&connection, "module_path_overrides").expect("check"),
            "dry-run must not create the overrides table"
        );
    }
}
