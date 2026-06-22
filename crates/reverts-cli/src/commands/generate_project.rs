//! `generate-project-v2` subcommand: load a project bundle from SQLite,
//! run the output pipeline, audit-gate the result, then materialise the
//! TypeScript project (sources, scaffold, assets) under `--output`.

use std::path::{Path, PathBuf};

use clap::Args;
use reverts_pipeline::{
    FunctionParamRenameRow, GenerateProjectOptions, LocalBindingRename,
    generate_project_from_input_with_options,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::args::{parse_args_with_name, parse_project_id};
use crate::commands::naming_gates::{NamingGateMode, validate_name_acceptance};
use crate::errors::{CliError, CliRunError};
use crate::format_audit_findings;
use crate::input_externalization::{
    load_materialized_package_manifests, load_project_bundle_with_package_externalization,
};
use crate::runtime_dependency_coherence::prune_transitively_provided_scope_incoherent_dependencies;
use crate::{collect_sqlite_rows, sqlite_table_exists, sqlite_table_has_column};

#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct GenerateProjectV2Args {
    #[arg(long)]
    pub input: PathBuf,
    #[arg(long)]
    pub output: PathBuf,
    #[arg(long, value_parser = parse_project_id)]
    pub project_id: u32,
    /// Emit all generated source under this directory (e.g. `src`) for a modern
    /// TypeScript project layout: `moduleResolution: NodeNext`, a `package.json`
    /// `exports` map, `.gitignore`, and pipeline metadata
    /// (`symbol-index.json`, `binding-name-index.json`) relocated to `.reverts/`
    /// so the source tree stays clean. Omit it for the flat legacy layout.
    #[arg(long)]
    pub source_root: Option<String>,
}

impl GenerateProjectV2Args {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let mut args = args.into_iter().collect::<Vec<_>>();
        if args
            .first()
            .is_some_and(|argument| argument == crate::help::GENERATE_PROJECT_V2_COMMAND)
        {
            args.remove(0);
        }
        parse_args_with_name(crate::help::GENERATE_PROJECT_V2_COMMAND, args)
    }
}

pub(crate) fn run(args: GenerateProjectV2Args) -> Result<(), CliRunError> {
    let timing_enabled = std::env::var_os("REVERTS_GENERATE_TIMING").is_some();
    let started = std::time::Instant::now();
    let mut last = started;
    macro_rules! mark_timing {
        ($stage:literal) => {
            if timing_enabled {
                let now = std::time::Instant::now();
                eprintln!(
                    "generate-project cli timing: {} stage={:.3}s total={:.3}s",
                    $stage,
                    now.duration_since(last).as_secs_f64(),
                    now.duration_since(started).as_secs_f64()
                );
                last = now;
            }
        };
    }
    validate_accepted_naming_gate_records(&args.input, args.project_id)?;
    mark_timing!("validate_naming");
    let input = load_project_bundle_with_package_externalization(&args.input, args.project_id)
        .map_err(CliRunError::LoadInput)?;
    mark_timing!("load_input");
    let local_binding_renames = load_local_binding_renames(&args.input, args.project_id)?;
    mark_timing!("load_binding_renames");
    let function_param_renames = load_function_param_renames(&args.input, args.project_id)?;
    mark_timing!("load_param_renames");
    let package_anchored_island_bindings =
        load_package_anchored_island_bindings(&args.input, args.project_id)?;
    mark_timing!("load_island_anchors");
    let run = generate_project_from_input_with_options(
        input,
        GenerateProjectOptions {
            local_binding_renames: local_binding_renames.clone(),
            function_param_renames,
            package_anchored_island_bindings,
        },
    )
    .map_err(CliRunError::Pipeline)?;
    mark_timing!("pipeline");

    // Only errors block writing the output. Warnings (e.g. duplicate
    // top-level binding, ambiguous binding shape) describe input-bundle
    // conditions per ADR 0002: surface them, don't strand the user.
    if run.audit.has_errors() {
        return Err(CliRunError::AuditRejected(format_audit_findings(
            &run.audit,
        )));
    }
    if !run.audit.is_clean() {
        eprintln!(
            "warning: generated project carries {} audit warning(s):\n{}",
            run.audit.warning_count(),
            format_audit_findings(&run.audit)
        );
    }

    let accepted_project = run
        .accepted_project
        .ok_or_else(|| CliRunError::AuditRejected(format_audit_findings(&run.audit)))?;
    // Drop scope-incoherent root pins (e.g. a mis-matched off-major `@smithy/*`
    // sibling) that npm would otherwise install transitively at a coherent
    // version; root-pinning conflicting majors of one scope blows up `npm
    // install`. Needs the cached `package.json` dependency graph, which only
    // the CLI (not the generation pipeline) can see.
    let manifests =
        load_materialized_package_manifests(&args.input).map_err(CliRunError::LoadInput)?;
    mark_timing!("load_manifests");
    let runtime_dependencies = prune_transitively_provided_scope_incoherent_dependencies(
        run.runtime_dependencies.clone(),
        &manifests,
    );
    mark_timing!("prune_dependencies");
    let mut assets = run.assets.clone();
    assets.extend(run.source_mirror_assets.clone());
    let source_root = args.source_root.as_deref();
    let written = write_accepted_project(
        &accepted_project,
        assets.as_slice(),
        &args.output,
        &runtime_dependencies,
        source_root,
    )?;
    mark_timing!("write_project");
    // Pipeline metadata is not part of the published source tree: in the modern
    // (source-root) layout it lives in a `.reverts/` sidecar; the flat layout
    // keeps it at the output root for backward compatibility.
    let metadata_dir = match source_root {
        Some(_) => args.output.join(".reverts"),
        None => args.output.clone(),
    };
    std::fs::create_dir_all(&metadata_dir).map_err(|source| CliRunError::WriteOutput {
        path: metadata_dir.clone(),
        source,
    })?;
    let symbol_index_path = metadata_dir.join("symbol-index.json");
    std::fs::write(
        &symbol_index_path,
        serialize_symbol_index(&run.symbol_index),
    )
    .map_err(|source| CliRunError::WriteOutput {
        path: symbol_index_path.clone(),
        source,
    })?;
    let binding_name_index_path = metadata_dir.join("binding-name-index.json");
    std::fs::write(
        &binding_name_index_path,
        serialize_binding_name_index(&local_binding_renames),
    )
    .map_err(|source| CliRunError::WriteOutput {
        path: binding_name_index_path.clone(),
        source,
    })?;
    println!(
        "generated project {} into {} with {written} files ({} symbol-index entries)",
        args.project_id,
        args.output.display(),
        run.symbol_index.len()
    );
    if timing_enabled {
        let _ = last;
    }
    Ok(())
}

fn load_local_binding_renames(
    input: &Path,
    project_id: u32,
) -> Result<Vec<LocalBindingRename>, CliRunError> {
    let connection = Connection::open_with_flags(input, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    if !sqlite_table_exists(&connection, "semantic_binding_names")
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?
    {
        return Ok(Vec::new());
    }
    let has_binding_key =
        sqlite_column_exists(&connection, "semantic_binding_names", "binding_key")?;
    // A DB without the gate_status column predates the quality gate; grandfather its
    // accepted names (load them ungated) instead of refusing to generate. Gate-aware
    // DBs keep the strict `gate_status = 'passed'` filter.
    let gate_clause = if sqlite_column_exists(&connection, "semantic_binding_names", "gate_status")?
    {
        "AND gate_status = 'passed'"
    } else {
        ""
    };
    let query = if has_binding_key {
        format!(
            "SELECT file_path, original_name, binding_index, semantic_name
            FROM semantic_binding_names
            WHERE project_id = ?1
              AND accepted = 1
              {gate_clause}
              AND TRIM(file_path) != ''
              AND TRIM(original_name) != ''
              AND TRIM(semantic_name) != ''
            ORDER BY file_path, original_name, binding_key"
        )
    } else {
        format!(
            "SELECT file_path, original_name, NULL AS binding_index, semantic_name
            FROM semantic_binding_names
            WHERE project_id = ?1
              AND accepted = 1
              {gate_clause}
              AND TRIM(file_path) != ''
              AND TRIM(original_name) != ''
              AND TRIM(semantic_name) != ''
            ORDER BY file_path, original_name"
        )
    };
    let mut statement = connection
        .prepare(query.as_str())
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let rows = statement
        .query_map(params![i64::from(project_id)], |row| {
            Ok(LocalBindingRename {
                file_path: row.get(0)?,
                original_name: row.get(1)?,
                binding_index: row
                    .get::<_, Option<i64>>(2)?
                    .and_then(|value| u32::try_from(value).ok()),
                semantic_name: row.get(3)?,
            })
        })
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    collect_sqlite_rows(rows).map_err(|source| CliRunError::GenerateProject(source.to_string()))
}

/// Bundle-local names of eager entry-island bindings the matcher anchored to a
/// third-party library (`package_island_anchors`). These are library code, not
/// application symbols, so generation drops them from the naming denominator.
/// A pre-anchoring database (no table) simply contributes nothing.
fn load_package_anchored_island_bindings(
    input: &Path,
    project_id: u32,
) -> Result<std::collections::BTreeSet<String>, CliRunError> {
    let connection = Connection::open_with_flags(input, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    if !sqlite_table_exists(&connection, "package_island_anchors")
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?
    {
        return Ok(std::collections::BTreeSet::new());
    }
    let mut statement = connection
        .prepare(
            "SELECT DISTINCT binding_name FROM package_island_anchors \
             WHERE project_id = ?1 AND TRIM(binding_name) != ''",
        )
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let rows = statement
        .query_map(params![i64::from(project_id)], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let mut names = std::collections::BTreeSet::new();
    for row in rows {
        names.insert(row.map_err(|source| CliRunError::GenerateProject(source.to_string()))?);
    }
    Ok(names)
}

fn load_function_param_renames(
    input: &Path,
    project_id: u32,
) -> Result<Vec<FunctionParamRenameRow>, CliRunError> {
    let connection = Connection::open_with_flags(input, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    if !sqlite_table_exists(&connection, "semantic_function_param_names")
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?
    {
        return Ok(Vec::new());
    }
    let mut statement = connection
        .prepare(
            r"
            SELECT file_path, function_name, param_index, semantic_name
            FROM semantic_function_param_names
            WHERE project_id = ?1
              AND accepted = 1
              AND NULLIF(TRIM(semantic_name), '') IS NOT NULL
            ",
        )
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let rows = statement
        .query_map(params![i64::from(project_id)], |row| {
            Ok(FunctionParamRenameRow {
                file_path: row.get(0)?,
                function_name: row.get(1)?,
                param_index: u32::try_from(row.get::<_, i64>(2)?).unwrap_or(u32::MAX),
                semantic_name: row.get(3)?,
            })
        })
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    collect_sqlite_rows(rows).map_err(|source| CliRunError::GenerateProject(source.to_string()))
}

fn validate_accepted_naming_gate_records(input: &Path, project_id: u32) -> Result<(), CliRunError> {
    let connection = Connection::open_with_flags(input, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    validate_active_symbol_names_have_passed_gates(&connection, project_id)?;
    validate_accepted_binding_names_have_passed_gates(&connection, project_id)
}

fn validate_active_symbol_names_have_passed_gates(
    connection: &Connection,
    project_id: u32,
) -> Result<(), CliRunError> {
    if !sqlite_table_exists(connection, "symbols")
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?
    {
        return Ok(());
    }
    let has_source_column = sqlite_table_has_column(connection, "symbols", "semantic_name_source")
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let source_expr = if has_source_column {
        "NULLIF(TRIM(s.semantic_name_source), '') AS semantic_name_source"
    } else {
        "NULL AS semantic_name_source"
    };
    let sql = format!(
        r"
        SELECT s.module_id, s.original_name, NULLIF(TRIM(s.semantic_name), '') AS semantic_name,
               {source_expr}
        FROM symbols s
        JOIN modules m ON m.id = s.module_id
        JOIN project_files pf ON pf.file_id = m.file_id
        WHERE pf.project_id = ?1
          AND s.scope_level = 'module'
          AND NULLIF(TRIM(s.semantic_name), '') IS NOT NULL
        ORDER BY s.module_id, s.original_name
        "
    );
    let mut statement = connection
        .prepare(sql.as_str())
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let rows = statement
        .query_map(params![i64::from(project_id)], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let rows = collect_sqlite_rows(rows)
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    // A name with no `semantic_name_source` is a pre-gate / legacy name (applied
    // before the proposal-provenance system existed, e.g. a project imported and
    // agent-named under an older schema). It makes no provenance claim and no
    // matching proposal can exist (proposals join on `origin`), so HARD-blocking
    // generation on it would make every legacy project ungeneratable — a regression,
    // not a safety win. Grandfather those by enforcing only names that DO claim a
    // source: properly gate-applied names stay fully validated, and a name that
    // fakes a source without a matching proposal is still caught below.
    let provenanced: Vec<_> = rows
        .into_iter()
        .filter(|(_, _, _, origin)| origin.is_some())
        .collect();
    if provenanced.is_empty() {
        return Ok(());
    }
    if !sqlite_table_exists(connection, "symbol_name_proposals")
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?
        || !sqlite_table_has_column(connection, "symbol_name_proposals", "gate_status")
            .map_err(|source| CliRunError::GenerateProject(source.to_string()))?
    {
        // The proposal table predates the gate-status mechanism: this project was
        // named under an older schema where the quality gate did not exist. Its
        // active names cannot be validated against a gate that was never present
        // and were applied before it — grandfather the whole pre-gate project (same
        // rationale as the source-less legacy names above) rather than making every
        // older decompile DB ungeneratable. Gate-aware DBs (column present) are
        // still fully validated below.
        return Ok(());
    }
    for (module_id, original_name, semantic_name, origin) in provenanced {
        let origin = origin.expect("filtered to provenanced (origin = Some) rows above");
        let proposal_evidence = connection
            .query_row(
                r"
                SELECT evidence
                FROM symbol_name_proposals
                WHERE project_id = ?1
                  AND module_id = ?2
                  AND original_name = ?3
                  AND semantic_name = ?4
                  AND origin = ?5
                  AND accepted = 1
                  AND gate_status = 'passed'
                LIMIT 1
                ",
                params![
                    i64::from(project_id),
                    module_id,
                    original_name.as_str(),
                    semantic_name.as_str(),
                    origin.as_str(),
                ],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
        let Some(evidence) = proposal_evidence else {
            return Err(CliRunError::GenerateProject(format!(
                "active semantic name {module_id}:{original_name} -> {semantic_name} has no matching accepted gate-passed proposal"
            )));
        };
        validate_name_acceptance(
            original_name.as_str(),
            semantic_name.as_str(),
            origin.as_str(),
            evidence.as_deref(),
            NamingGateMode::Symbol,
        )
        .map_err(|error| CliRunError::GenerateProject(error.message()))?;
    }
    Ok(())
}

fn validate_accepted_binding_names_have_passed_gates(
    connection: &Connection,
    project_id: u32,
) -> Result<(), CliRunError> {
    if !sqlite_table_exists(connection, "semantic_binding_names")
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?
    {
        return Ok(());
    }
    if !sqlite_table_has_column(connection, "semantic_binding_names", "gate_status")
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?
    {
        // Pre-gate DB (no gate_status column): its accepted names were applied before
        // the quality gate existed — grandfather them rather than blocking generation.
        return Ok(());
    }
    let has_binding_key =
        sqlite_table_has_column(connection, "semantic_binding_names", "binding_key")
            .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let mut statement = connection
        .prepare(if has_binding_key {
            r"
            SELECT file_path, original_name, binding_index, semantic_name, origin, evidence, gate_status
            FROM semantic_binding_names
            WHERE project_id = ?1 AND accepted = 1
            ORDER BY file_path, original_name, binding_key
            "
        } else {
            r"
            SELECT file_path, original_name, NULL AS binding_index, semantic_name, origin, evidence, gate_status
            FROM semantic_binding_names
            WHERE project_id = ?1 AND accepted = 1
            ORDER BY file_path, original_name
            "
        })
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let rows = statement
        .query_map(params![i64::from(project_id)], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
            ))
        })
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    for row in collect_sqlite_rows(rows)
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?
    {
        let (file_path, original_name, _binding_index, semantic_name, origin, evidence, status) =
            row;
        if status != "passed" {
            return Err(CliRunError::GenerateProject(format!(
                "accepted binding name {file_path}:{original_name} -> {semantic_name} has gate_status={status}, expected passed"
            )));
        }
        validate_name_acceptance(
            original_name.as_str(),
            semantic_name.as_str(),
            origin.as_str(),
            evidence.as_deref(),
            NamingGateMode::LocalBinding,
        )
        .map_err(|error| CliRunError::GenerateProject(error.message()))?;
    }
    Ok(())
}

fn sqlite_column_exists(
    connection: &Connection,
    table: &str,
    column: &str,
) -> Result<bool, CliRunError> {
    let mut statement = connection
        .prepare(format!("PRAGMA table_info({table})").as_str())
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    let columns = collect_sqlite_rows(rows)
        .map_err(|source| CliRunError::GenerateProject(source.to_string()))?;
    Ok(columns.iter().any(|existing| existing == column))
}

fn serialize_binding_name_index(entries: &[LocalBindingRename]) -> String {
    let rows = entries
        .iter()
        .map(|entry| {
            serde_json::json!({
                "file_path": entry.file_path,
                "original_name": entry.original_name,
                "binding_index": entry.binding_index,
                "emitted_name": entry.semantic_name,
                "semantic_named": true,
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string_pretty(&serde_json::Value::Array(rows))
        .expect("serializing a JSON array of plain values is infallible")
}

/// Serializes the symbol index as a JSON array. `reverts_pipeline::SymbolIndexEntry`
/// has no serde derive (the pipeline crate stays serde-free), so the CLI renders
/// the rows it needs.
fn serialize_symbol_index(entries: &[reverts_pipeline::SymbolIndexEntry]) -> String {
    let rows: Vec<serde_json::Value> = entries
        .iter()
        .map(|entry| {
            serde_json::json!({
                // `null` for bindings in unmodularized recovered-code files
                // (e.g. the eager entrypoint island) — named via the
                // file-path-keyed binding-name channel, not the symbols table.
                "module_id": entry.module_id.map(|module_id| module_id.0),
                "original_name": entry.original_name,
                "emitted_name": entry.emitted_name,
                "semantic_named": entry.semantic_named,
                "file_path": entry.file_path,
                "function_like": entry.function_like,
                "dead": entry.dead,
                "exported": entry.exported,
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::Value::Array(rows))
        .expect("serializing a JSON array of plain values is infallible")
}

pub(crate) use crate::project_writer::write_accepted_project;

#[cfg(test)]
pub(crate) use crate::project_writer::{
    checked_output_path, write_emitted_project, write_emitted_project_with_source_root,
};

#[cfg(test)]
mod tests {
    use super::validate_accepted_naming_gate_records;

    use rusqlite::{Connection, params};
    use tempfile::TempDir;

    fn gate_db() -> (TempDir, std::path::PathBuf) {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("input.sqlite");
        let connection = Connection::open(path.as_path()).expect("open sqlite");
        connection
            .execute_batch(
                r"
                CREATE TABLE project_files (
                    project_id INTEGER NOT NULL,
                    file_id INTEGER NOT NULL
                );
                CREATE TABLE modules (
                    id INTEGER PRIMARY KEY,
                    file_id INTEGER NOT NULL
                );
                CREATE TABLE symbols (
                    module_id INTEGER NOT NULL,
                    original_name TEXT NOT NULL,
                    semantic_name TEXT,
                    semantic_name_source TEXT,
                    export_name TEXT,
                    scope_level TEXT
                );
                CREATE TABLE symbol_name_proposals (
                    project_id INTEGER NOT NULL,
                    module_id INTEGER NOT NULL,
                    original_name TEXT NOT NULL,
                    semantic_name TEXT NOT NULL,
                    origin TEXT NOT NULL,
                    accepted INTEGER NOT NULL,
                    evidence TEXT,
                    gate_status TEXT NOT NULL
                );
                CREATE TABLE semantic_binding_names (
                    project_id INTEGER NOT NULL,
                    file_path TEXT NOT NULL,
                    original_name TEXT NOT NULL,
                    binding_index INTEGER,
                    binding_key TEXT NOT NULL,
                    semantic_name TEXT NOT NULL,
                    origin TEXT NOT NULL,
                    evidence TEXT,
                    accepted INTEGER NOT NULL,
                    gate_status TEXT NOT NULL
                );
                INSERT INTO project_files (project_id, file_id) VALUES (1, 100);
                INSERT INTO modules (id, file_id) VALUES (10, 100);
                ",
            )
            .expect("create gate schema");
        drop(connection);
        (temp, path)
    }

    #[test]
    fn generate_project_rejects_active_symbol_name_without_gate_passed_proposal() {
        let (_temp, path) = gate_db();
        let connection = Connection::open(path.as_path()).expect("open sqlite");
        connection
            .execute(
                r"
                INSERT INTO symbols (
                    module_id, original_name, semantic_name, semantic_name_source,
                    export_name, scope_level
                ) VALUES (10, '$F1', 'createClient', 'agent', NULL, 'module')
                ",
                [],
            )
            .expect("insert symbol");
        drop(connection);

        let error = validate_accepted_naming_gate_records(path.as_path(), 1)
            .expect_err("ungated active symbol names must be rejected");
        assert!(
            error
                .to_string()
                .contains("no matching accepted gate-passed proposal")
        );
    }

    #[test]
    fn generate_project_grandfathers_legacy_symbol_name_without_source() {
        // A legacy project: an active module symbol name with NO
        // `semantic_name_source` (applied before the proposal-provenance system).
        // There is no proposal table for it and none can match. The gate must
        // grandfather it (allow generation) rather than hard-block, otherwise every
        // pre-gate project becomes ungeneratable.
        let (_temp, path) = gate_db();
        let connection = Connection::open(path.as_path()).expect("open sqlite");
        // Drop the proposals table to mimic a genuinely legacy DB.
        connection
            .execute("DROP TABLE symbol_name_proposals", [])
            .expect("drop proposals");
        connection
            .execute(
                r"
                INSERT INTO symbols (
                    module_id, original_name, semantic_name, semantic_name_source,
                    export_name, scope_level
                ) VALUES (10, '$F1', 'createClient', NULL, NULL, 'module')
                ",
                [],
            )
            .expect("insert symbol");
        drop(connection);

        validate_accepted_naming_gate_records(path.as_path(), 1)
            .expect("legacy un-provenanced names must be grandfathered, not blocked");
    }

    #[test]
    fn generate_project_revalidates_gate_passed_symbol_proposal_content() {
        let (_temp, path) = gate_db();
        let connection = Connection::open(path.as_path()).expect("open sqlite");
        connection
            .execute(
                r"
                INSERT INTO symbols (
                    module_id, original_name, semantic_name, semantic_name_source,
                    export_name, scope_level
                ) VALUES (10, '$F1', 'billingInvoiceHandler', 'agent', NULL, 'module')
                ",
                [],
            )
            .expect("insert symbol");
        connection
            .execute(
                r"
                INSERT INTO symbol_name_proposals (
                    project_id, module_id, original_name, semantic_name, origin,
                    accepted, evidence, gate_status
                ) VALUES (1, 10, '$F1', 'billingInvoiceHandler', 'agent',
                          1, 'route:/api/session handler', 'passed')
                ",
                [],
            )
            .expect("insert proposal");
        drop(connection);

        let error = validate_accepted_naming_gate_records(path.as_path(), 1)
            .expect_err("passed status is not enough if deterministic gates fail");
        assert!(error.to_string().contains("absent from evidence"));
    }

    #[test]
    fn generate_project_rejects_accepted_binding_name_without_passed_gate() {
        let (_temp, path) = gate_db();
        let connection = Connection::open(path.as_path()).expect("open sqlite");
        connection
            .execute(
                r"
                INSERT INTO semantic_binding_names (
                    project_id, file_path, original_name, binding_index, binding_key,
                    semantic_name, origin, evidence, accepted, gate_status
                ) VALUES (1, 'src/index.ts', 'a', NULL, '', 'refreshAccessToken',
                          'agent', 'string:refresh_token string:access_token', 1, 'legacy')
                ",
                [],
            )
            .expect("insert binding name");
        drop(connection);

        let error = validate_accepted_naming_gate_records(path.as_path(), 1)
            .expect_err("accepted binding names require passed gates");
        assert!(error.to_string().contains("expected passed"));
    }

    #[test]
    fn generate_project_accepts_binding_name_with_passed_revalidated_gate() {
        let (_temp, path) = gate_db();
        let connection = Connection::open(path.as_path()).expect("open sqlite");
        connection
            .execute(
                r"
                INSERT INTO semantic_binding_names (
                    project_id, file_path, original_name, binding_index, binding_key,
                    semantic_name, origin, evidence, accepted, gate_status
                ) VALUES (?1, 'src/index.ts', 'a', NULL, '', 'refreshAccessToken',
                          'agent', 'string:refresh_token string:access_token', 1, 'passed')
                ",
                params![1_i64],
            )
            .expect("insert binding name");
        drop(connection);

        validate_accepted_naming_gate_records(path.as_path(), 1)
            .expect("gate-passed accepted binding name should pass preflight");
    }
}
