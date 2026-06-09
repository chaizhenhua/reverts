use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use reverts_input::sqlite::{SqliteInputError, load_project_bundle_from_sqlite};
use reverts_observe::AuditReport;
use reverts_pipeline::{EmittedFile, PipelineError, generate_project_from_input};

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
            .is_some_and(|argument| argument == "generate-project-v2")
        {
            args.remove(0);
        }
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--input" => input = Some(next_path(&mut args, "--input")?),
                "--output" => output = Some(next_path(&mut args, "--output")?),
                "--project-id" => {
                    project_id = Some(parse_project_id(next_value(&mut args, "--project-id")?)?);
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

fn next_path(
    args: &mut impl Iterator<Item = String>,
    flag: &'static str,
) -> Result<PathBuf, CliError> {
    args.next()
        .map(PathBuf::from)
        .ok_or(CliError::MissingArgument(flag))
}

fn next_value(
    args: &mut impl Iterator<Item = String>,
    flag: &'static str,
) -> Result<String, CliError> {
    args.next().ok_or(CliError::MissingArgument(flag))
}

fn parse_project_id(value: String) -> Result<u32, CliError> {
    let parsed = value
        .parse::<u32>()
        .map_err(|_error| CliError::InvalidProjectId(value.clone()))?;
    if parsed == 0 {
        Err(CliError::InvalidProjectId(value))
    } else {
        Ok(parsed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    MissingArgument(&'static str),
    InvalidProjectId(String),
    UnknownArgument(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingArgument(argument) => write!(formatter, "missing argument {argument}"),
            Self::InvalidProjectId(value) => write!(formatter, "invalid project id {value}"),
            Self::UnknownArgument(argument) => write!(formatter, "unknown argument {argument}"),
        }
    }
}

impl Error for CliError {}

pub fn run(args: impl IntoIterator<Item = String>) -> Result<(), CliRunError> {
    let args = GenerateProjectV2Args::parse(args).map_err(CliRunError::Args)?;
    let input = load_project_bundle_from_sqlite(&args.input, args.project_id)
        .map_err(CliRunError::LoadInput)?;
    let run = generate_project_from_input(input).map_err(CliRunError::Pipeline)?;

    if !run.audit.is_clean() {
        return Err(CliRunError::AuditRejected(format_audit_findings(
            &run.audit,
        )));
    }

    let written = write_emitted_project(&run.project.files, &args.output)?;
    println!(
        "generated project {} into {} with {written} files",
        args.project_id,
        args.output.display()
    );
    Ok(())
}

fn write_emitted_project(files: &[EmittedFile], output: &Path) -> Result<usize, CliRunError> {
    fs::create_dir_all(output).map_err(|source| CliRunError::WriteOutput {
        path: output.to_path_buf(),
        source,
    })?;

    for file in files {
        let path = checked_output_path(output, file.path.as_str())?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| CliRunError::WriteOutput {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        fs::write(&path, file.source.as_bytes())
            .map_err(|source| CliRunError::WriteOutput { path, source })?;
    }

    Ok(files.len())
}

fn checked_output_path(output: &Path, relative: &str) -> Result<PathBuf, CliRunError> {
    let relative = Path::new(relative);
    if relative.is_absolute() {
        return Err(CliRunError::UnsafeOutputPath(relative.to_path_buf()));
    }

    let mut path = output.to_path_buf();
    for component in relative.components() {
        match component {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(CliRunError::UnsafeOutputPath(relative.to_path_buf()));
            }
        }
    }
    Ok(path)
}

fn format_audit_findings(audit: &AuditReport) -> String {
    audit
        .findings()
        .iter()
        .take(20)
        .map(|finding| {
            format!(
                "{:?}: {}{}{}",
                finding.code,
                finding.message,
                finding
                    .module
                    .as_deref()
                    .map(|module| format!(" module={module}"))
                    .unwrap_or_default(),
                finding
                    .binding
                    .as_deref()
                    .map(|binding| format!(" binding={binding}"))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug)]
pub enum CliRunError {
    Args(CliError),
    LoadInput(SqliteInputError),
    Pipeline(PipelineError),
    AuditRejected(String),
    UnsafeOutputPath(PathBuf),
    WriteOutput { path: PathBuf, source: io::Error },
}

impl fmt::Display for CliRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Args(source) => write!(formatter, "{source}"),
            Self::LoadInput(source) => write!(formatter, "{source}"),
            Self::Pipeline(source) => write!(formatter, "{source}"),
            Self::AuditRejected(summary) => {
                write!(
                    formatter,
                    "generated project was rejected by audit:\n{summary}"
                )
            }
            Self::UnsafeOutputPath(path) => {
                write!(
                    formatter,
                    "emitted file path is not safe: {}",
                    path.display()
                )
            }
            Self::WriteOutput { path, source } => {
                write!(formatter, "failed to write {}: {source}", path.display())
            }
        }
    }
}

impl Error for CliRunError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Args(source) => Some(source),
            Self::LoadInput(source) => Some(source),
            Self::Pipeline(source) => Some(source),
            Self::WriteOutput { source, .. } => Some(source),
            Self::AuditRejected(_) | Self::UnsafeOutputPath(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{CliError, GenerateProjectV2Args, checked_output_path};

    #[test]
    fn parses_generate_project_v2_paths_without_external_process() {
        let args = GenerateProjectV2Args::parse([
            "generate-project-v2".to_string(),
            "--input".to_string(),
            "input.db".to_string(),
            "--project-id".to_string(),
            "13495".to_string(),
            "--output".to_string(),
            "out".to_string(),
        ])
        .expect("args should parse");

        assert_eq!(args.input, PathBuf::from("input.db"));
        assert_eq!(args.project_id, 13495);
        assert_eq!(args.output, PathBuf::from("out"));
    }

    #[test]
    fn project_id_must_be_positive_integer() {
        let error = GenerateProjectV2Args::parse([
            "--input".to_string(),
            "input.db".to_string(),
            "--project-id".to_string(),
            "0".to_string(),
            "--output".to_string(),
            "out".to_string(),
        ]);

        assert!(matches!(error, Err(CliError::InvalidProjectId(value)) if value == "0"));
    }

    #[test]
    fn output_paths_cannot_escape_output_directory() {
        let error = checked_output_path(PathBuf::from("out").as_path(), "../escape.ts");

        assert!(error.is_err());
    }
}
