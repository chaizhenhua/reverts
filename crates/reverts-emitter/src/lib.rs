use std::error::Error;
use std::fmt;
use std::path::Path;

use reverts_ir::BindingName;
use reverts_js::{JsError, format_source_pretty, sanitize_identifier};
use reverts_package::PackageResolution;
use reverts_planner::{EmitPlan, PlannedFile};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EmittedProject {
    pub files: Vec<EmittedFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmittedFile {
    pub path: String,
    pub source: String,
}

pub fn emit_project(plan: &EmitPlan) -> Result<EmittedProject, EmitError> {
    let mut files = Vec::with_capacity(plan.files.len());
    for file in &plan.files {
        files.push(emit_file(file)?);
    }
    Ok(EmittedProject { files })
}

fn emit_file(file: &PlannedFile) -> Result<EmittedFile, EmitError> {
    let mut lines = Vec::new();

    for import in &file.imports {
        if import.source_backed {
            continue;
        }
        if let Some(specifier) = accepted_specifier(&import.resolution) {
            lines.push(format!(
                "import * as {} from '{}';",
                emit_binding_name(&import.namespace),
                specifier
            ));
        }
    }

    for line in &file.body {
        lines.push(line.clone());
    }

    for export in &file.exports {
        if export.source_backed {
            continue;
        }
        lines.push(format!(
            "export {{ {} }};",
            emit_binding_name(&export.binding)
        ));
    }

    let source = if lines.is_empty() {
        "export {};".to_string()
    } else {
        lines.join("\n")
    };

    let formatted = format_source_pretty(
        &source,
        emit_path_hint(file),
        file.source_strategy().parse_goal(),
    )
    .map_err(|source_error| EmitError::UnparseableOutput {
        path: file.path.clone(),
        message: parse_error_message(&source_error),
    })?;

    Ok(EmittedFile {
        path: file.path.clone(),
        source: formatted,
    })
}

fn emit_path_hint(file: &PlannedFile) -> Option<&Path> {
    match file.source_strategy() {
        reverts_planner::SourceCompilerStrategy::DirectSource => {
            Some(Path::new(file.path.as_str()))
        }
        reverts_planner::SourceCompilerStrategy::WebpackRuntime
        | reverts_planner::SourceCompilerStrategy::EsbuildHelpers
        | reverts_planner::SourceCompilerStrategy::RollupFacade
        | reverts_planner::SourceCompilerStrategy::BabelTranspiled
        | reverts_planner::SourceCompilerStrategy::TerserMinified => None,
    }
}

fn accepted_specifier(resolution: &PackageResolution) -> Option<&str> {
    match resolution {
        PackageResolution::Builtin { specifier }
        | PackageResolution::External { specifier, .. }
        | PackageResolution::Local { specifier } => Some(specifier),
        PackageResolution::Rejected { .. } => None,
    }
}

fn emit_binding_name(binding: &BindingName) -> String {
    sanitize_identifier(binding.as_str())
}

fn parse_error_message(error: &JsError) -> String {
    match error {
        JsError::ParseFailed(errors) => errors.first().map_or_else(
            || "output could not be parsed".to_string(),
            |error| {
                let diagnostic = error
                    .diagnostics
                    .first()
                    .map_or("no diagnostic", String::as_str);
                format!(
                    "output could not be parsed as {}: {diagnostic}",
                    error.source_type
                )
            },
        ),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmitError {
    UnparseableOutput { path: String, message: String },
}

impl fmt::Display for EmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnparseableOutput { path, message } => write!(formatter, "{path}: {message}"),
        }
    }
}

impl Error for EmitError {}

#[cfg(test)]
mod tests {
    use reverts_planner::{EmitPlan, PlannedFile};

    use super::emit_project;

    #[test]
    fn planned_source_body_emits_parseable_source_without_synthetic_implementation() {
        let mut file = PlannedFile::new("src/index.ts");
        file.push_source("export const answer = 42;");
        let mut plan = EmitPlan::default();
        plan.push_file(file);

        let project = emit_project(&plan).expect("planned file should emit");

        assert_eq!(project.files[0].path, "src/index.ts");
        assert!(project.files[0].source.contains("export const answer = 42"));
        assert!(!project.files[0].source.contains("undefined as any"));
    }
}
