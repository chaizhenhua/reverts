//! `binding-names` command: accept explicit semantic names for generated-output
//! local bindings. These names are consumed by `generate-project-v2` before
//! emission and counted by `identifier-inventory` only when accepted.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use clap::Args;
use reverts_js::{is_generated_placeholder_identifier, sanitize_identifier};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::args::{parse_args_with_name, parse_project_id};
use crate::errors::{CliError, CliRunError};
use crate::{collect_sqlite_rows, sqlite_table_exists};

#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct BindingNamesArgs {
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
    #[arg(long = "accept", value_parser = parse_binding_name_spec)]
    pub accepts: Vec<BindingNameSpec>,
    #[arg(long)]
    pub batch: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingNameSpec {
    pub file_path: String,
    pub original_name: String,
    pub binding_index: Option<u32>,
    pub semantic_name: String,
}

impl BindingNamesArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let mut args = args.into_iter().collect::<Vec<_>>();
        if args
            .first()
            .is_some_and(|argument| argument == crate::help::BINDING_NAMES_COMMAND)
        {
            args.remove(0);
        }
        let parsed: Self = parse_args_with_name(crate::help::BINDING_NAMES_COMMAND, args)?;
        validate_args(parsed)
    }
}

pub fn validate_args(args: BindingNamesArgs) -> Result<BindingNamesArgs, CliError> {
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

pub(crate) fn run(args: BindingNamesArgs) -> Result<(), CliRunError> {
    let outcome = binding_names_from_sqlite(&args)?;
    if args.list {
        println!("file_path\toriginal_name\tbinding_index\tsemantic_name\torigin\tevidence");
        for row in outcome.listed {
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}",
                row.file_path,
                row.original_name,
                row.binding_index
                    .map(|index| index.to_string())
                    .unwrap_or_default(),
                row.semantic_name,
                row.origin,
                row.evidence.unwrap_or_default()
            );
        }
    } else if args.apply {
        println!(
            "updated binding names for project {}: {} change(s) written",
            args.project_id, outcome.written_changes
        );
    } else {
        println!(
            "dry-run: would update {} binding name(s) for project {}; pass --apply to persist",
            outcome.requested_changes, args.project_id
        );
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingNamesOutcome {
    pub listed: Vec<BindingNameRow>,
    pub requested_changes: usize,
    pub written_changes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingNameRow {
    pub file_path: String,
    pub original_name: String,
    pub binding_index: Option<u32>,
    pub semantic_name: String,
    pub origin: String,
    pub evidence: Option<String>,
}

pub fn binding_names_from_sqlite(
    args: &BindingNamesArgs,
) -> Result<BindingNamesOutcome, CliRunError> {
    let flags = if args.apply {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let mut connection = Connection::open_with_flags(args.input.as_path(), flags)
        .map_err(|source| CliRunError::BindingNames(source.to_string()))?;
    connection
        .busy_timeout(Duration::from_secs(30))
        .map_err(|source| CliRunError::BindingNames(source.to_string()))?;
    connection
        .execute_batch("PRAGMA foreign_keys = ON")
        .map_err(|source| CliRunError::BindingNames(source.to_string()))?;
    ensure_project_exists(&connection, args.project_id)?;
    if args.list {
        ensure_binding_names_table_if_writable(&connection, false)?;
        return Ok(BindingNamesOutcome {
            listed: load_binding_name_rows(&connection, args.project_id)?,
            requested_changes: 0,
            written_changes: 0,
        });
    }

    let specs = collect_specs(args)?;
    validate_specs(&specs)?;
    if args.apply {
        ensure_binding_names_table_if_writable(&connection, true)?;
    }
    validate_final_names(&connection, args.project_id, &specs)?;
    let written_changes = if args.apply {
        let transaction = connection
            .transaction()
            .map_err(|source| CliRunError::BindingNames(source.to_string()))?;
        let mut written = 0_usize;
        for spec in &specs {
            let binding_key = binding_key(spec.binding_index);
            written += transaction
                .execute(
                    r"
                    INSERT INTO semantic_binding_names (
                        project_id, file_path, original_name, binding_index, binding_key, semantic_name,
                        origin, evidence, accepted, created_at, updated_at
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, datetime('now'), datetime('now'))
                    ON CONFLICT(project_id, file_path, original_name, binding_key) DO UPDATE SET
                        binding_index = excluded.binding_index,
                        semantic_name = excluded.semantic_name,
                        origin = excluded.origin,
                        evidence = excluded.evidence,
                        accepted = 1,
                        updated_at = datetime('now')
                    ",
                    params![
                        i64::from(args.project_id),
                        spec.file_path,
                        spec.original_name,
                        spec.binding_index.map(i64::from),
                        binding_key,
                        spec.semantic_name,
                        args.origin,
                        args.evidence,
                    ],
                )
                .map_err(|source| CliRunError::BindingNames(source.to_string()))?;
        }
        transaction
            .commit()
            .map_err(|source| CliRunError::BindingNames(source.to_string()))?;
        written
    } else {
        0
    };

    Ok(BindingNamesOutcome {
        listed: Vec::new(),
        requested_changes: specs.len(),
        written_changes,
    })
}

fn parse_binding_name_spec(value: &str) -> Result<BindingNameSpec, String> {
    let Some((target, semantic_name)) = value.split_once('=') else {
        return Err("expected FILE_PATH:ORIGINAL_NAME=SEMANTIC_NAME".to_string());
    };
    let Some((file_path, original_name)) = target.rsplit_once(':') else {
        return Err("expected FILE_PATH:ORIGINAL_NAME=SEMANTIC_NAME".to_string());
    };
    let (original_name, binding_index) = parse_original_name_with_optional_index(original_name)?;
    Ok(BindingNameSpec {
        file_path: file_path.to_string(),
        original_name,
        binding_index,
        semantic_name: semantic_name.to_string(),
    })
}

fn parse_original_name_with_optional_index(value: &str) -> Result<(String, Option<u32>), String> {
    let Some((original_name, binding_index)) = value.rsplit_once('#') else {
        return Ok((value.to_string(), None));
    };
    if original_name.is_empty() {
        return Err("original name before # must be non-empty".to_string());
    }
    let binding_index = binding_index
        .parse::<u32>()
        .map_err(|_| "binding index after # must be a positive integer".to_string())?;
    if binding_index == 0 {
        return Err("binding index after # must be a positive integer".to_string());
    }
    Ok((original_name.to_string(), Some(binding_index)))
}

fn collect_specs(args: &BindingNamesArgs) -> Result<Vec<BindingNameSpec>, CliRunError> {
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
            if columns.len() < 4 || columns[0] != "accept" {
                return Err(CliRunError::BindingNames(format!(
                    "invalid batch row {}: expected accept<TAB>FILE_PATH<TAB>ORIGINAL_NAME<TAB>SEMANTIC_NAME or accept<TAB>FILE_PATH<TAB>ORIGINAL_NAME<TAB>BINDING_INDEX<TAB>SEMANTIC_NAME",
                    index + 1
                )));
            }
            let file_path = columns[1].to_string();
            let original_name = columns[2].to_string();
            let (binding_index, semantic_name) = if columns.len() >= 5 {
                match columns[3].parse::<u32>() {
                    Ok(index) if index > 0 => (Some(index), columns[4].to_string()),
                    _ => (None, columns[3].to_string()),
                }
            } else {
                (None, columns[3].to_string())
            };
            specs.push(BindingNameSpec {
                file_path,
                original_name,
                binding_index,
                semantic_name,
            });
        }
    }
    Ok(specs)
}

fn validate_specs(specs: &[BindingNameSpec]) -> Result<(), CliRunError> {
    for spec in specs {
        if spec.file_path.trim().is_empty()
            || spec.original_name.trim().is_empty()
            || spec.semantic_name.trim().is_empty()
        {
            return Err(CliRunError::BindingNames(
                "file_path, original_name, and semantic_name must be non-empty".to_string(),
            ));
        }
        if spec.binding_index == Some(0) {
            return Err(CliRunError::BindingNames(
                "binding_index must be a positive integer when supplied".to_string(),
            ));
        }
        if is_generated_placeholder_identifier(&spec.semantic_name)
            || sanitize_identifier(&spec.semantic_name) != spec.semantic_name
        {
            return Err(CliRunError::BindingNames(format!(
                "invalid semantic name {} for {}:{}",
                spec.semantic_name, spec.file_path, spec.original_name
            )));
        }
    }
    Ok(())
}

fn validate_final_names(
    connection: &Connection,
    project_id: u32,
    specs: &[BindingNameSpec],
) -> Result<(), CliRunError> {
    let mut by_file_semantic = BTreeMap::<(String, String), String>::new();
    if sqlite_table_exists(connection, "semantic_binding_names")
        .map_err(|source| CliRunError::BindingNames(source.to_string()))?
    {
        let has_binding_key = binding_names_table_has_binding_key(connection)?;
        let mut statement = connection
            .prepare(if has_binding_key {
                r"
                SELECT file_path, original_name, binding_index, semantic_name
                FROM semantic_binding_names
                WHERE project_id = ?1 AND accepted = 1
                "
            } else {
                r"
                SELECT file_path, original_name, NULL AS binding_index, semantic_name
                FROM semantic_binding_names
                WHERE project_id = ?1 AND accepted = 1
                "
            })
            .map_err(|source| CliRunError::BindingNames(source.to_string()))?;
        let rows = statement
            .query_map(params![i64::from(project_id)], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(|source| CliRunError::BindingNames(source.to_string()))?;
        for row in collect_sqlite_rows(rows)
            .map_err(|source| CliRunError::BindingNames(source.to_string()))?
        {
            if row.2.is_none() {
                by_file_semantic.insert((row.0, row.3), row.1);
            }
        }
    }
    for spec in specs {
        if spec.binding_index.is_some() {
            continue;
        }
        let key = (spec.file_path.clone(), spec.semantic_name.clone());
        if let Some(existing_original) = by_file_semantic.get(&key)
            && existing_original != &spec.original_name
        {
            return Err(CliRunError::BindingNames(format!(
                "semantic name collision in {}: {} is already assigned to {}",
                spec.file_path, spec.semantic_name, existing_original
            )));
        }
        by_file_semantic.insert(key, spec.original_name.clone());
    }
    Ok(())
}

fn binding_key(binding_index: Option<u32>) -> String {
    binding_index
        .map(|index| index.to_string())
        .unwrap_or_else(|| "*".to_string())
}

fn binding_names_table_has_binding_key(connection: &Connection) -> Result<bool, CliRunError> {
    let columns = connection
        .prepare("PRAGMA table_info(semantic_binding_names)")
        .and_then(|mut statement| {
            let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
            collect_sqlite_rows(rows)
        })
        .map_err(|source| CliRunError::BindingNames(source.to_string()))?;
    Ok(columns.iter().any(|column| column == "binding_key"))
}

fn ensure_project_exists(connection: &Connection, project_id: u32) -> Result<(), CliRunError> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM projects WHERE id = ?1",
            params![i64::from(project_id)],
            |_| Ok(()),
        )
        .optional()
        .map_err(|source| CliRunError::BindingNames(source.to_string()))?
        .is_some();
    if exists {
        Ok(())
    } else {
        Err(CliRunError::BindingNames(format!(
            "project {project_id} not found"
        )))
    }
}

pub(crate) fn ensure_binding_names_table_if_writable(
    connection: &Connection,
    writable: bool,
) -> Result<(), CliRunError> {
    if sqlite_table_exists(connection, "semantic_binding_names")
        .map_err(|source| CliRunError::BindingNames(source.to_string()))?
    {
        if writable {
            migrate_binding_names_table(connection)?;
        }
        return Ok(());
    }
    if !writable {
        return Ok(());
    }
    connection
        .execute_batch(
            r"
            CREATE TABLE IF NOT EXISTS semantic_binding_names (
                project_id INTEGER NOT NULL,
                file_path TEXT NOT NULL,
                original_name TEXT NOT NULL,
                binding_index INTEGER,
                binding_key TEXT NOT NULL,
                semantic_name TEXT NOT NULL,
                origin TEXT NOT NULL,
                evidence TEXT,
                accepted INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (project_id, file_path, original_name, binding_key)
            );
            CREATE INDEX IF NOT EXISTS idx_semantic_binding_names_project_file
                ON semantic_binding_names(project_id, file_path);
            ",
        )
        .map_err(|source| CliRunError::BindingNames(source.to_string()))
}

fn migrate_binding_names_table(connection: &Connection) -> Result<(), CliRunError> {
    let columns = connection
        .prepare("PRAGMA table_info(semantic_binding_names)")
        .and_then(|mut statement| {
            let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
            collect_sqlite_rows(rows)
        })
        .map_err(|source| CliRunError::BindingNames(source.to_string()))?;
    if columns.iter().any(|column| column == "binding_key") {
        return Ok(());
    }
    connection
        .execute_batch(
            r"
            ALTER TABLE semantic_binding_names RENAME TO semantic_binding_names_old;
            CREATE TABLE semantic_binding_names (
                project_id INTEGER NOT NULL,
                file_path TEXT NOT NULL,
                original_name TEXT NOT NULL,
                binding_index INTEGER,
                binding_key TEXT NOT NULL,
                semantic_name TEXT NOT NULL,
                origin TEXT NOT NULL,
                evidence TEXT,
                accepted INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (project_id, file_path, original_name, binding_key)
            );
            INSERT INTO semantic_binding_names (
                project_id, file_path, original_name, binding_index, binding_key,
                semantic_name, origin, evidence, accepted, created_at, updated_at
            )
            SELECT
                project_id, file_path, original_name, NULL, '*',
                semantic_name, origin, evidence, accepted, created_at, updated_at
            FROM semantic_binding_names_old;
            DROP TABLE semantic_binding_names_old;
            CREATE INDEX IF NOT EXISTS idx_semantic_binding_names_project_file
                ON semantic_binding_names(project_id, file_path);
            ",
        )
        .map_err(|source| CliRunError::BindingNames(source.to_string()))
}

fn load_binding_name_rows(
    connection: &Connection,
    project_id: u32,
) -> Result<Vec<BindingNameRow>, CliRunError> {
    if !sqlite_table_exists(connection, "semantic_binding_names")
        .map_err(|source| CliRunError::BindingNames(source.to_string()))?
    {
        return Ok(Vec::new());
    }
    let has_binding_key = binding_names_table_has_binding_key(connection)?;
    let mut statement = connection
        .prepare(if has_binding_key {
            r"
            SELECT file_path, original_name, binding_index, semantic_name, origin, evidence
            FROM semantic_binding_names
            WHERE project_id = ?1 AND accepted = 1
            ORDER BY file_path, original_name, binding_key
            "
        } else {
            r"
            SELECT file_path, original_name, NULL AS binding_index, semantic_name, origin, evidence
            FROM semantic_binding_names
            WHERE project_id = ?1 AND accepted = 1
            ORDER BY file_path, original_name
            "
        })
        .map_err(|source| CliRunError::BindingNames(source.to_string()))?;
    let rows = statement
        .query_map(params![i64::from(project_id)], |row| {
            Ok(BindingNameRow {
                file_path: row.get(0)?,
                original_name: row.get(1)?,
                binding_index: row
                    .get::<_, Option<i64>>(2)?
                    .and_then(|value| u32::try_from(value).ok()),
                semantic_name: row.get(3)?,
                origin: row.get(4)?,
                evidence: row.get(5)?,
            })
        })
        .map_err(|source| CliRunError::BindingNames(source.to_string()))?;
    collect_sqlite_rows(rows).map_err(|source| CliRunError::BindingNames(source.to_string()))
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::*;

    fn create_db(connection: &Connection) {
        connection
            .execute_batch(
                r"
                CREATE TABLE projects (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                INSERT INTO projects (id, name) VALUES (1, 'fixture');
                ",
            )
            .expect("schema");
    }

    #[test]
    fn accepts_and_lists_binding_names() {
        let temp = tempdir().expect("temp dir");
        let db = temp.path().join("project.sqlite");
        let connection = Connection::open(&db).expect("open db");
        create_db(&connection);
        drop(connection);

        let args = BindingNamesArgs {
            input: db.clone(),
            project_id: 1,
            list: false,
            apply: true,
            origin: "agent".to_string(),
            evidence: Some("test".to_string()),
            accepts: vec![BindingNameSpec {
                file_path: "modules/entrypoint.ts".to_string(),
                original_name: "a".to_string(),
                binding_index: None,
                semantic_name: "requestOptions".to_string(),
            }],
            batch: None,
        };
        let outcome = binding_names_from_sqlite(&args).expect("apply");
        assert_eq!(outcome.written_changes, 1);

        let listed = binding_names_from_sqlite(&BindingNamesArgs {
            input: db,
            project_id: 1,
            list: true,
            apply: false,
            origin: "agent".to_string(),
            evidence: None,
            accepts: Vec::new(),
            batch: None,
        })
        .expect("list");
        assert_eq!(listed.listed.len(), 1);
        assert_eq!(listed.listed[0].semantic_name, "requestOptions");
    }

    #[test]
    fn accepts_binding_index_names() {
        let temp = tempdir().expect("temp dir");
        let db = temp.path().join("project.sqlite");
        let connection = Connection::open(&db).expect("open db");
        create_db(&connection);
        drop(connection);

        let args = BindingNamesArgs {
            input: db.clone(),
            project_id: 1,
            list: false,
            apply: true,
            origin: "agent".to_string(),
            evidence: Some("test".to_string()),
            accepts: vec![BindingNameSpec {
                file_path: "modules/entrypoint.ts".to_string(),
                original_name: "a".to_string(),
                binding_index: Some(2),
                semantic_name: "secondInput".to_string(),
            }],
            batch: None,
        };
        let outcome = binding_names_from_sqlite(&args).expect("apply");
        assert_eq!(outcome.written_changes, 1);

        let listed = binding_names_from_sqlite(&BindingNamesArgs {
            input: db,
            project_id: 1,
            list: true,
            apply: false,
            origin: "agent".to_string(),
            evidence: None,
            accepts: Vec::new(),
            batch: None,
        })
        .expect("list");
        assert_eq!(listed.listed[0].binding_index, Some(2));
        assert_eq!(listed.listed[0].semantic_name, "secondInput");
    }

    #[test]
    fn rejects_placeholder_binding_name() {
        let spec = BindingNameSpec {
            file_path: "modules/entrypoint.ts".to_string(),
            original_name: "a".to_string(),
            binding_index: None,
            semantic_name: "semanticValue1".to_string(),
        };

        assert!(validate_specs(&[spec]).is_err());
    }
}
