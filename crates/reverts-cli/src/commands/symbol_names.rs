//! `symbol-names` command: inspect, propose, and accept symbol semantic names.
//!
//! `--propose` records a naming suggestion without changing emission.
//! `--accept` records the suggestion and updates the active
//! `symbols.semantic_name` input field, so the next `generate-project-v2` run
//! carries the accepted name through the normal input → analyze → plan → emit
//! path. The verbs are `propose` / `accept` / `clear-active`, matching the batch
//! TSV ops and the `binding-names` schema (`op⇥key⇥original⇥semantic⇥[evidence]`).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::args::{parse_args_with_name, parse_project_id};
use crate::commands::naming_gates::{
    NamingGateError, NamingGateMode, is_automated_name_origin, validate_name_acceptance,
};
use crate::errors::{CliError, CliRunError, SymbolNamesError};
use crate::{collect_sqlite_rows, sqlite_table_exists, sqlite_table_has_column};

pub const SYMBOL_NAME_ORIGIN_AGENT: &str = "agent";

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
    #[arg(long, default_value = SYMBOL_NAME_ORIGIN_AGENT)]
    pub origin: String,
    #[arg(long)]
    pub evidence: Option<String>,
    /// Record a naming suggestion without changing emitted output.
    #[arg(long = "propose", value_parser = parse_name_spec)]
    pub proposals: Vec<SymbolNameSpec>,
    /// Accept a semantic name and make it active for the next emit.
    #[arg(long = "accept", value_parser = parse_name_spec)]
    pub accepts: Vec<SymbolNameSpec>,
    /// Clear the active semantic name.
    #[arg(long = "clear-active", value_parser = parse_clear_spec)]
    pub clear_active: Vec<SymbolNameClearSpec>,
    #[arg(long)]
    pub batch: Option<PathBuf>,
    #[arg(long = "all-proposals")]
    pub all_proposals: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolNameSpec {
    pub module_id: u32,
    pub original_name: String,
    pub semantic_name: String,
    pub evidence: Option<String>,
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
        validate_args(parsed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolNamesOutcome {
    pub project_id: u32,
    pub listed: Vec<SymbolNameRow>,
    pub listed_proposals: Vec<SymbolNameProposalRow>,
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
pub struct SymbolNameProposalRow {
    pub module_id: u32,
    pub original_name: String,
    pub semantic_name: String,
    pub origin: String,
    pub accepted: bool,
    pub evidence: Option<String>,
    pub gate_status: Option<String>,
    pub gate_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SymbolNameOperation {
    Propose(SymbolNameSpec),
    Accept(SymbolNameSpec),
    ClearActive(SymbolNameClearSpec),
}

pub(crate) fn run(args: SymbolNamesArgs) -> Result<(), CliRunError> {
    let outcome = symbol_names_from_sqlite(&args).map_err(CliRunError::SymbolNames)?;
    if args.list && args.all_proposals {
        print_symbol_name_proposals(&outcome.listed_proposals);
    } else if args.list {
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

pub fn validate_args(args: SymbolNamesArgs) -> Result<SymbolNamesArgs, CliError> {
    if args.list
        && (!args.proposals.is_empty()
            || !args.accepts.is_empty()
            || !args.clear_active.is_empty()
            || args.batch.is_some()
            || args.apply)
    {
        return Err(CliError::UnknownArgument(
            "--list cannot be combined with mutations".to_string(),
        ));
    }
    if args.all_proposals && !args.list {
        return Err(CliError::UnknownArgument(
            "--all-proposals requires --list".to_string(),
        ));
    }
    if !args.list
        && args.proposals.is_empty()
        && args.accepts.is_empty()
        && args.clear_active.is_empty()
        && args.batch.is_none()
    {
        return Err(CliError::MissingArgument(
            "--list | --propose | --accept | --clear-active | --batch",
        ));
    }
    Ok(args)
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
            listed: if args.all_proposals {
                Vec::new()
            } else {
                load_symbol_name_rows(connection, args.project_id)?
            },
            listed_proposals: if args.all_proposals {
                load_symbol_name_proposals(connection, args.project_id)?
            } else {
                Vec::new()
            },
            requested_changes: 0,
            written_changes: 0,
            apply: false,
        });
    }

    let operations = collect_operations(args)?;
    validate_naming_gates(&operations, args.origin.as_str(), args.evidence.as_deref())?;

    let written_changes = if args.apply {
        ensure_semantic_name_source_column(connection)?;
        ensure_symbol_name_proposals_table(connection)?;
        let transaction = connection
            .transaction()
            .map_err(SymbolNamesError::ConfigureDatabase)?;
        validate_operation_targets(&transaction, args.project_id, &operations)?;
        validate_public_surface_policy(
            &transaction,
            args.project_id,
            &operations,
            args.origin.as_str(),
            args.evidence.as_deref(),
        )?;
        ensure_operation_symbol_rows(&transaction, &operations)?;
        validate_final_names(&transaction, args.project_id, &operations)?;
        let mut written = 0_usize;
        for operation in &operations {
            written += apply_operation(
                &transaction,
                args.project_id,
                args.origin.as_str(),
                args.evidence.as_deref(),
                operation,
            )?;
        }
        transaction
            .commit()
            .map_err(SymbolNamesError::WriteSymbolName)?;
        written
    } else {
        validate_operation_targets(connection, args.project_id, &operations)?;
        validate_public_surface_policy(
            connection,
            args.project_id,
            &operations,
            args.origin.as_str(),
            args.evidence.as_deref(),
        )?;
        validate_final_names(connection, args.project_id, &operations)?;
        0
    };

    Ok(SymbolNamesOutcome {
        project_id: args.project_id,
        listed: Vec::new(),
        listed_proposals: Vec::new(),
        requested_changes: operations.len(),
        written_changes,
        apply: args.apply,
    })
}

fn parse_name_spec(value: &str) -> Result<SymbolNameSpec, String> {
    let Some((target, semantic_name)) = value.split_once('=') else {
        return Err(format!(
            "invalid symbol name spec {value}; expected MODULE_ID:ORIGINAL=SEMANTIC"
        ));
    };
    let clear = parse_clear_spec(target)?;
    if semantic_name.trim().is_empty() {
        return Err(format!(
            "invalid symbol name spec {value}; semantic name is empty"
        ));
    }
    Ok(SymbolNameSpec {
        module_id: clear.module_id,
        original_name: clear.original_name,
        semantic_name: semantic_name.to_string(),
        evidence: None,
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
    operations.extend(
        args.proposals
            .iter()
            .cloned()
            .map(SymbolNameOperation::Propose),
    );
    operations.extend(
        args.accepts
            .iter()
            .cloned()
            .map(SymbolNameOperation::Accept),
    );
    operations.extend(
        args.clear_active
            .iter()
            .cloned()
            .map(SymbolNameOperation::ClearActive),
    );
    if let Some(batch) = &args.batch {
        operations.extend(load_batch_operations(batch.as_path())?);
    }
    validate_unique_operations(&operations)?;
    Ok(operations)
}

fn load_batch_operations(path: &Path) -> Result<Vec<SymbolNameOperation>, SymbolNamesError> {
    let content = if path == Path::new("-") {
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
            ["propose", module_id, original_name, semantic_name] => {
                operations.push(SymbolNameOperation::Propose(batch_name_spec(
                    module_id,
                    original_name,
                    semantic_name,
                    None,
                    line_number,
                    "propose",
                )?));
            }
            ["propose", module_id, original_name, semantic_name, evidence] => {
                operations.push(SymbolNameOperation::Propose(batch_name_spec(
                    module_id,
                    original_name,
                    semantic_name,
                    Some(*evidence),
                    line_number,
                    "propose",
                )?));
            }
            ["accept", module_id, original_name, semantic_name] => {
                operations.push(SymbolNameOperation::Accept(batch_name_spec(
                    module_id,
                    original_name,
                    semantic_name,
                    None,
                    line_number,
                    "accept",
                )?));
            }
            ["accept", module_id, original_name, semantic_name, evidence] => {
                operations.push(SymbolNameOperation::Accept(batch_name_spec(
                    module_id,
                    original_name,
                    semantic_name,
                    Some(*evidence),
                    line_number,
                    "accept",
                )?));
            }
            ["clear-active", module_id, original_name] => {
                if original_name.is_empty() {
                    return Err(SymbolNamesError::InvalidBatchLine {
                        line: line_number,
                        message: "clear-active requires non-empty original_name".to_string(),
                    });
                }
                operations.push(SymbolNameOperation::ClearActive(SymbolNameClearSpec {
                    module_id: parse_batch_u32(module_id, line_number)?,
                    original_name: (*original_name).to_string(),
                }));
            }
            _ => {
                return Err(SymbolNamesError::InvalidBatchLine {
                    line: line_number,
                    message: "expected tab-separated propose|accept MODULE_ID ORIGINAL SEMANTIC [EVIDENCE] or clear-active MODULE_ID ORIGINAL".to_string(),
                });
            }
        }
    }
    Ok(operations)
}

fn batch_name_spec(
    module_id: &str,
    original_name: &str,
    semantic_name: &str,
    evidence: Option<&str>,
    line: usize,
    action: &str,
) -> Result<SymbolNameSpec, SymbolNamesError> {
    if original_name.is_empty() || semantic_name.is_empty() {
        return Err(SymbolNamesError::InvalidBatchLine {
            line,
            message: format!("{action} requires non-empty original_name and semantic_name"),
        });
    }
    Ok(SymbolNameSpec {
        module_id: parse_batch_u32(module_id, line)?,
        original_name: original_name.to_string(),
        semantic_name: semantic_name.to_string(),
        evidence: evidence
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
    })
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
    let mut active_targets = BTreeSet::<(u32, &str)>::new();
    let mut proposals = BTreeSet::<(u32, &str, &str)>::new();
    for operation in operations {
        match operation {
            SymbolNameOperation::Propose(spec) => {
                if !proposals.insert((
                    spec.module_id,
                    spec.original_name.as_str(),
                    spec.semantic_name.as_str(),
                )) {
                    return Err(SymbolNamesError::ConflictingOperation {
                        module_id: spec.module_id,
                        original_name: spec.original_name.clone(),
                    });
                }
            }
            SymbolNameOperation::Accept(spec) => {
                if !active_targets.insert((spec.module_id, spec.original_name.as_str())) {
                    return Err(SymbolNamesError::ConflictingOperation {
                        module_id: spec.module_id,
                        original_name: spec.original_name.clone(),
                    });
                }
            }
            SymbolNameOperation::ClearActive(spec) => {
                if !active_targets.insert((spec.module_id, spec.original_name.as_str())) {
                    return Err(SymbolNamesError::ConflictingOperation {
                        module_id: spec.module_id,
                        original_name: spec.original_name.clone(),
                    });
                }
            }
        }
    }
    Ok(())
}

fn validate_naming_gates(
    operations: &[SymbolNameOperation],
    origin: &str,
    evidence: Option<&str>,
) -> Result<(), SymbolNamesError> {
    for operation in operations {
        let (original_name, semantic_name) = match operation {
            SymbolNameOperation::Propose(spec) | SymbolNameOperation::Accept(spec) => {
                (spec.original_name.as_str(), spec.semantic_name.as_str())
            }
            SymbolNameOperation::ClearActive(_) => continue,
        };
        let row_evidence = operation_evidence(operation).or(evidence);
        validate_name_acceptance(
            original_name,
            semantic_name,
            origin,
            row_evidence,
            NamingGateMode::Symbol,
        )
        .map_err(symbol_names_error_from_gate)?;
    }
    Ok(())
}

fn operation_evidence(operation: &SymbolNameOperation) -> Option<&str> {
    match operation {
        SymbolNameOperation::Propose(spec) | SymbolNameOperation::Accept(spec) => {
            spec.evidence.as_deref()
        }
        SymbolNameOperation::ClearActive(_) => None,
    }
}

fn validate_public_surface_policy(
    connection: &Connection,
    project_id: u32,
    operations: &[SymbolNameOperation],
    origin: &str,
    global_evidence: Option<&str>,
) -> Result<(), SymbolNamesError> {
    if !is_automated_name_origin(origin) {
        return Ok(());
    }
    for operation in operations {
        let SymbolNameOperation::Accept(spec) = operation else {
            continue;
        };
        if spec.semantic_name == spec.original_name {
            continue;
        }
        if !is_public_surface_symbol(connection, project_id, spec.module_id, &spec.original_name)? {
            continue;
        }
        let evidence = spec
            .evidence
            .as_deref()
            .or(global_evidence)
            .unwrap_or_default();
        if has_public_surface_structural_evidence(evidence) {
            continue;
        }
        return Err(SymbolNamesError::NamingGate {
            message: format!(
                "automated origin {origin} cannot auto-accept public-surface rename {}:{} -> {} without export/import/property structural evidence",
                spec.module_id, spec.original_name, spec.semantic_name
            ),
        });
    }
    Ok(())
}

fn is_public_surface_symbol(
    connection: &Connection,
    project_id: u32,
    module_id: u32,
    original_name: &str,
) -> Result<bool, SymbolNamesError> {
    connection
        .query_row(
            r"
            SELECT 1
            FROM symbols s
            JOIN modules m ON m.id = s.module_id
            JOIN project_files pf ON pf.file_id = m.file_id
            WHERE pf.project_id = ?1
              AND s.module_id = ?2
              AND s.original_name = ?3
              AND s.scope_level = 'module'
              AND NULLIF(TRIM(s.export_name), '') IS NOT NULL
            LIMIT 1
            ",
            params![i64::from(project_id), i64::from(module_id), original_name,],
            |_row| Ok(()),
        )
        .optional()
        .map(|value| value.is_some())
        .map_err(SymbolNamesError::QuerySymbolNames)
}

fn has_public_surface_structural_evidence(evidence: &str) -> bool {
    let normalized = evidence.to_ascii_lowercase();
    [
        "export:",
        "export_name",
        "export-name",
        "import:",
        "import_name",
        "import-name",
        "property:",
        "property_key",
        "property-key",
        "object_key",
        "object-key",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}
fn symbol_names_error_from_gate(error: NamingGateError) -> SymbolNamesError {
    match error {
        NamingGateError::InvalidIdentifier { name } => SymbolNamesError::InvalidSemanticName {
            semantic_name: name,
        },
        NamingGateError::PlaceholderIdentifier { name } => {
            SymbolNamesError::PlaceholderSemanticName {
                semantic_name: name,
            }
        }
        other => SymbolNamesError::NamingGate {
            message: other.message(),
        },
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
            SymbolNameOperation::Propose(spec) | SymbolNameOperation::Accept(spec) => {
                (spec.module_id, spec.original_name.as_str())
            }
            SymbolNameOperation::ClearActive(spec) => (spec.module_id, spec.original_name.as_str()),
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
        if count == 0 && matches!(operation, SymbolNameOperation::ClearActive(_)) {
            return Err(SymbolNamesError::UnknownSymbol {
                module_id,
                original_name: original_name.to_string(),
            });
        }
    }
    Ok(())
}

fn ensure_operation_symbol_rows(
    connection: &Connection,
    operations: &[SymbolNameOperation],
) -> Result<(), SymbolNamesError> {
    for operation in operations {
        let SymbolNameOperation::Accept(spec) = operation else {
            continue;
        };
        connection
            .execute(
                r"
                INSERT INTO symbols (
                    module_id, original_name, semantic_name, semantic_name_source,
                    export_name, scope_level
                )
                SELECT ?1, ?2, NULL, NULL, NULL, 'module'
                WHERE NOT EXISTS (
                    SELECT 1
                    FROM symbols
                    WHERE module_id = ?1
                      AND original_name = ?2
                      AND scope_level = 'module'
                )
                ",
                params![i64::from(spec.module_id), spec.original_name.as_str()],
            )
            .map_err(SymbolNamesError::WriteSymbolName)?;
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
            SymbolNameOperation::Accept(spec) => {
                let state = states
                    .entry(spec.module_id)
                    .or_insert_with(|| SymbolNameState {
                        original_names: BTreeSet::new(),
                        semantic_names: BTreeMap::new(),
                    });
                state.original_names.insert(spec.original_name.clone());
                state
                    .semantic_names
                    .insert(spec.original_name.clone(), Some(spec.semantic_name.clone()));
            }
            SymbolNameOperation::ClearActive(spec) => {
                if let Some(state) = states.get_mut(&spec.module_id) {
                    state
                        .semantic_names
                        .insert(spec.original_name.clone(), None);
                }
            }
            SymbolNameOperation::Propose(_) => {}
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

pub(crate) fn ensure_semantic_name_source_column(
    connection: &Connection,
) -> Result<(), SymbolNamesError> {
    if sqlite_table_has_column(connection, "symbols", "semantic_name_source")
        .map_err(SymbolNamesError::QuerySymbolNames)?
    {
        return Ok(());
    }
    connection
        .execute_batch("ALTER TABLE symbols ADD COLUMN semantic_name_source TEXT;")
        .map_err(SymbolNamesError::WriteSymbolName)
}

pub(crate) fn ensure_symbol_name_proposals_table(
    connection: &Connection,
) -> Result<(), SymbolNamesError> {
    connection
        .execute_batch(
            r"
            CREATE TABLE IF NOT EXISTS symbol_name_proposals (
                project_id INTEGER NOT NULL,
                module_id INTEGER NOT NULL,
                original_name TEXT NOT NULL,
                semantic_name TEXT NOT NULL,
                origin TEXT NOT NULL,
                accepted INTEGER NOT NULL DEFAULT 0,
                evidence TEXT,
                gate_status TEXT NOT NULL DEFAULT 'legacy',
                gate_reason TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (project_id, module_id, original_name, origin, semantic_name)
            );
            CREATE INDEX IF NOT EXISTS idx_symbol_name_proposals_symbol
                ON symbol_name_proposals(project_id, module_id, original_name);
            ",
        )
        .map_err(SymbolNamesError::WriteSymbolName)?;
    ensure_symbol_name_proposals_gate_columns(connection)
}

fn ensure_symbol_name_proposals_gate_columns(
    connection: &Connection,
) -> Result<(), SymbolNamesError> {
    if !sqlite_table_has_column(connection, "symbol_name_proposals", "gate_status")
        .map_err(SymbolNamesError::QuerySymbolNames)?
    {
        connection
            .execute_batch(
                "ALTER TABLE symbol_name_proposals ADD COLUMN gate_status TEXT NOT NULL DEFAULT 'legacy';",
            )
            .map_err(SymbolNamesError::WriteSymbolName)?;
    }
    if !sqlite_table_has_column(connection, "symbol_name_proposals", "gate_reason")
        .map_err(SymbolNamesError::QuerySymbolNames)?
    {
        connection
            .execute_batch("ALTER TABLE symbol_name_proposals ADD COLUMN gate_reason TEXT;")
            .map_err(SymbolNamesError::WriteSymbolName)?;
    }
    Ok(())
}

fn apply_operation(
    connection: &Connection,
    project_id: u32,
    origin: &str,
    evidence: Option<&str>,
    operation: &SymbolNameOperation,
) -> Result<usize, SymbolNamesError> {
    match operation {
        SymbolNameOperation::Propose(spec) => {
            upsert_proposal(
                connection,
                project_id,
                spec,
                origin,
                spec.evidence.as_deref().or(evidence),
                false,
            )?;
            Ok(1)
        }
        SymbolNameOperation::Accept(spec) => {
            upsert_proposal(
                connection,
                project_id,
                spec,
                origin,
                spec.evidence.as_deref().or(evidence),
                true,
            )?;
            deactivate_other_proposals(connection, project_id, spec, origin)?;
            connection
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
                        origin,
                    ],
                )
                .map_err(SymbolNamesError::WriteSymbolName)
        }
        SymbolNameOperation::ClearActive(spec) => {
            let updated = connection
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
                .map_err(SymbolNamesError::WriteSymbolName)?;
            connection
                .execute(
                    r"
                    UPDATE symbol_name_proposals
                       SET accepted = 0
                     WHERE project_id = ?1
                       AND module_id = ?2
                       AND original_name = ?3
                    ",
                    params![
                        i64::from(project_id),
                        i64::from(spec.module_id),
                        spec.original_name.as_str(),
                    ],
                )
                .map_err(SymbolNamesError::WriteSymbolName)?;
            Ok(updated)
        }
    }
}

fn upsert_proposal(
    connection: &Connection,
    project_id: u32,
    spec: &SymbolNameSpec,
    origin: &str,
    evidence: Option<&str>,
    accepted: bool,
) -> Result<(), SymbolNamesError> {
    connection
        .execute(
            r"
            INSERT INTO symbol_name_proposals (
                project_id, module_id, original_name, semantic_name,
                origin, accepted, evidence, gate_status, gate_reason
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'passed', 'deterministic-gates-passed')
            ON CONFLICT(project_id, module_id, original_name, origin, semantic_name)
            DO UPDATE SET
                accepted = excluded.accepted,
                evidence = COALESCE(excluded.evidence, symbol_name_proposals.evidence),
                gate_status = excluded.gate_status,
                gate_reason = excluded.gate_reason
            ",
            params![
                i64::from(project_id),
                i64::from(spec.module_id),
                spec.original_name.as_str(),
                spec.semantic_name.as_str(),
                origin,
                i64::from(accepted),
                evidence,
            ],
        )
        .map_err(SymbolNamesError::WriteSymbolName)?;
    Ok(())
}

fn deactivate_other_proposals(
    connection: &Connection,
    project_id: u32,
    spec: &SymbolNameSpec,
    origin: &str,
) -> Result<(), SymbolNamesError> {
    connection
        .execute(
            r"
            UPDATE symbol_name_proposals
               SET accepted = 0
             WHERE project_id = ?1
               AND module_id = ?2
               AND original_name = ?3
               AND NOT (origin = ?4 AND semantic_name = ?5)
            ",
            params![
                i64::from(project_id),
                i64::from(spec.module_id),
                spec.original_name.as_str(),
                origin,
                spec.semantic_name.as_str(),
            ],
        )
        .map_err(SymbolNamesError::WriteSymbolName)?;
    Ok(())
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

fn load_symbol_name_proposals(
    connection: &Connection,
    project_id: u32,
) -> Result<Vec<SymbolNameProposalRow>, SymbolNamesError> {
    if !sqlite_table_exists(connection, "symbol_name_proposals")
        .map_err(SymbolNamesError::QuerySymbolNames)?
    {
        return Ok(Vec::new());
    }
    let gate_status_expr =
        if sqlite_table_has_column(connection, "symbol_name_proposals", "gate_status")
            .map_err(SymbolNamesError::QuerySymbolNames)?
        {
            "NULLIF(TRIM(gate_status), '') AS gate_status"
        } else {
            "NULL AS gate_status"
        };
    let gate_reason_expr =
        if sqlite_table_has_column(connection, "symbol_name_proposals", "gate_reason")
            .map_err(SymbolNamesError::QuerySymbolNames)?
        {
            "NULLIF(TRIM(gate_reason), '') AS gate_reason"
        } else {
            "NULL AS gate_reason"
        };
    let mut statement = connection
        .prepare(&format!(
            r"
            SELECT module_id, original_name, semantic_name, origin, accepted, evidence,
                   {gate_status_expr}, {gate_reason_expr}
            FROM symbol_name_proposals
            WHERE project_id = ?1
            ORDER BY module_id, original_name, accepted DESC, origin, semantic_name
            ",
        ))
        .map_err(SymbolNamesError::QuerySymbolNames)?;
    let rows = statement
        .query_map(params![i64::from(project_id)], |row| {
            let module_id = row.get::<_, i64>(0)?;
            Ok(SymbolNameProposalRow {
                module_id: u32::try_from(module_id).map_err(|source| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Integer,
                        Box::new(source),
                    )
                })?,
                original_name: row.get(1)?,
                semantic_name: row.get(2)?,
                origin: row.get(3)?,
                accepted: row.get::<_, i64>(4)? != 0,
                evidence: row.get(5)?,
                gate_status: row.get(6)?,
                gate_reason: row.get(7)?,
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

fn print_symbol_name_proposals(rows: &[SymbolNameProposalRow]) {
    println!(
        "module_id\toriginal_name\tsemantic_name\torigin\taccepted\tevidence\tgate_status\tgate_reason"
    );
    for row in rows {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            row.module_id,
            row.original_name,
            row.semantic_name,
            row.origin,
            u8::from(row.accepted),
            row.evidence.as_deref().unwrap_or(""),
            row.gate_status.as_deref().unwrap_or(""),
            row.gate_reason.as_deref().unwrap_or("")
        );
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, params};

    use super::{
        SYMBOL_NAME_ORIGIN_AGENT, SymbolNameClearSpec, SymbolNameOperation, SymbolNameSpec,
        SymbolNamesArgs, SymbolNamesError, SymbolNamesOutcome, parse_batch_operations,
        symbol_names_from_connection,
    };

    fn args_with_accept(apply: bool) -> SymbolNamesArgs {
        SymbolNamesArgs {
            input: "fixture.db".into(),
            project_id: 1,
            list: false,
            apply,
            origin: SYMBOL_NAME_ORIGIN_AGENT.to_string(),
            evidence: Some("calls:create client".to_string()),
            proposals: Vec::new(),
            accepts: vec![SymbolNameSpec {
                module_id: 10,
                original_name: "$F1".to_string(),
                semantic_name: "createClient".to_string(),
                evidence: None,
            }],
            clear_active: Vec::new(),
            batch: None,
            all_proposals: false,
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
    fn batch_parser_accepts_propose_accept_clear_active() {
        let operations = parse_batch_operations(
            "action\tmodule_id\toriginal_name\tsemantic_name\tevidence\npropose\t10\t$F1\tmaybeClient\tcandidate:maybe client\naccept\t10\ta\tsettings\tconfig:settings\nclear-active\t10\toldName\n",
        )
        .expect("batch should parse");

        assert_eq!(operations.len(), 3);
        let SymbolNameOperation::Propose(spec) = &operations[0] else {
            panic!("first op should be propose");
        };
        assert_eq!(spec.evidence.as_deref(), Some("candidate:maybe client"));
    }

    #[test]
    fn dry_run_does_not_write_symbol_name() {
        let mut connection = create_fixture();
        let outcome = symbol_names_from_connection(&mut connection, &args_with_accept(false))
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
    fn accept_sets_active_semantic_name_and_records_proposal() {
        let mut connection = create_fixture();
        let outcome = symbol_names_from_connection(&mut connection, &args_with_accept(true))
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
        assert_eq!(source, "agent");
        let accepted: i64 = connection
            .query_row(
                "SELECT accepted FROM symbol_name_proposals WHERE module_id = 10 AND original_name = '$F1' AND semantic_name = 'createClient'",
                [],
                |row| row.get(0),
            )
            .expect("query proposal");
        assert_eq!(accepted, 1);
    }

    #[test]
    fn accept_rejects_generated_placeholder_semantic_name() {
        let mut connection = create_fixture();
        let mut args = args_with_accept(true);
        args.accepts[0].semantic_name = "semanticValue25".to_string();

        let error = symbol_names_from_connection(&mut connection, &args)
            .expect_err("placeholder names should be rejected");

        assert!(matches!(
            error,
            SymbolNamesError::PlaceholderSemanticName { .. }
        ));
    }

    #[test]
    fn accept_can_create_missing_module_symbol_from_generated_plan() {
        let mut connection = create_fixture();
        let mut args = args_with_accept(true);
        args.accepts = vec![SymbolNameSpec {
            module_id: 10,
            original_name: "_a".to_string(),
            semantic_name: "workerMessage".to_string(),
            evidence: None,
        }];

        let outcome = symbol_names_from_connection(&mut connection, &args)
            .expect("generated symbol-index target should be accepted");

        assert_eq!(outcome.written_changes, 1);
        let (name, source, scope): (String, String, String) = connection
            .query_row(
                "SELECT semantic_name, semantic_name_source, scope_level FROM symbols WHERE module_id = 10 AND original_name = '_a'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("query synthesized symbol");
        assert_eq!(name, "workerMessage");
        assert_eq!(source, "agent");
        assert_eq!(scope, "module");
    }

    #[test]
    fn propose_records_name_without_changing_active_semantic_name() {
        let mut connection = create_fixture();
        let mut args = args_with_accept(true);
        args.accepts.clear();
        args.evidence = Some("candidate:maybe client".to_string());
        args.proposals.push(SymbolNameSpec {
            module_id: 10,
            original_name: "$F1".to_string(),
            semantic_name: "maybeClient".to_string(),
            evidence: None,
        });

        let outcome = symbol_names_from_connection(&mut connection, &args).expect("propose writes");
        assert_eq!(outcome.written_changes, 1);
        let stored: Option<String> = connection
            .query_row(
                "SELECT semantic_name FROM symbols WHERE module_id = 10 AND original_name = '$F1'",
                [],
                |row| row.get(0),
            )
            .expect("query symbol");
        assert_eq!(stored, None);
        let accepted: i64 = connection
            .query_row(
                "SELECT accepted FROM symbol_name_proposals WHERE module_id = 10 AND original_name = '$F1' AND semantic_name = 'maybeClient'",
                [],
                |row| row.get(0),
            )
            .expect("query proposal");
        assert_eq!(accepted, 0);
    }

    #[test]
    fn clear_active_removes_semantic_name_and_deactivates_proposals() {
        let mut connection = create_fixture();
        let mut args = args_with_accept(true);
        symbol_names_from_connection(&mut connection, &args).expect("accept should write");
        args.accepts.clear();
        args.clear_active.push(SymbolNameClearSpec {
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
        let accepted: i64 = connection
            .query_row(
                "SELECT accepted FROM symbol_name_proposals WHERE module_id = 10 AND original_name = '$F1' AND semantic_name = 'createClient'",
                [],
                |row| row.get(0),
            )
            .expect("query proposal");
        assert_eq!(accepted, 0);
    }

    #[test]
    fn rejects_name_collision_with_existing_original() {
        let mut connection = create_fixture();
        let mut args = args_with_accept(false);
        args.evidence = Some("existing:other_name".to_string());
        args.accepts[0].semantic_name = "otherName".to_string();

        let error = symbol_names_from_connection(&mut connection, &args)
            .expect_err("collision should be rejected");
        assert!(matches!(error, SymbolNamesError::NameCollision { .. }));
    }

    #[test]
    fn automated_public_surface_accept_requires_structural_evidence() {
        let mut connection = create_fixture();
        connection
            .execute(
                "INSERT INTO symbols (module_id, semantic_name, export_name, original_name, scope_level) VALUES (?1, NULL, 'x', 'x', 'module')",
                params![10_i64],
            )
            .expect("insert public symbol");
        let mut args = args_with_accept(false);
        args.evidence = Some("handler".to_string());
        args.accepts = vec![SymbolNameSpec {
            module_id: 10,
            original_name: "x".to_string(),
            semantic_name: "handler".to_string(),
            evidence: None,
        }];

        let error = symbol_names_from_connection(&mut connection, &args)
            .expect_err("public surface needs structural evidence");
        assert!(matches!(error, SymbolNamesError::NamingGate { .. }));

        args.evidence = Some("export:handler".to_string());
        symbol_names_from_connection(&mut connection, &args)
            .expect("export evidence should satisfy public surface policy");
    }

    #[test]
    fn list_reads_without_source_column() {
        let mut connection = create_fixture();
        let args = SymbolNamesArgs {
            input: "fixture.db".into(),
            project_id: 1,
            list: true,
            apply: false,
            origin: SYMBOL_NAME_ORIGIN_AGENT.to_string(),
            evidence: None,
            proposals: Vec::new(),
            accepts: Vec::new(),
            clear_active: Vec::new(),
            batch: None,
            all_proposals: false,
        };

        let SymbolNamesOutcome { listed, .. } =
            symbol_names_from_connection(&mut connection, &args).expect("list should work");
        assert_eq!(listed.len(), 3);
        assert!(listed.iter().all(|row| row.semantic_name_source.is_none()));
    }

    #[test]
    fn list_all_proposals_reads_recorded_origins() {
        let mut connection = create_fixture();
        symbol_names_from_connection(&mut connection, &args_with_accept(true))
            .expect("accept should write");
        let args = SymbolNamesArgs {
            input: "fixture.db".into(),
            project_id: 1,
            list: true,
            apply: false,
            origin: SYMBOL_NAME_ORIGIN_AGENT.to_string(),
            evidence: None,
            proposals: Vec::new(),
            accepts: Vec::new(),
            clear_active: Vec::new(),
            batch: None,
            all_proposals: true,
        };

        let outcome = symbol_names_from_connection(&mut connection, &args).expect("list proposals");
        assert_eq!(outcome.listed_proposals.len(), 1);
        assert!(outcome.listed_proposals[0].accepted);
        assert_eq!(outcome.listed_proposals[0].origin, "agent");
    }

    #[test]
    fn accept_touches_all_duplicate_symbol_rows() {
        let mut connection = create_fixture();
        connection
            .execute(
                "INSERT INTO symbols (module_id, semantic_name, export_name, original_name, scope_level) VALUES (?1, NULL, NULL, ?2, 'module')",
                params![10_i64, "$F1"],
            )
            .expect("insert duplicate");

        let outcome = symbol_names_from_connection(&mut connection, &args_with_accept(true))
            .expect("apply should write duplicate rows");
        assert_eq!(outcome.written_changes, 2);
    }
}
