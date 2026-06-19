//! `generate-project-v2` subcommand: load a project bundle from SQLite,
//! run the output pipeline, audit-gate the result, then materialise the
//! TypeScript project (sources, scaffold, assets) under `--output`.

use std::path::PathBuf;

use clap::Args;
use reverts_pipeline::generate_project_from_input;

use crate::args::{parse_args_with_name, parse_project_id};
use crate::errors::{CliError, CliRunError};
use crate::format_audit_findings;
use crate::input_externalization::{
    load_materialized_package_manifests, load_project_bundle_with_package_externalization,
};
use crate::runtime_dependency_coherence::prune_transitively_provided_scope_incoherent_dependencies;

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
    let run = generate_project_from_input(input).map_err(CliRunError::Pipeline)?;

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
    let written = write_accepted_project(
        accepted_project,
        &run.assets,
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
    println!(
        "generated project {} into {} with {written} files ({} symbol-index entries)",
        args.project_id,
        args.output.display(),
        run.symbol_index.len()
    );
    Ok(())
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
