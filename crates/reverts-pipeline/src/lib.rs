use std::error::Error;
use std::fmt;
use std::path::Path;

use reverts_analyze::enrich_program;
use reverts_emitter::{EmitError, emit_project};
use reverts_input::InputBundle;
use reverts_js::{JsError, ParseGoal, parse_source};
use reverts_model::ProgramModel;
use reverts_observe::{AuditFinding, AuditReport, FindingCode};
use reverts_planner::ImportExportPlanner;

pub use reverts_emitter::{EmittedFile, EmittedProject};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputRun {
    pub project: EmittedProject,
    pub audit: AuditReport,
}

pub fn generate_project_from_input(input: InputBundle) -> Result<OutputRun, PipelineError> {
    let model = ProgramModel::from_input(input);
    let enrichment = enrich_program(model);
    let planner = ImportExportPlanner;
    let plan = planner.plan_enriched_program(&enrichment.program);
    let project = emit_project(&plan).map_err(PipelineError::Emit)?;

    let mut audit = enrichment.audit;
    audit.extend(audit_emitted_project_parse(&project));

    Ok(OutputRun { project, audit })
}

fn audit_emitted_project_parse(project: &EmittedProject) -> AuditReport {
    let mut audit = AuditReport::default();
    for file in &project.files {
        if let Err(error) = parse_source(
            file.source.as_str(),
            Some(Path::new(file.path.as_str())),
            ParseGoal::TypeScript,
        ) {
            audit.push(
                AuditFinding::error(FindingCode::UnparseableOutput, parse_error_message(&error))
                    .with_module(file.path.clone()),
            );
        }
    }
    audit
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
pub enum PipelineError {
    Emit(EmitError),
}

impl fmt::Display for PipelineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Emit(error) => write!(formatter, "{error}"),
        }
    }
}

impl Error for PipelineError {}

#[cfg(test)]
mod tests {
    use reverts_input::{
        InputBundle, InputRows, ModuleDependencyInput, ModuleDependencyTarget, ModuleInput,
        PackageAttributionInput, ProjectInput, SymbolInput,
    };
    use reverts_ir::ModuleId;
    use reverts_observe::FindingCode;

    use super::generate_project_from_input;

    fn rows_with_application_module() -> InputRows {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "app", "src/index.ts"));
        rows
    }

    #[test]
    fn complete_fixture_generates_parseable_project_with_package_match_and_semantic_names() {
        let mut rows = rows_with_application_module();
        rows.modules.push(ModuleInput::package(
            ModuleId(2),
            "lodash_map",
            "node_modules/lodash/map.js",
            "lodash",
            Some("4.17.21".to_string()),
        ));
        rows.symbols.push(SymbolInput {
            module_id: ModuleId(1),
            name: "activate".to_string(),
        });
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(1),
            target: ModuleDependencyTarget::Package {
                specifier: "lodash/map".to_string(),
            },
        });
        rows.package_attributions.push(
            PackageAttributionInput::accepted_external(
                ModuleId(2),
                "lodash",
                "4.17.21",
                "lodash/map",
            )
            .with_subpath("map"),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        assert_eq!(run.project.files.len(), 1);
        let source = run.project.files[0].source.as_str();
        assert!(source.contains("import * as __pkg_lodash_map from 'lodash/map';"));
        assert!(source.contains("const activate: any = undefined as any;"));
        assert!(source.contains("export { activate };"));
    }

    #[test]
    fn unknown_package_surface_reports_unresolvable_import_without_external_tools() {
        let mut rows = rows_with_application_module();
        rows.dependencies.push(ModuleDependencyInput {
            from_module_id: ModuleId(1),
            target: ModuleDependencyTarget::Package {
                specifier: "lodash/map".to_string(),
            },
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.has(FindingCode::UnresolvableBareImport));
        assert_eq!(run.project.files.len(), 1);
        assert!(!run.project.files[0].source.contains("lodash/map"));
    }

    #[test]
    fn semantic_naming_sanitizes_reserved_symbol_names_in_output() {
        let mut rows = rows_with_application_module();
        rows.symbols.push(SymbolInput {
            module_id: ModuleId(1),
            name: "class".to_string(),
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        assert!(run.project.files[0].source.contains("const _class: any"));
        assert!(run.project.files[0].source.contains("export { _class };"));
    }

    #[test]
    fn duplicate_input_symbols_emit_single_declaration() {
        let mut rows = rows_with_application_module();
        rows.symbols.push(SymbolInput {
            module_id: ModuleId(1),
            name: "activate".to_string(),
        });
        rows.symbols.push(SymbolInput {
            module_id: ModuleId(1),
            name: "activate".to_string(),
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert_eq!(
            run.project.files[0]
                .source
                .matches("const activate: any")
                .count(),
            1
        );
    }
}
