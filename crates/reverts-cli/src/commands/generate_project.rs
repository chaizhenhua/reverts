//! `generate-project-v2` subcommand: load a project bundle from SQLite,
//! run the output pipeline, audit-gate the result, then materialise the
//! TypeScript project (sources, scaffold, assets) under `--output`.

use std::path::{Path, PathBuf};

use clap::Args;
use reverts_js::{
    CompilerLowering, GeneratedRename, ParseGoal, format_source_with_module_items_and_renames,
};
use reverts_pipeline::{
    EmittedAsset, GenerateProjectOptions, LocalBindingRename,
    generate_project_from_input_with_options,
};
use rusqlite::{Connection, OpenFlags, params};

use crate::args::{parse_args_with_name, parse_project_id};
use crate::errors::{CliError, CliRunError};
use crate::format_audit_findings;
use crate::input_externalization::{
    load_materialized_package_manifests, load_project_bundle_with_package_externalization,
};
use crate::runtime_dependency_coherence::prune_transitively_provided_scope_incoherent_dependencies;
use crate::{collect_sqlite_rows, sqlite_table_exists};

#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct GenerateProjectV2Args {
    #[arg(long)]
    pub input: PathBuf,
    #[arg(long)]
    pub output: PathBuf,
    #[arg(long, value_parser = parse_project_id)]
    pub project_id: u32,
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
    let input = load_project_bundle_with_package_externalization(&args.input, args.project_id)
        .map_err(CliRunError::LoadInput)?;
    let local_binding_renames = load_local_binding_renames(&args.input, args.project_id)?;
    let run = generate_project_from_input_with_options(
        input,
        GenerateProjectOptions {
            local_binding_renames: local_binding_renames.clone(),
        },
    )
    .map_err(CliRunError::Pipeline)?;

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
        .as_ref()
        .ok_or_else(|| CliRunError::AuditRejected(format_audit_findings(&run.audit)))?;
    // Drop scope-incoherent root pins (e.g. a mis-matched off-major `@smithy/*`
    // sibling) that npm would otherwise install transitively at a coherent
    // version; root-pinning conflicting majors of one scope blows up `npm
    // install`. Needs the cached `package.json` dependency graph, which only
    // the CLI (not the generation pipeline) can see.
    let manifests =
        load_materialized_package_manifests(&args.input).map_err(CliRunError::LoadInput)?;
    let runtime_dependencies = prune_transitively_provided_scope_incoherent_dependencies(
        run.runtime_dependencies.clone(),
        &manifests,
    );
    let mut assets = apply_local_binding_renames_to_code_assets(
        run.assets.as_slice(),
        local_binding_renames.as_slice(),
    )?;
    assets.extend(run.source_mirror_assets.clone());
    let written = write_accepted_project(
        accepted_project,
        assets.as_slice(),
        &args.output,
        &runtime_dependencies,
    )?;
    let symbol_index_path = args.output.join("symbol-index.json");
    std::fs::write(
        &symbol_index_path,
        serialize_symbol_index(&run.symbol_index),
    )
    .map_err(|source| CliRunError::WriteOutput {
        path: symbol_index_path.clone(),
        source,
    })?;
    let binding_name_index_path = args.output.join("binding-name-index.json");
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
    Ok(())
}

fn apply_local_binding_renames_to_code_assets(
    assets: &[EmittedAsset],
    renames: &[LocalBindingRename],
) -> Result<Vec<EmittedAsset>, CliRunError> {
    if renames.is_empty() {
        return Ok(assets.to_vec());
    }
    let mut renames_by_path = std::collections::BTreeMap::<&str, Vec<GeneratedRename>>::new();
    for rename in renames {
        renames_by_path
            .entry(rename.file_path.as_str())
            .or_default()
            .push(if let Some(binding_index) = rename.binding_index {
                GeneratedRename::new_binding_index(
                    rename.original_name.as_str(),
                    rename.semantic_name.as_str(),
                    binding_index,
                )
            } else {
                GeneratedRename::new_all_scopes(
                    rename.original_name.as_str(),
                    rename.semantic_name.as_str(),
                )
            });
    }
    let mut transformed = Vec::with_capacity(assets.len());
    for asset in assets {
        let Some(asset_renames) = renames_by_path.get(asset.path.as_str()) else {
            transformed.push(asset.clone());
            continue;
        };
        if !is_code_output_path(asset.path.as_str()) {
            transformed.push(asset.clone());
            continue;
        }
        let source = std::str::from_utf8(asset.bytes.as_slice()).map_err(|error| {
            CliRunError::GenerateProject(format!(
                "failed to read code asset {} as UTF-8 for binding renames: {error}",
                asset.path
            ))
        })?;
        let formatted = format_source_with_module_items_and_renames(
            source,
            &[],
            &[],
            asset_renames,
            Some(std::path::Path::new(asset.path.as_str())),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .map_err(|error| {
            CliRunError::GenerateProject(format!(
                "failed to apply binding renames to asset {}: {error}",
                asset.path
            ))
        })?;
        let mut asset = asset.clone();
        asset.bytes = formatted.into_bytes();
        transformed.push(asset);
    }
    Ok(transformed)
}

fn is_code_output_path(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| {
            matches!(
                extension,
                "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts"
            )
        })
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
    let mut statement = connection
        .prepare(if has_binding_key {
            r"
            SELECT file_path, original_name, binding_index, semantic_name
            FROM semantic_binding_names
            WHERE project_id = ?1
              AND accepted = 1
              AND TRIM(file_path) != ''
              AND TRIM(original_name) != ''
              AND TRIM(semantic_name) != ''
            ORDER BY file_path, original_name, binding_key
            "
        } else {
            r"
            SELECT file_path, original_name, NULL AS binding_index, semantic_name
            FROM semantic_binding_names
            WHERE project_id = ?1
              AND accepted = 1
              AND TRIM(file_path) != ''
              AND TRIM(original_name) != ''
              AND TRIM(semantic_name) != ''
            ORDER BY file_path, original_name
            "
        })
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
                "module_id": entry.module_id.0,
                "original_name": entry.original_name,
                "emitted_name": entry.emitted_name,
                "semantic_named": entry.semantic_named,
                "file_path": entry.file_path,
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::Value::Array(rows))
        .expect("serializing a JSON array of plain values is infallible")
}

pub(crate) use crate::project_writer::write_accepted_project;

#[cfg(test)]
pub(crate) use crate::project_writer::{checked_output_path, write_emitted_project};
