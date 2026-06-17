//! `generate-project-v2` subcommand: load a project bundle from SQLite,
//! run the output pipeline, audit-gate the result, then materialise the
//! TypeScript project (sources, scaffold, assets) under `--output`.

use std::path::PathBuf;

use clap::Args;
use reverts_pipeline::generate_project_from_input;

use crate::args::{parse_args_with_name, parse_project_id};
use crate::errors::{CliError, CliRunError};
use crate::format_audit_findings;
use crate::input_externalization::load_project_bundle_with_verified_externalization_hints;

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
    let input =
        load_project_bundle_with_verified_externalization_hints(&args.input, args.project_id)
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
    let written = write_accepted_project(
        accepted_project,
        &run.assets,
        &args.output,
        &run.runtime_dependencies,
    )?;
    println!(
        "generated project {} into {} with {written} files",
        args.project_id,
        args.output.display()
    );
    Ok(())
}

pub(crate) use crate::project_writer::write_accepted_project;

#[cfg(test)]
pub(crate) use crate::project_writer::{checked_output_path, write_emitted_project};
