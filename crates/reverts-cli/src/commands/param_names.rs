//! `param-names` command: accept explicit semantic names for function PARAMETERS
//! of decompiled functions, keyed by (file, function name, parameter index).
//! Consumed by `generate` (via `semantic_function_param_names`) and applied by
//! the emitter's function-param pass before emission. Batch-only: the agent
//! naming workflow produces a TSV of accepts.
//!
//! Two things make agent batches land without an external pre-processing script:
//!
//! - Name resolution. The param matcher locates a function by its ORIGINAL
//!   (pre-rename) name, because parameter transfer runs before name-scope renames
//!   rewrite the binding. Agents read the EMITTED source and naturally refer to a
//!   function/class by its readable name. So the container part of the key (the
//!   class for `Class.method`, or the whole name for a top-level function) is
//!   resolved back to its original through `semantic_binding_names` for that file.
//!   A name that was never renamed resolves to itself; one that maps to several
//!   originals is ambiguous and skipped.
//!
//! - Lenient batches. With `--lenient`, rows that fail the naming gate, resolve
//!   ambiguously, or duplicate an earlier row are skipped and counted rather than
//!   aborting the whole batch — the right default for noisy bulk agent output.

use std::collections::{BTreeMap, BTreeSet};
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
    /// Skip (and count) rows that fail the gate, resolve ambiguously, or
    /// duplicate, instead of aborting the whole batch.
    #[arg(long)]
    pub lenient: bool,
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

#[derive(Default)]
struct RunOutcome {
    requested: usize,
    written: usize,
    skipped: usize,
}

pub(crate) fn run(args: ParamNamesArgs) -> Result<(), CliRunError> {
    let outcome = param_names_from_sqlite(&args)?;
    let skipped = if outcome.skipped > 0 {
        format!(" ({} skipped)", outcome.skipped)
    } else {
        String::new()
    };
    if args.apply {
        println!(
            "updated function-parameter names for project {}: {} change(s) written{skipped}",
            args.project_id, outcome.written
        );
    } else {
        println!(
            "dry-run: {} function-parameter name(s) would apply for project {}{skipped}; pass --apply to persist",
            outcome.requested, args.project_id
        );
    }
    Ok(())
}

fn param_names_from_sqlite(args: &ParamNamesArgs) -> Result<RunOutcome, CliRunError> {
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
    let resolver = ContainerResolver::load(&connection, args.project_id)?;

    let mut outcome = RunOutcome::default();
    let mut seen = BTreeSet::<(String, String, u32)>::new();
    let mut resolved = Vec::<ParamNameSpec>::new();
    for spec in specs {
        if spec.file_path.trim().is_empty()
            || spec.function_name.trim().is_empty()
            || spec.semantic_name.trim().is_empty()
        {
            // A blank key field is a malformed batch, not a low-confidence name:
            // fail hard even under --lenient.
            return Err(CliRunError::ParamNames(
                "file_path, function_name, and semantic_name must be non-empty".to_string(),
            ));
        }
        // Resolve the readable container in the key back to its original name so
        // the matcher (which sees pre-rename names) can locate the function.
        let key = match resolver.resolve_key(&spec.file_path, &spec.function_name) {
            ResolvedKey::Key(key) => key,
            ResolvedKey::Ambiguous => {
                if args.lenient {
                    outcome.skipped += 1;
                    continue;
                }
                return Err(CliRunError::ParamNames(format!(
                    "function key {} in {} resolves to multiple original names; disambiguate or use --lenient",
                    spec.function_name, spec.file_path
                )));
            }
        };
        if let Err(error) = validate_name_acceptance(
            // Parameters have no minified "original" to gate on, so pass an empty
            // original: that keeps `semantic != original`, which is what makes the
            // vocabulary gate actually run (it is skipped for unchanged names).
            "",
            spec.semantic_name.as_str(),
            args.origin.as_str(),
            spec.evidence.as_deref().or(args.evidence.as_deref()),
            NamingGateMode::LocalBinding,
        ) {
            if args.lenient {
                outcome.skipped += 1;
                continue;
            }
            return Err(CliRunError::ParamNames(error.message()));
        }
        if !seen.insert((spec.file_path.clone(), key.clone(), spec.param_index)) {
            outcome.skipped += 1;
            continue;
        }
        resolved.push(ParamNameSpec {
            function_name: key,
            ..spec
        });
    }

    outcome.requested = resolved.len();
    if !args.apply {
        return Ok(outcome);
    }
    ensure_table(&connection)?;
    for spec in &resolved {
        outcome.written += connection
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
    Ok(outcome)
}

/// Resolves the readable container portion of a function key to its original
/// (pre-rename) name, per file. Keyed by (normalized file path, readable name).
struct ContainerResolver {
    by_file_name: BTreeMap<(String, String), BTreeSet<String>>,
}

enum ResolvedKey {
    Key(String),
    Ambiguous,
}

impl ContainerResolver {
    fn load(connection: &Connection, project_id: u32) -> Result<Self, CliRunError> {
        let mut by_file_name = BTreeMap::<(String, String), BTreeSet<String>>::new();
        if !table_exists(connection, "semantic_binding_names")? {
            return Ok(Self { by_file_name });
        }
        let mut statement = connection
            .prepare(
                r"
                SELECT file_path, semantic_name, original_name
                FROM semantic_binding_names
                WHERE project_id = ?1 AND accepted = 1
                ",
            )
            .map_err(|source| CliRunError::ParamNames(source.to_string()))?;
        let rows = statement
            .query_map(params![i64::from(project_id)], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|source| CliRunError::ParamNames(source.to_string()))?;
        for row in rows {
            let (file_path, semantic, original) =
                row.map_err(|source| CliRunError::ParamNames(source.to_string()))?;
            by_file_name
                .entry((normalize_island_path(&file_path), semantic))
                .or_default()
                .insert(original);
        }
        Ok(Self { by_file_name })
    }

    /// Resolve the container (text before the first `.`, or the whole key for a
    /// top-level function) from readable to original. A never-renamed container
    /// resolves to itself; one mapping to several originals is ambiguous.
    fn resolve_key(&self, file_path: &str, key: &str) -> ResolvedKey {
        let file = normalize_island_path(file_path);
        let (container, member) = match key.split_once('.') {
            Some((container, member)) => (container, Some(member)),
            None => (key, None),
        };
        let original = match self.by_file_name.get(&(file, container.to_string())) {
            None => container.to_string(),
            Some(originals) if originals.len() == 1 => {
                originals.iter().next().cloned().unwrap_or_default()
            }
            Some(_) => return ResolvedKey::Ambiguous,
        };
        ResolvedKey::Key(match member {
            Some(member) => format!("{original}.{member}"),
            None => original,
        })
    }
}

/// Drop the island process-prefix so binding-name paths and emitted/agent paths
/// (e.g. `auth/oauth.ts`) compare equal; tolerates the doubled-prefix artifact.
fn normalize_island_path(path: &str) -> String {
    let mut rest = path;
    while let Some(stripped) = rest.strip_prefix("modules/island/") {
        rest = stripped;
    }
    rest.to_string()
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

fn table_exists(connection: &Connection, name: &str) -> Result<bool, CliRunError> {
    let found: Option<i64> = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![name],
            |row| row.get(0),
        )
        .optional()
        .map_err(|source| CliRunError::ParamNames(source.to_string()))?;
    Ok(found.is_some())
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

    fn seed_binding(db: &std::path::Path, file: &str, original: &str, semantic: &str) {
        let connection = Connection::open(db).expect("open db");
        connection
            .execute_batch(
                r"
                CREATE TABLE IF NOT EXISTS semantic_binding_names (
                    project_id INTEGER NOT NULL, file_path TEXT NOT NULL,
                    original_name TEXT NOT NULL, binding_index INTEGER,
                    binding_key TEXT NOT NULL, semantic_name TEXT NOT NULL,
                    origin TEXT NOT NULL, evidence TEXT,
                    accepted INTEGER NOT NULL DEFAULT 1
                );
                ",
            )
            .expect("binding schema");
        connection
            .execute(
                "INSERT INTO semantic_binding_names \
                 (project_id, file_path, original_name, binding_key, semantic_name, origin, accepted) \
                 VALUES (1, ?1, ?2, ?2, ?3, 'agent', 1)",
                params![file, original, semantic],
            )
            .expect("insert binding");
    }

    fn args(db: &std::path::Path, batch: &std::path::Path, apply: bool, lenient: bool) -> ParamNamesArgs {
        ParamNamesArgs {
            input: db.to_path_buf(),
            project_id: 1,
            apply,
            origin: "agent".to_string(),
            evidence: None,
            batch: batch.to_path_buf(),
            lenient,
        }
    }

    fn stored_name(db: &std::path::Path, function: &str, index: i64) -> Option<String> {
        let connection = Connection::open(db).expect("reopen");
        connection
            .query_row(
                "SELECT semantic_name FROM semantic_function_param_names \
                 WHERE function_name = ?1 AND param_index = ?2",
                params![function, index],
                |row| row.get(0),
            )
            .optional()
            .expect("query")
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

        let outcome = param_names_from_sqlite(&args(&db, &batch, true, false)).expect("apply");
        assert_eq!(outcome.requested, 1);
        assert_eq!(outcome.written, 1);
        assert_eq!(stored_name(&db, "handleRequest", 0).as_deref(), Some("requestOptions"));
    }

    #[test]
    fn resolves_readable_class_to_original_for_method_key() {
        let temp = tempdir().expect("temp dir");
        let db = temp.path().join("project.sqlite");
        create_db(&db);
        // Class BatchQueue was renamed from minified gPe.
        seed_binding(&db, "batch-queue.ts", "gPe", "BatchQueue");
        let batch = temp.path().join("batch.tsv");
        fs::write(
            &batch,
            "accept\tbatch-queue.ts\tBatchQueue.enqueue\t0\titems\titems appended to the queue\n",
        )
        .expect("write batch");

        let outcome = param_names_from_sqlite(&args(&db, &batch, true, false)).expect("apply");
        assert_eq!(outcome.written, 1);
        // Stored under the ORIGINAL class name so the matcher can locate it.
        assert_eq!(stored_name(&db, "gPe.enqueue", 0).as_deref(), Some("items"));
        assert!(stored_name(&db, "BatchQueue.enqueue", 0).is_none());
    }

    #[test]
    fn never_renamed_container_resolves_to_itself() {
        let temp = tempdir().expect("temp dir");
        let db = temp.path().join("project.sqlite");
        create_db(&db);
        let batch = temp.path().join("batch.tsv");
        fs::write(
            &batch,
            "accept\tutil.ts\tparseConfig\t0\tsource\tsource text to parse\n",
        )
        .expect("write batch");

        let outcome = param_names_from_sqlite(&args(&db, &batch, true, false)).expect("apply");
        assert_eq!(outcome.written, 1);
        assert_eq!(stored_name(&db, "parseConfig", 0).as_deref(), Some("source"));
    }

    #[test]
    fn lenient_skips_ungated_and_duplicate_rows() {
        let temp = tempdir().expect("temp dir");
        let db = temp.path().join("project.sqlite");
        create_db(&db);
        let batch = temp.path().join("batch.tsv");
        fs::write(
            &batch,
            // row 1 ok; row 2 fails gate (zebra unsupported); row 3 duplicates row 1.
            "accept\ta.ts\tf\t0\tsource\tsource text\n\
             accept\ta.ts\tg\t0\tzebraThing\tunsupported evidence\n\
             accept\ta.ts\tf\t0\tsource\tsource text again\n",
        )
        .expect("write batch");

        let outcome = param_names_from_sqlite(&args(&db, &batch, true, true)).expect("apply");
        assert_eq!(outcome.written, 1);
        assert_eq!(outcome.skipped, 2);
    }

    #[test]
    fn strict_mode_rejects_ungated_row() {
        let temp = tempdir().expect("temp dir");
        let db = temp.path().join("project.sqlite");
        create_db(&db);
        let batch = temp.path().join("batch.tsv");
        fs::write(&batch, "accept\ta.ts\tf\t0\tzebraQuokka\tno support\n").expect("write batch");

        let result = param_names_from_sqlite(&args(&db, &batch, true, false));
        assert!(matches!(result, Err(CliRunError::ParamNames(_))));
    }

    #[test]
    fn dry_run_writes_nothing() {
        let temp = tempdir().expect("temp dir");
        let db = temp.path().join("project.sqlite");
        create_db(&db);
        let batch = temp.path().join("batch.tsv");
        fs::write(&batch, "accept\ta.ts\tf\t0\tsource\tsource text\n").expect("write batch");

        let outcome = param_names_from_sqlite(&args(&db, &batch, false, false)).expect("dry run");
        assert_eq!(outcome.requested, 1);
        assert_eq!(outcome.written, 0);
    }

    #[test]
    fn rejects_malformed_batch_row() {
        let temp = tempdir().expect("temp dir");
        let db = temp.path().join("project.sqlite");
        create_db(&db);
        let batch = temp.path().join("batch.tsv");
        fs::write(&batch, "accept\ta.ts\tf\n").expect("write batch");

        let result = param_names_from_sqlite(&args(&db, &batch, true, false));
        assert!(matches!(result, Err(CliRunError::ParamNames(_))));
    }
}
