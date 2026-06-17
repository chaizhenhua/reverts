//! `symbol-names` command: inspect and manually set symbol semantic names.
//!
//! The command writes the existing `symbols.semantic_name` input field rather
//! than editing emitted output. The next `generate-project-v2` run then carries
//! the requested names through the normal input → analyze → plan → emit path.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use clap::Args;
use reverts_js::sanitize_identifier;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::args::{parse_args_with_name, parse_project_id};
use crate::errors::{CliError, CliRunError, SymbolNamesError};
use crate::{collect_sqlite_rows, sqlite_table_has_column};

pub const SYMBOL_NAME_SOURCE_MANUAL: &str = "manual";

#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct SymbolNamesArgs {
    #[arg(long)]
    pub input: PathBuf,
    #[arg(long, value_parser = parse_project_id)]
    pub project_id: u32,
    #[arg(long)]
    pub list: bool,
    #[arg(long)]
    pub apply: bool,
    #[arg(long = "set", value_parser = parse_set_spec)]
    pub sets: Vec<SymbolNameSetSpec>,
    #[arg(long = "clear", value_parser = parse_clear_spec)]
    pub clears: Vec<SymbolNameClearSpec>,
    #[arg(long)]
    pub batch: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolNameSetSpec {
    pub module_id: u32,
    pub original_name: String,
    pub semantic_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolNameClearSpec {
    pub module_id: u32,
    pub original_name: String,
}

impl SymbolNamesArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let mut args = args.into_iter().collect::<Vec<_>>();
        if args
            .first()
            .is_some_and(|argument| argument == crate::help::SYMBOL_NAMES_COMMAND)
        {
            args.remove(0);
        }
        let parsed: Self = parse_args_with_name(crate::help::SYMBOL_NAMES_COMMAND, args)?;

        if parsed.list
            && (!parsed.sets.is_empty()
                || !parsed.clears.is_empty()
                || parsed.batch.is_some()
                || parsed.apply)
        {
            return Err(CliError::UnknownArgument(
                "--list cannot be combined with mutations".to_string(),
            ));
        }
        if !parsed.list
            && parsed.sets.is_empty()
            && parsed.clears.is_empty()
            && parsed.batch.is_none()
        {
            return Err(CliError::MissingArgument(
                "--list | --set | --clear | --batch",
            ));
        }

        Ok(parsed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolNamesOutcome {
    pub project_id: u32,
    pub listed: Vec<SymbolNameRow>,
    pub requested_changes: usize,
    pub written_changes: usize,
    pub apply: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolNameRow {
    pub module_id: u32,
    pub original_name: String,
    pub semantic_name: Option<String>,
    pub semantic_name_source: Option<String>,
    pub export_name: Option<String>,
    pub scope_level: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SymbolNameOperation {
    Set(SymbolNameSetSpec),
    Clear(SymbolNameClearSpec),
}

pub(crate) fn run(args: SymbolNamesArgs) -> Result<(), CliRunError> {
    let outcome = symbol_names_from_sqlite(&args).map_err(CliRunError::SymbolNames)?;
    if args.list {
        print_symbol_rows(&outcome.listed);
    } else if outcome.apply {
        println!(
            "updated symbol names for project {}: {} change(s) written",
            outcome.project_id, outcome.written_changes
        );
    } else {
        println!(
            "dry-run: would update {} symbol name(s) for project {}; pass --apply to persist",
            outcome.requested_changes, outcome.project_id
        );
    }
    Ok(())
}

pub fn symbol_names_from_sqlite(
    args: &SymbolNamesArgs,
) -> Result<SymbolNamesOutcome, SymbolNamesError> {
    let flags = if args.apply {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let mut connection =
        Connection::open_with_flags(args.input.as_path(), flags).map_err(|source| {
            SymbolNamesError::OpenDatabase {
                path: args.input.clone(),
                source,
            }
        })?;
    connection
        .busy_timeout(Duration::from_secs(30))
        .map_err(SymbolNamesError::ConfigureDatabase)?;
    connection
        .execute_batch("PRAGMA foreign_keys = ON")
        .map_err(SymbolNamesError::ConfigureDatabase)?;
    symbol_names_from_connection(&mut connection, args)
}

pub fn symbol_names_from_connection(
    connection: &mut Connection,
    args: &SymbolNamesArgs,
) -> Result<SymbolNamesOutcome, SymbolNamesError> {
    ensure_project_exists(connection, args.project_id)?;
    if args.list {
        return Ok(SymbolNamesOutcome {
            project_id: args.project_id,
            listed: load_symbol_name_rows(connection, args.project_id)?,
            requested_changes: 0,
            written_changes: 0,
            apply: false,
        });
    }

    let operations = collect_operations(args)?;

    let written_changes = if args.apply {
        ensure_semantic_name_source_column(connection)?;
        let transaction = connection
            .transaction()
            .map_err(SymbolNamesError::ConfigureDatabase)?;
        validate_operation_targets(&transaction, args.project_id, &operations)?;
        validate_final_names(&transaction, args.project_id, &operations)?;
        let mut written = 0_usize;
        for operation in &operations {
            written += apply_operation(&transaction, operation)?;
        }
        transaction
            .commit()
            .map_err(SymbolNamesError::WriteSymbolName)?;
        written
    } else {
        validate_operation_targets(connection, args.project_id, &operations)?;
        validate_final_names(connection, args.project_id, &operations)?;
        0
    };

    Ok(SymbolNamesOutcome {
        project_id: args.project_id,
        listed: Vec::new(),
        requested_changes: operations.len(),
        written_changes,
        apply: args.apply,
    })
}

fn parse_set_spec(value: &str) -> Result<SymbolNameSetSpec, String> {
    let Some((target, semantic_name)) = value.split_once('=') else {
        return Err(format!(
            "invalid --set value {value}; expected MODULE_ID:ORIGINAL=SEMANTIC"
        ));
    };
    let clear = parse_clear_spec(target)?;
    if semantic_name.trim().is_empty() {
        return Err(format!(
            "invalid --set value {value}; semantic name is empty"
        ));
    }
    Ok(SymbolNameSetSpec {
        module_id: clear.module_id,
        original_name: clear.original_name,
        semantic_name: semantic_name.to_string(),
    })
}

fn parse_clear_spec(value: &str) -> Result<SymbolNameClearSpec, String> {
    let Some((module_id, original_name)) = value.split_once(':') else {
        return Err(format!(
            "invalid symbol target {value}; expected MODULE_ID:ORIGINAL"
        ));
    };
    if original_name.is_empty() {
        return Err(format!(
            "invalid symbol target {value}; original name is empty"
        ));
    }
    Ok(SymbolNameClearSpec {
        module_id: parse_project_id(module_id)?,
        original_name: original_name.to_string(),
    })
}

fn collect_operations(
    args: &SymbolNamesArgs,
) -> Result<Vec<SymbolNameOperation>, SymbolNamesError> {
    let mut operations = Vec::new();
    operations.extend(args.sets.iter().cloned().map(SymbolNameOperation::Set));
    operations.extend(args.clears.iter().cloned().map(SymbolNameOperation::Clear));
    if let Some(batch) = &args.batch {
        operations.extend(load_batch_operations(batch.as_path())?);
    }
    validate_unique_operations(&operations)?;
    Ok(operations)
}

fn load_batch_operations(
    path: &std::path::Path,
) -> Result<Vec<SymbolNameOperation>, SymbolNamesError> {
    let content = if path == std::path::Path::new("-") {
        std::io::read_to_string(std::io::stdin()).map_err(SymbolNamesError::ReadBatch)?
    } else {
        fs::read_to_string(path).map_err(SymbolNamesError::ReadBatch)?
    };
    parse_batch_operations(content.as_str())
}

fn parse_batch_operations(content: &str) -> Result<Vec<SymbolNameOperation>, SymbolNamesError> {
    let mut operations = Vec::new();
    for (line_index, line) in content.lines().enumerate() {
        let line_number = line_index + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.first().copied() == Some("action") {
            continue;
        }
        match fields.as_slice() {
            ["set", module_id, original_name, semantic_name] => {
                if original_name.is_empty() || semantic_name.is_empty() {
                    return Err(SymbolNamesError::InvalidBatchLine {
                        line: line_number,
                        message: "set requires non-empty original_name and semantic_name"
                            .to_string(),
                    });
                }
                operations.push(SymbolNameOperation::Set(SymbolNameSetSpec {
                    module_id: parse_batch_u32(module_id, line_number)?,
                    original_name: (*original_name).to_string(),
                    semantic_name: (*semantic_name).to_string(),
                }));
            }
            ["clear", module_id, original_name] => {
                if original_name.is_empty() {
                    return Err(SymbolNamesError::InvalidBatchLine {
                        line: line_number,
                        message: "clear requires non-empty original_name".to_string(),
                    });
                }
                operations.push(SymbolNameOperation::Clear(SymbolNameClearSpec {
                    module_id: parse_batch_u32(module_id, line_number)?,
                    original_name: (*original_name).to_string(),
                }));
            }
            _ => {
                return Err(SymbolNamesError::InvalidBatchLine {
                    line: line_number,
                    message: "expected tab-separated set MODULE_ID ORIGINAL SEMANTIC or clear MODULE_ID ORIGINAL".to_string(),
                });
            }
        }
    }
    Ok(operations)
}

fn parse_batch_u32(value: &str, line: usize) -> Result<u32, SymbolNamesError> {
    value
        .parse::<u32>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| SymbolNamesError::InvalidBatchLine {
            line,
            message: format!("invalid module id {value}"),
        })
}

fn validate_unique_operations(operations: &[SymbolNameOperation]) -> Result<(), SymbolNamesError> {
    let mut seen = BTreeSet::<(u32, &str)>::new();
    for operation in operations {
        let key = match operation {
            SymbolNameOperation::Set(spec) => (spec.module_id, spec.original_name.as_str()),
            SymbolNameOperation::Clear(spec) => (spec.module_id, spec.original_name.as_str()),
        };
        if !seen.insert(key) {
            return Err(SymbolNamesError::ConflictingOperation {
                module_id: key.0,
                original_name: key.1.to_string(),
            });
        }
        if let SymbolNameOperation::Set(spec) = operation {
            validate_semantic_identifier(spec.semantic_name.as_str())?;
        }
    }
    Ok(())
}

fn validate_semantic_identifier(name: &str) -> Result<(), SymbolNamesError> {
    if sanitize_identifier(name) == name {
        Ok(())
    } else {
        Err(SymbolNamesError::InvalidSemanticName {
            semantic_name: name.to_string(),
        })
    }
}

fn ensure_project_exists(connection: &Connection, project_id: u32) -> Result<(), SymbolNamesError> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM projects WHERE id = ?1",
            params![i64::from(project_id)],
            |_row| Ok(()),
        )
        .optional()
        .map_err(SymbolNamesError::QuerySymbolNames)?
        .is_some();
    if exists {
        Ok(())
    } else {
        Err(SymbolNamesError::ProjectNotFound { project_id })
    }
}

fn validate_operation_targets(
    connection: &Connection,
    project_id: u32,
    operations: &[SymbolNameOperation],
) -> Result<(), SymbolNamesError> {
    for operation in operations {
        let (module_id, original_name) = match operation {
            SymbolNameOperation::Set(spec) => (spec.module_id, spec.original_name.as_str()),
            SymbolNameOperation::Clear(spec) => (spec.module_id, spec.original_name.as_str()),
        };
        if !module_belongs_to_project(connection, project_id, module_id)? {
            return Err(SymbolNamesError::UnknownModule {
                project_id,
                module_id,
            });
        }
        let count = connection
            .query_row(
                r"
                SELECT COUNT(*)
                FROM symbols s
                WHERE s.module_id = ?1
                  AND s.original_name = ?2
                  AND s.scope_level = 'module'
                ",
                params![i64::from(module_id), original_name],
                |row| row.get::<_, i64>(0),
            )
            .map_err(SymbolNamesError::QuerySymbolNames)?;
        if count == 0 {
            return Err(SymbolNamesError::UnknownSymbol {
                module_id,
                original_name: original_name.to_string(),
            });
        }
    }
    Ok(())
}

fn module_belongs_to_project(
    connection: &Connection,
    project_id: u32,
    module_id: u32,
) -> Result<bool, SymbolNamesError> {
    connection
        .query_row(
            r"
            SELECT 1
            FROM modules m
            JOIN project_files pf ON pf.file_id = m.file_id
            WHERE pf.project_id = ?1
              AND m.id = ?2
            ",
            params![i64::from(project_id), i64::from(module_id)],
            |_row| Ok(()),
        )
        .optional()
        .map(|value| value.is_some())
        .map_err(SymbolNamesError::QuerySymbolNames)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SymbolNameState {
    original_names: BTreeSet<String>,
    semantic_names: BTreeMap<String, Option<String>>,
}

fn validate_final_names(
    connection: &Connection,
    project_id: u32,
    operations: &[SymbolNameOperation],
) -> Result<(), SymbolNamesError> {
    let mut states = load_symbol_name_states(connection, project_id)?;
    for operation in operations {
        match operation {
            SymbolNameOperation::Set(spec) => {
                if let Some(state) = states.get_mut(&spec.module_id) {
                    state
                        .semantic_names
                        .insert(spec.original_name.clone(), Some(spec.semantic_name.clone()));
                }
            }
            SymbolNameOperation::Clear(spec) => {
                if let Some(state) = states.get_mut(&spec.module_id) {
                    state
                        .semantic_names
                        .insert(spec.original_name.clone(), None);
                }
            }
        }
    }

    for (module_id, state) in states {
        let mut targets = BTreeMap::<String, String>::new();
        for (original_name, semantic_name) in state.semantic_names {
            let Some(semantic_name) = semantic_name else {
                continue;
            };
            if state.original_names.contains(semantic_name.as_str())
                && semantic_name != original_name
            {
                return Err(SymbolNamesError::NameCollision {
                    module_id,
                    semantic_name: semantic_name.clone(),
                    existing_original_name: semantic_name.clone(),
                });
            }
            if let Some(existing_original_name) =
                targets.insert(semantic_name.clone(), original_name)
            {
                return Err(SymbolNamesError::NameCollision {
                    module_id,
                    semantic_name,
                    existing_original_name,
                });
            }
        }
    }
    Ok(())
}

fn load_symbol_name_states(
    connection: &Connection,
    project_id: u32,
) -> Result<BTreeMap<u32, SymbolNameState>, SymbolNamesError> {
    let mut statement = connection
        .prepare(
            r"
            SELECT DISTINCT
                s.module_id,
                s.original_name,
                NULLIF(TRIM(s.semantic_name), '') AS semantic_name
            FROM symbols s
            JOIN modules m ON m.id = s.module_id
            JOIN project_files pf ON pf.file_id = m.file_id
            WHERE pf.project_id = ?1
              AND s.scope_level = 'module'
              AND TRIM(s.original_name) != ''
            ORDER BY s.module_id, s.original_name
            ",
        )
        .map_err(SymbolNamesError::QuerySymbolNames)?;
    let rows = statement
        .query_map(params![i64::from(project_id)], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(SymbolNamesError::QuerySymbolNames)?;

    let mut states = BTreeMap::<u32, SymbolNameState>::new();
    for row in rows {
        let (module_id, original_name, semantic_name) =
            row.map_err(SymbolNamesError::QuerySymbolNames)?;
        let module_id =
            u32::try_from(module_id).map_err(|_| SymbolNamesError::InvalidDatabaseId {
                owner: "symbol.module_id",
                value: module_id,
            })?;
        let state = states.entry(module_id).or_insert_with(|| SymbolNameState {
            original_names: BTreeSet::new(),
            semantic_names: BTreeMap::new(),
        });
        state.original_names.insert(original_name.clone());
        state.semantic_names.insert(original_name, semantic_name);
    }
    Ok(states)
}

fn ensure_semantic_name_source_column(connection: &Connection) -> Result<(), SymbolNamesError> {
    if sqlite_table_has_column(connection, "symbols", "semantic_name_source")
        .map_err(SymbolNamesError::QuerySymbolNames)?
    {
        return Ok(());
    }
    connection
        .execute_batch("ALTER TABLE symbols ADD COLUMN semantic_name_source TEXT;")
        .map_err(SymbolNamesError::WriteSymbolName)
}

fn apply_operation(
    connection: &Connection,
    operation: &SymbolNameOperation,
) -> Result<usize, SymbolNamesError> {
    match operation {
        SymbolNameOperation::Set(spec) => connection
            .execute(
                r"
                UPDATE symbols
                   SET semantic_name = ?3,
                       semantic_name_source = ?4
                 WHERE module_id = ?1
                   AND original_name = ?2
                   AND scope_level = 'module'
                ",
                params![
                    i64::from(spec.module_id),
                    spec.original_name.as_str(),
                    spec.semantic_name.as_str(),
                    SYMBOL_NAME_SOURCE_MANUAL,
                ],
            )
            .map_err(SymbolNamesError::WriteSymbolName),
        SymbolNameOperation::Clear(spec) => connection
            .execute(
                r"
                UPDATE symbols
                   SET semantic_name = NULL,
                       semantic_name_source = NULL
                 WHERE module_id = ?1
                   AND original_name = ?2
                   AND scope_level = 'module'
                ",
                params![i64::from(spec.module_id), spec.original_name.as_str()],
            )
            .map_err(SymbolNamesError::WriteSymbolName),
    }
}

fn load_symbol_name_rows(
    connection: &Connection,
    project_id: u32,
) -> Result<Vec<SymbolNameRow>, SymbolNamesError> {
    let source_expr = if sqlite_table_has_column(connection, "symbols", "semantic_name_source")
        .map_err(SymbolNamesError::QuerySymbolNames)?
    {
        "NULLIF(TRIM(s.semantic_name_source), '') AS semantic_name_source"
    } else {
        "NULL AS semantic_name_source"
    };
    let sql = format!(
        r"
        SELECT DISTINCT
            s.module_id,
            s.original_name,
            NULLIF(TRIM(s.semantic_name), '') AS semantic_name,
            {source_expr},
            NULLIF(TRIM(s.export_name), '') AS export_name,
            NULLIF(TRIM(s.scope_level), '') AS scope_level
        FROM symbols s
        JOIN modules m ON m.id = s.module_id
        JOIN project_files pf ON pf.file_id = m.file_id
        WHERE pf.project_id = ?1
          AND s.scope_level IN ('module', 'global')
          AND TRIM(s.original_name) != ''
        ORDER BY s.module_id, s.original_name, semantic_name, export_name
        "
    );
    let mut statement = connection
        .prepare(sql.as_str())
        .map_err(SymbolNamesError::QuerySymbolNames)?;
    let rows = statement
        .query_map(params![i64::from(project_id)], |row| {
            let module_id = row.get::<_, i64>(0)?;
            Ok(SymbolNameRow {
                module_id: u32::try_from(module_id).map_err(|source| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Integer,
                        Box::new(source),
                    )
                })?,
                original_name: row.get(1)?,
                semantic_name: row.get(2)?,
                semantic_name_source: row.get(3)?,
                export_name: row.get(4)?,
                scope_level: row.get(5)?,
            })
        })
        .map_err(SymbolNamesError::QuerySymbolNames)?;
    collect_sqlite_rows(rows).map_err(SymbolNamesError::QuerySymbolNames)
}

fn print_symbol_rows(rows: &[SymbolNameRow]) {
    println!(
        "module_id\toriginal_name\tsemantic_name\tsemantic_name_source\texport_name\tscope_level"
    );
    for row in rows {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            row.module_id,
            row.original_name,
            row.semantic_name.as_deref().unwrap_or(""),
            row.semantic_name_source.as_deref().unwrap_or(""),
            row.export_name.as_deref().unwrap_or(""),
            row.scope_level.as_deref().unwrap_or("")
        );
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, params};

    use super::{
        SymbolNameClearSpec, SymbolNameSetSpec, SymbolNamesArgs, SymbolNamesError,
        SymbolNamesOutcome, parse_batch_operations, symbol_names_from_connection,
    };

    fn args_with_set(apply: bool) -> SymbolNamesArgs {
        SymbolNamesArgs {
            input: "fixture.db".into(),
            project_id: 1,
            list: false,
            apply,
            sets: vec![SymbolNameSetSpec {
                module_id: 10,
                original_name: "$F1".to_string(),
                semantic_name: "createClient".to_string(),
            }],
            clears: Vec::new(),
            batch: None,
        }
    }

    fn create_fixture() -> Connection {
        let connection = Connection::open_in_memory().expect("open sqlite");
        connection
            .execute_batch(
                r"
                CREATE TABLE projects (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                CREATE TABLE source_files (id INTEGER PRIMARY KEY, file_path TEXT NOT NULL);
                CREATE TABLE project_files (project_id INTEGER NOT NULL, file_id INTEGER NOT NULL);
                CREATE TABLE modules (
                    id INTEGER PRIMARY KEY,
                    file_id INTEGER,
                    original_name TEXT NOT NULL,
                    semantic_name TEXT,
                    module_category TEXT,
                    package_name TEXT,
                    package_version TEXT,
                    byte_start INTEGER,
                    byte_end INTEGER
                );
                CREATE TABLE symbols (
                    module_id INTEGER,
                    semantic_name TEXT,
                    export_name TEXT,
                    original_name TEXT,
                    scope_level TEXT
                );
                INSERT INTO projects (id, name) VALUES (1, 'fixture');
                INSERT INTO source_files (id, file_path) VALUES (1, 'src/index.js');
                INSERT INTO project_files (project_id, file_id) VALUES (1, 1);
                INSERT INTO modules (id, file_id, original_name, semantic_name, module_category)
                VALUES (10, 1, 'entry', 'src/index.js', 'application');
                INSERT INTO symbols (module_id, semantic_name, export_name, original_name, scope_level)
                VALUES
                    (10, NULL, NULL, '$F1', 'module'),
                    (10, NULL, NULL, 'otherName', 'module'),
                    (10, NULL, NULL, 'globalName', 'global');
                ",
            )
            .expect("create fixture schema");
        connection
    }

    #[test]
    fn batch_parser_accepts_set_clear_and_header() {
        let operations = parse_batch_operations(
            "action\tmodule_id\toriginal_name\tsemantic_name\nset\t10\t$F1\tcreateClient\nclear\t10\toldName\n",
        )
        .expect("batch should parse");

        assert_eq!(operations.len(), 2);
    }

    #[test]
    fn dry_run_does_not_write_symbol_name() {
        let mut connection = create_fixture();
        let outcome = symbol_names_from_connection(&mut connection, &args_with_set(false))
            .expect("dry-run should validate");

        assert_eq!(outcome.requested_changes, 1);
        assert_eq!(outcome.written_changes, 0);
        let stored: Option<String> = connection
            .query_row(
                "SELECT semantic_name FROM symbols WHERE module_id = 10 AND original_name = '$F1'",
                [],
                |row| row.get(0),
            )
            .expect("query symbol");
        assert_eq!(stored, None);
    }

    #[test]
    fn apply_sets_semantic_name_and_adds_source_column() {
        let mut connection = create_fixture();
        let outcome = symbol_names_from_connection(&mut connection, &args_with_set(true))
            .expect("apply should write");

        assert_eq!(outcome.written_changes, 1);
        let (name, source): (String, String) = connection
            .query_row(
                "SELECT semantic_name, semantic_name_source FROM symbols WHERE module_id = 10 AND original_name = '$F1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("query symbol");
        assert_eq!(name, "createClient");
        assert_eq!(source, "manual");
    }

    #[test]
    fn clear_removes_semantic_name_and_source() {
        let mut connection = create_fixture();
        let mut args = args_with_set(true);
        symbol_names_from_connection(&mut connection, &args).expect("set should write");
        args.sets.clear();
        args.clears.push(SymbolNameClearSpec {
            module_id: 10,
            original_name: "$F1".to_string(),
        });

        symbol_names_from_connection(&mut connection, &args).expect("clear should write");
        let (name, source): (Option<String>, Option<String>) = connection
            .query_row(
                "SELECT semantic_name, semantic_name_source FROM symbols WHERE module_id = 10 AND original_name = '$F1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("query symbol");
        assert_eq!(name, None);
        assert_eq!(source, None);
    }

    #[test]
    fn rejects_name_collision_with_existing_original() {
        let mut connection = create_fixture();
        let mut args = args_with_set(false);
        args.sets[0].semantic_name = "otherName".to_string();

        let error = symbol_names_from_connection(&mut connection, &args)
            .expect_err("collision should be rejected");
        assert!(matches!(error, SymbolNamesError::NameCollision { .. }));
    }

    #[test]
    fn list_reads_without_source_column() {
        let mut connection = create_fixture();
        let args = SymbolNamesArgs {
            input: "fixture.db".into(),
            project_id: 1,
            list: true,
            apply: false,
            sets: Vec::new(),
            clears: Vec::new(),
            batch: None,
        };

        let SymbolNamesOutcome { listed, .. } =
            symbol_names_from_connection(&mut connection, &args).expect("list should work");
        assert_eq!(listed.len(), 3);
        assert!(listed.iter().all(|row| row.semantic_name_source.is_none()));
    }

    #[test]
    fn update_touches_all_duplicate_symbol_rows() {
        let mut connection = create_fixture();
        connection
            .execute(
                "INSERT INTO symbols (module_id, semantic_name, export_name, original_name, scope_level) VALUES (?1, NULL, NULL, ?2, 'module')",
                params![10_i64, "$F1"],
            )
            .expect("insert duplicate");

        let outcome = symbol_names_from_connection(&mut connection, &args_with_set(true))
            .expect("apply should write duplicate rows");
        assert_eq!(outcome.written_changes, 2);
    }
}
