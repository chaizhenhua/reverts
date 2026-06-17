//! `generate-project-v2` subcommand: load a project bundle from SQLite,
//! run the output pipeline, audit-gate the result, then materialise the
//! TypeScript project (sources, scaffold, assets) under `--output`.

use std::path::PathBuf;

use reverts_input::sqlite::load_project_bundle_from_sqlite;
use reverts_pipeline::generate_project_from_input;

use crate::errors::{CliError, CliRunError};
use crate::{format_audit_findings, next_path, parse_project_id};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerateProjectV2Args {
    pub input: PathBuf,
    pub output: PathBuf,
    pub project_id: u32,
}

impl GenerateProjectV2Args {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let mut input = None;
        let mut output = None;
        let mut project_id = None;
        let mut args = args.into_iter().collect::<Vec<_>>();
        if args
            .first()
            .is_some_and(|argument| argument == crate::help::GENERATE_PROJECT_V2_COMMAND)
        {
            args.remove(0);
        }
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--input" => input = Some(next_path(&mut args, "--input")?),
                "--output" => output = Some(next_path(&mut args, "--output")?),
                "--project-id" => {
                    project_id = Some(parse_project_id(crate::next_value(
                        &mut args,
                        "--project-id",
                    )?)?);
                }
                other => return Err(CliError::UnknownArgument(other.to_string())),
            }
        }

        Ok(Self {
            input: input.ok_or(CliError::MissingArgument("--input"))?,
            output: output.ok_or(CliError::MissingArgument("--output"))?,
            project_id: project_id.ok_or(CliError::MissingArgument("--project-id"))?,
        })
    }
}

pub(crate) fn run(args: GenerateProjectV2Args) -> Result<(), CliRunError> {
    let input = load_project_bundle_from_sqlite(&args.input, args.project_id)
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
