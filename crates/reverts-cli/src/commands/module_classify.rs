//! `module-classify` command: record per-module classification
//! (application vs third-party vs runtime-glue) so `naming-progress` and the
//! naming plan can exclude non-first-party modules from the naming denominator.
//!
//! Deterministic signals stay authoritative: real package externalization is
//! decided by the fingerprint matcher (`package_attributions`), never here.
//! This table only records the *naming-eligibility triage* — `--auto` fills in
//! the cheap path signal (vendored `node_modules` -> third-party) and `--batch`
//! records an agent's verdicts for the residue. No classification here ever
//! emits a bare `import`.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::Duration;

use reverts_ir::ModuleId;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::args::ModuleClassifyArgs;
use crate::errors::{CliRunError, ModuleClassifyError};
use crate::sqlite_table_exists;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleClassification {
    Application,
    ThirdPartyLibrary,
    RuntimeGlue,
}

impl ModuleClassification {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Application => "application",
            Self::ThirdPartyLibrary => "third-party-library",
            Self::RuntimeGlue => "runtime-glue",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "application" => Some(Self::Application),
            "third-party-library" => Some(Self::ThirdPartyLibrary),
            "runtime-glue" => Some(Self::RuntimeGlue),
            _ => None,
        }
    }

    /// Whether a module with this classification is excluded from first-party
    /// naming work.
    #[must_use]
    pub fn excludes_from_naming(self) -> bool {
        matches!(self, Self::ThirdPartyLibrary | Self::RuntimeGlue)
    }
}

pub const MODULE_CLASSIFY_ORIGIN_DETERMINISTIC: &str = "deterministic";

#[derive(Debug, Clone, PartialEq)]
pub struct ModuleClassificationRow {
    pub module_id: u32,
    pub classification: ModuleClassification,
    pub origin: String,
    pub confidence: Option<f64>,
    pub evidence: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct ClassifySpec {
    module_id: u32,
    classification: ModuleClassification,
    evidence: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModuleClassifyOutcome {
    pub project_id: u32,
    pub listed: Vec<ModuleClassificationRow>,
    pub requested_changes: usize,
    pub written_changes: usize,
    pub apply: bool,
}

pub(crate) fn run(args: ModuleClassifyArgs) -> Result<(), CliRunError> {
    let outcome = module_classify_from_sqlite(&args).map_err(CliRunError::ModuleClassify)?;
    if args.list {
        println!("module_id\tclassification\torigin\tconfidence\tevidence");
        for row in &outcome.listed {
            println!(
                "{}\t{}\t{}\t{}\t{}",
                row.module_id,
                row.classification.as_str(),
                row.origin,
                row.confidence
                    .map_or_else(String::new, |value| format!("{value:.2}")),
                row.evidence.as_deref().unwrap_or("")
            );
        }
    } else if outcome.apply {
        println!(
            "classified modules for project {}: {} change(s) written",
            outcome.project_id, outcome.written_changes
        );
    } else {
        println!(
            "dry-run: would classify {} module(s) for project {}; pass --apply to persist",
            outcome.requested_changes, outcome.project_id
        );
    }
    Ok(())
}

pub fn module_classify_from_sqlite(
    args: &ModuleClassifyArgs,
) -> Result<ModuleClassifyOutcome, ModuleClassifyError> {
    let flags = if args.apply {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let mut connection =
        Connection::open_with_flags(args.input.as_path(), flags).map_err(|source| {
            ModuleClassifyError::OpenDatabase {
                path: args.input.clone(),
                source,
            }
        })?;
    connection
        .busy_timeout(Duration::from_secs(30))
        .map_err(ModuleClassifyError::ConfigureDatabase)?;
    module_classify_from_connection(&mut connection, args)
}

pub fn module_classify_from_connection(
    connection: &mut Connection,
    args: &ModuleClassifyArgs,
) -> Result<ModuleClassifyOutcome, ModuleClassifyError> {
    ensure_project_exists(connection, args.project_id)?;
    if args.list {
        return Ok(ModuleClassifyOutcome {
            project_id: args.project_id,
            listed: load_classification_rows(connection, args.project_id)?,
            requested_changes: 0,
            written_changes: 0,
            apply: false,
        });
    }

    // Auto (deterministic path signal) and batch (agent verdicts) are recorded
    // under distinct origins so provenance stays auditable.
    let auto_specs = if args.auto {
        auto_classify(connection, args.project_id)?
    } else {
        Vec::new()
    };
    let batch_specs = if let Some(batch) = &args.batch {
        load_batch_specs(batch.as_path())?
    } else {
        Vec::new()
    };
    let requested_changes = auto_specs.len() + batch_specs.len();

    let written_changes = if args.apply {
        ensure_module_classification_table(connection)?;
        let transaction = connection
            .transaction()
            .map_err(ModuleClassifyError::ConfigureDatabase)?;
        let mut written = 0_usize;
        for spec in &auto_specs {
            written += upsert_classification(
                &transaction,
                args.project_id,
                MODULE_CLASSIFY_ORIGIN_DETERMINISTIC,
                spec,
            )?;
        }
        for spec in &batch_specs {
            written += upsert_classification(&transaction, args.project_id, &args.origin, spec)?;
        }
        transaction
            .commit()
            .map_err(ModuleClassifyError::WriteClassification)?;
        written
    } else {
        0
    };

    Ok(ModuleClassifyOutcome {
        project_id: args.project_id,
        listed: Vec::new(),
        requested_changes,
        written_changes,
        apply: args.apply,
    })
}

/// Deterministic, path-based triage: modules whose semantic path is vendored
/// `node_modules` source are third-party. The fingerprint matcher remains the
/// authority for real externalization; this only marks naming-eligibility.
fn auto_classify(
    connection: &Connection,
    project_id: u32,
) -> Result<Vec<ClassifySpec>, ModuleClassifyError> {
    let mut statement = connection
        .prepare(
            r"
            SELECT m.id, COALESCE(NULLIF(TRIM(m.semantic_name), ''), m.original_name)
            FROM modules m
            JOIN project_files pf ON pf.file_id = m.file_id
            WHERE pf.project_id = ?1
            ORDER BY m.id
            ",
        )
        .map_err(ModuleClassifyError::QueryClassification)?;
    let rows = statement
        .query_map(params![i64::from(project_id)], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(ModuleClassifyError::QueryClassification)?;
    let mut specs = Vec::new();
    for row in rows {
        let (module_id, path) = row.map_err(ModuleClassifyError::QueryClassification)?;
        if path_is_vendored(&path) {
            let module_id =
                u32::try_from(module_id).map_err(|_| ModuleClassifyError::InvalidDatabaseId {
                    owner: "module.id",
                    value: module_id,
                })?;
            specs.push(ClassifySpec {
                module_id,
                classification: ModuleClassification::ThirdPartyLibrary,
                evidence: Some(format!("vendored path: {path}")),
            });
        }
    }
    Ok(specs)
}

fn path_is_vendored(path: &str) -> bool {
    path.contains("node_modules/") || path.starts_with("node_modules")
}

fn load_batch_specs(path: &Path) -> Result<Vec<ClassifySpec>, ModuleClassifyError> {
    let content = if path == Path::new("-") {
        std::io::read_to_string(std::io::stdin()).map_err(ModuleClassifyError::ReadBatch)?
    } else {
        fs::read_to_string(path).map_err(ModuleClassifyError::ReadBatch)?
    };
    let mut specs = Vec::new();
    for (line_index, line) in content.lines().enumerate() {
        let line_number = line_index + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.first().copied() == Some("module_id") {
            continue;
        }
        let (module_id, classification, evidence) = match fields.as_slice() {
            [module_id, classification] => (*module_id, *classification, None),
            [module_id, classification, evidence] => (*module_id, *classification, Some(*evidence)),
            _ => {
                return Err(ModuleClassifyError::InvalidBatchLine {
                    line: line_number,
                    message: "expected MODULE_ID<TAB>CLASSIFICATION[<TAB>EVIDENCE]".to_string(),
                });
            }
        };
        let module_id = module_id
            .parse::<u32>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(|| ModuleClassifyError::InvalidBatchLine {
                line: line_number,
                message: format!("invalid module id {module_id}"),
            })?;
        let classification = ModuleClassification::parse(classification).ok_or_else(|| {
            ModuleClassifyError::InvalidBatchLine {
                line: line_number,
                message: format!(
                    "invalid classification {classification}; expected application | third-party-library | runtime-glue"
                ),
            }
        })?;
        // Note: module ids come from the post-extraction module space
        // (symbol-index / naming-plan), which includes synthetic bundle-split
        // modules with no `modules` DB row. We therefore do not validate
        // against the DB `modules` table; a non-matching id simply never
        // excludes anything in `naming-progress`.
        specs.push(ClassifySpec {
            module_id,
            classification,
            evidence: evidence.map(str::to_string),
        });
    }
    Ok(specs)
}

fn ensure_project_exists(
    connection: &Connection,
    project_id: u32,
) -> Result<(), ModuleClassifyError> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM projects WHERE id = ?1",
            params![i64::from(project_id)],
            |_row| Ok(()),
        )
        .optional()
        .map_err(ModuleClassifyError::QueryClassification)?
        .is_some();
    if exists {
        Ok(())
    } else {
        Err(ModuleClassifyError::ProjectNotFound { project_id })
    }
}

fn ensure_module_classification_table(connection: &Connection) -> Result<(), ModuleClassifyError> {
    connection
        .execute_batch(
            r"
            CREATE TABLE IF NOT EXISTS module_classification (
                project_id INTEGER NOT NULL,
                module_id INTEGER NOT NULL,
                classification TEXT NOT NULL,
                origin TEXT NOT NULL,
                confidence REAL,
                evidence TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (project_id, module_id, origin)
            );
            ",
        )
        .map_err(ModuleClassifyError::WriteClassification)
}

fn upsert_classification(
    connection: &Connection,
    project_id: u32,
    origin: &str,
    spec: &ClassifySpec,
) -> Result<usize, ModuleClassifyError> {
    connection
        .execute(
            r"
            INSERT INTO module_classification (
                project_id, module_id, classification, origin, evidence
            ) VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(project_id, module_id, origin)
            DO UPDATE SET
                classification = excluded.classification,
                evidence = COALESCE(excluded.evidence, module_classification.evidence)
            ",
            params![
                i64::from(project_id),
                i64::from(spec.module_id),
                spec.classification.as_str(),
                origin,
                spec.evidence.as_deref(),
            ],
        )
        .map_err(ModuleClassifyError::WriteClassification)
}

fn load_classification_rows(
    connection: &Connection,
    project_id: u32,
) -> Result<Vec<ModuleClassificationRow>, ModuleClassifyError> {
    if !sqlite_table_exists(connection, "module_classification")
        .map_err(ModuleClassifyError::QueryClassification)?
    {
        return Ok(Vec::new());
    }
    let mut statement = connection
        .prepare(
            r"
            SELECT module_id, classification, origin, confidence, evidence
            FROM module_classification
            WHERE project_id = ?1
            ORDER BY module_id, origin
            ",
        )
        .map_err(ModuleClassifyError::QueryClassification)?;
    let rows = statement
        .query_map(params![i64::from(project_id)], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<f64>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })
        .map_err(ModuleClassifyError::QueryClassification)?;
    let mut out = Vec::new();
    for row in rows {
        let (module_id, classification, origin, confidence, evidence) =
            row.map_err(ModuleClassifyError::QueryClassification)?;
        let Some(classification) = ModuleClassification::parse(&classification) else {
            continue;
        };
        let module_id =
            u32::try_from(module_id).map_err(|_| ModuleClassifyError::InvalidDatabaseId {
                owner: "module_classification.module_id",
                value: module_id,
            })?;
        out.push(ModuleClassificationRow {
            module_id,
            classification,
            origin,
            confidence,
            evidence,
        });
    }
    Ok(out)
}

/// Module ids excluded from first-party naming because some classification
/// (any origin) marks them third-party or runtime-glue. Read by
/// `naming-progress` and `naming-plan`. Returns an empty set when the table
/// does not exist.
pub fn excluded_module_ids_from_connection(
    connection: &Connection,
    project_id: u32,
) -> Result<BTreeSet<ModuleId>, ModuleClassifyError> {
    let rows = load_classification_rows(connection, project_id)?;
    Ok(rows
        .into_iter()
        .filter(|row| row.classification.excludes_from_naming())
        .map(|row| ModuleId(row.module_id))
        .collect())
}

pub fn excluded_module_ids_from_sqlite(
    path: &Path,
    project_id: u32,
) -> Result<BTreeSet<ModuleId>, ModuleClassifyError> {
    let connection =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|source| {
            ModuleClassifyError::OpenDatabase {
                path: path.to_path_buf(),
                source,
            }
        })?;
    excluded_module_ids_from_connection(&connection, project_id)
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::{
        ModuleClassification, excluded_module_ids_from_connection, module_classify_from_connection,
    };
    use crate::args::ModuleClassifyArgs;

    fn fixture() -> Connection {
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
                    module_category TEXT
                );
                INSERT INTO projects (id, name) VALUES (1, 'fixture');
                INSERT INTO source_files (id, file_path) VALUES (1, 'bundle.js');
                INSERT INTO project_files (project_id, file_id) VALUES (1, 1);
                INSERT INTO modules (id, file_id, original_name, semantic_name, module_category)
                VALUES
                    (10, 1, 'app', 'src/app.ts', 'application'),
                    (11, 1, 'ws', 'node_modules/ws/index.js', 'application');
                ",
            )
            .expect("schema");
        connection
    }

    fn args(auto: bool, apply: bool, batch: Option<std::path::PathBuf>) -> ModuleClassifyArgs {
        ModuleClassifyArgs {
            input: "fixture.db".into(),
            project_id: 1,
            list: false,
            auto,
            apply,
            origin: "agent".to_string(),
            batch,
        }
    }

    #[test]
    fn auto_classifies_vendored_paths_as_third_party() {
        let mut connection = fixture();
        let outcome = module_classify_from_connection(&mut connection, &args(true, true, None))
            .expect("auto apply");
        // Only the node_modules module is auto-classified.
        assert_eq!(outcome.written_changes, 1);
        let excluded = excluded_module_ids_from_connection(&connection, 1).expect("excluded");
        assert_eq!(excluded.len(), 1);
        assert!(excluded.contains(&reverts_ir::ModuleId(11)));
    }

    #[test]
    fn dry_run_writes_nothing() {
        let mut connection = fixture();
        let outcome = module_classify_from_connection(&mut connection, &args(true, false, None))
            .expect("auto dry-run");
        assert_eq!(outcome.requested_changes, 1);
        assert_eq!(outcome.written_changes, 0);
    }

    #[test]
    fn classification_excludes_only_non_application() {
        assert!(ModuleClassification::ThirdPartyLibrary.excludes_from_naming());
        assert!(ModuleClassification::RuntimeGlue.excludes_from_naming());
        assert!(!ModuleClassification::Application.excludes_from_naming());
    }
}
