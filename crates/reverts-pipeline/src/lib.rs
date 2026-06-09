use std::error::Error;
use std::fmt;
use std::path::Path;

use reverts_analyze::enrich_program;
use reverts_emitter::{EmitError, emit_project};
use reverts_input::InputBundle;
use reverts_ir::{ModuleId, ModuleKind};
use reverts_js::{JsError, ParseGoal, parse_source};
use reverts_model::{EnrichedProgram, ProgramModel};
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
    let mut audit = enrichment.audit;
    audit.extend(audit_required_sources(&enrichment.program));
    if !audit.is_clean() {
        return Ok(OutputRun {
            project: EmittedProject::default(),
            audit,
        });
    }

    let planner = ImportExportPlanner;
    let plan = planner.plan_enriched_program(&enrichment.program);
    let project = emit_project(&plan).map_err(PipelineError::Emit)?;

    audit.extend(audit_emitted_project_parse(&project));

    Ok(OutputRun { project, audit })
}

fn audit_required_sources(program: &EnrichedProgram) -> AuditReport {
    let mut audit = AuditReport::default();
    for module in program.model().modules() {
        if module.kind == ModuleKind::Package {
            continue;
        }

        if has_module_source(program.model().input(), module.id) {
            continue;
        }

        let definitions = program.model().graph().definitions_for(module.id);
        if definitions.is_empty() {
            audit.push(
                AuditFinding::error(
                    FindingCode::MissingDefinition,
                    "module has no real source body to emit",
                )
                .with_module(module.id.0.to_string()),
            );
            continue;
        }

        for definition in definitions {
            audit.push(
                AuditFinding::error(
                    FindingCode::MissingDefinition,
                    "module has symbols but no real source body to emit",
                )
                .with_module(module.id.0.to_string())
                .with_binding(definition.as_str()),
            );
        }
    }
    audit
}

fn has_module_source(input: &InputBundle, module_id: ModuleId) -> bool {
    module_source(input, module_id).is_some()
}

fn module_source(input: &InputBundle, module_id: ModuleId) -> Option<&str> {
    let module = input.modules.iter().find(|module| module.id == module_id)?;
    let source_file_id = module.source_file_id?;
    let source = input
        .source_files
        .iter()
        .find(|source_file| source_file.id == source_file_id)?
        .source
        .as_deref()?;

    if let Some(span) = module.source_span {
        return source.get(span.byte_start as usize..span.byte_end as usize);
    }

    let module_count_for_source = input
        .modules
        .iter()
        .filter(|candidate| candidate.source_file_id == Some(source_file_id))
        .count();
    if module_count_for_source != 1 {
        return None;
    }

    Some(source)
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
        PackageAttributionInput, ProjectInput, SourceFileInput, SymbolInput,
    };
    use reverts_ir::{ModuleId, ModuleKind};
    use reverts_observe::FindingCode;

    use super::generate_project_from_input;

    fn rows_with_application_module() -> InputRows {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "app", "src/index.ts"));
        rows
    }

    fn rows_with_application_source(source: &str) -> InputRows {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "src/index.ts",
            Some(source.to_string()),
        ));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "app", "src/index.ts").with_source_file(1));
        rows
    }

    #[test]
    fn complete_fixture_generates_parseable_project_with_package_match_and_semantic_names() {
        let mut rows = rows_with_application_source("export function activate() { return 42; }");
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
        assert!(source.contains("export function activate()"));
        assert!(!source.contains("undefined as any"));
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
        assert!(run.project.files.is_empty());
    }

    #[test]
    fn missing_symbol_source_is_reported_without_emitting_generated_implementation() {
        let mut rows = rows_with_application_module();
        rows.symbols.push(SymbolInput {
            module_id: ModuleId(1),
            name: "activate".to_string(),
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.has(FindingCode::MissingDefinition));
        assert!(run.project.files.is_empty());
    }

    #[test]
    fn module_without_source_is_reported_without_emitting_empty_file() {
        let rows = rows_with_application_module();
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.findings().iter().any(|finding| {
            finding.code == FindingCode::MissingDefinition
                && finding.module.as_deref() == Some("1")
                && finding.binding.is_none()
        }));
        assert!(run.project.files.is_empty());
    }

    #[test]
    fn duplicate_input_symbols_emit_single_missing_source_finding() {
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
            run.audit
                .findings()
                .iter()
                .filter(|finding| finding.code == FindingCode::MissingDefinition
                    && finding.binding.as_deref() == Some("activate"))
                .count(),
            1
        );
        assert!(run.project.files.is_empty());
    }

    #[test]
    fn shared_bundle_source_is_not_duplicated_as_module_implementation() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("function bundle() {}".to_string()),
        ));
        rows.modules
            .push(ModuleInput::application(ModuleId(1), "m1", "modules/m1.ts").with_source_file(1));
        rows.modules.push(ModuleInput {
            id: ModuleId(2),
            kind: ModuleKind::Application,
            original_name: "m2".to_string(),
            semantic_path: "modules/m2.ts".to_string(),
            source_file_id: Some(1),
            source_span: None,
            package_name: None,
            package_version: None,
        });
        rows.symbols.push(SymbolInput {
            module_id: ModuleId(1),
            name: "one".to_string(),
        });
        rows.symbols.push(SymbolInput {
            module_id: ModuleId(2),
            name: "two".to_string(),
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.has(FindingCode::MissingDefinition));
        assert!(run.project.files.is_empty());
    }

    #[test]
    fn bundle_source_spans_emit_real_module_slices() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some("export const one = 1;\nexport const two = 2;".to_string()),
        ));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "one", "modules/one.ts")
                .with_source_file(1)
                .with_source_span(reverts_input::SourceSpan::new(0, 21)),
        );
        rows.modules.push(
            ModuleInput::application(ModuleId(2), "two", "modules/two.ts")
                .with_source_file(1)
                .with_source_span(reverts_input::SourceSpan::new(22, 43)),
        );
        rows.symbols.push(SymbolInput {
            module_id: ModuleId(1),
            name: "one".to_string(),
        });
        rows.symbols.push(SymbolInput {
            module_id: ModuleId(2),
            name: "two".to_string(),
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        assert_eq!(run.project.files.len(), 2);
        assert!(run.project.files[0].source.contains("export const one = 1"));
        assert!(run.project.files[1].source.contains("export const two = 2"));
    }
}
