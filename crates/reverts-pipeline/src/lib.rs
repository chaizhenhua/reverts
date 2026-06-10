use std::error::Error;
use std::fmt;
use std::path::Path;

use reverts_analyze::enrich_program;
use reverts_emitter::{EmitError, emit_project};
use reverts_input::InputBundle;
use reverts_ir::{BindingName, ModuleId, ModuleKind};
use reverts_js::{ParseGoal, parse_error_message, parse_source};
use reverts_model::{EnrichedProgram, ProgramModel};
use reverts_observe::{AuditFinding, AuditReport, FindingCode};
use reverts_planner::{EmitPlan, ImportExportPlanner, PlanError, PlannedFile};

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
    let plan = planner
        .plan_enriched_program(&enrichment.program)
        .map_err(PipelineError::Plan)?;
    audit.extend(audit_emit_plan_synthesis(&plan));
    if !audit.is_clean() {
        return Ok(OutputRun {
            project: EmittedProject::default(),
            audit,
        });
    }

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
    input.module_source_slice(module_id).is_some()
}

fn audit_emit_plan_synthesis(plan: &EmitPlan) -> AuditReport {
    let mut audit = AuditReport::default();
    for file in &plan.files {
        audit.extend(audit_file_synthesis(file));
    }
    audit
}

fn audit_file_synthesis(file: &PlannedFile) -> AuditReport {
    let mut audit = AuditReport::default();
    let declarations = planned_declarations(file);

    for binding in &file.bindings {
        if !binding.source_backed {
            audit.push(
                AuditFinding::error(
                    FindingCode::SyntheticReferenceWithoutDeclaration,
                    "planned binding has no recovered source declaration",
                )
                .with_module(file.path.clone())
                .with_binding(binding.emitted.as_str()),
            );
        }
    }

    for export in &file.exports {
        if export.source_backed {
            continue;
        }
        if !declarations.contains(&export.binding) {
            audit.push(
                AuditFinding::error(
                    FindingCode::SyntheticReferenceWithoutDeclaration,
                    "planned export references a binding without declaration or import",
                )
                .with_module(file.path.clone())
                .with_binding(export.binding.as_str()),
            );
        }
    }

    audit
}

fn planned_declarations(file: &PlannedFile) -> std::collections::BTreeSet<BindingName> {
    file.imports
        .iter()
        .filter(|import| !import.source_backed)
        .map(|import| import.namespace.clone())
        .chain(file.bindings.iter().map(|binding| binding.emitted.clone()))
        .collect()
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
                AuditFinding::error(
                    FindingCode::UnparseableOutput,
                    parse_error_message(&error, "output could not be parsed"),
                )
                .with_module(file.path.clone()),
            );
        }
    }
    audit
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineError {
    Plan(PlanError),
    Emit(EmitError),
}

impl fmt::Display for PipelineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plan(error) => write!(formatter, "{error}"),
            Self::Emit(error) => write!(formatter, "{error}"),
        }
    }
}

impl Error for PipelineError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Plan(source) => Some(source),
            Self::Emit(source) => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use reverts_input::{
        InputBundle, InputRows, ModuleDependencyInput, ModuleDependencyTarget, ModuleInput,
        PackageAttributionInput, ProjectInput, SourceFileInput, SymbolInput,
    };
    use reverts_ir::{BindingName, BindingShape, ModuleId, ModuleKind};
    use reverts_observe::FindingCode;
    use reverts_planner::{EmitPlan, PlannedBinding, PlannedFile};

    use super::{audit_emit_plan_synthesis, generate_project_from_input};

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
    fn source_backed_package_import_is_not_emitted_twice() {
        let mut rows = rows_with_application_source(
            "import { map } from 'lodash/map'; export const answer = map;",
        );
        rows.modules.push(ModuleInput::package(
            ModuleId(2),
            "lodash_map",
            "node_modules/lodash/map.js",
            "lodash",
            Some("4.17.21".to_string()),
        ));
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
        let source = run.project.files[0].source.as_str();
        assert!(source.contains("import { map } from 'lodash/map';"));
        assert!(!source.contains("__pkg_lodash_map"));
        assert_eq!(source.matches("from 'lodash/map'").count(), 1);
    }

    #[test]
    fn commonjs_require_package_import_uses_surface_attribution_without_synthetic_import() {
        let mut rows = rows_with_application_source(
            "const add = require('pkg/add'); export const total = add(1, 2);",
        );
        rows.modules.push(ModuleInput::package(
            ModuleId(2),
            "pkg_add",
            "node_modules/pkg/add.js",
            "pkg",
            Some("1.2.3".to_string()),
        ));
        rows.package_attributions.push(
            PackageAttributionInput::accepted_external(ModuleId(2), "pkg", "1.2.3", "pkg/add")
                .with_subpath("add"),
        );
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        let source = run.project.files[0].source.as_str();
        assert!(source.contains("require('pkg/add')"));
        assert!(!source.contains("__pkg_pkg_add"));
    }

    #[test]
    fn source_backed_default_export_does_not_require_synthetic_default_binding() {
        let rows = rows_with_application_source("export default () => 42;");
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(run.audit.is_clean());
        assert_eq!(run.project.files.len(), 1);
        assert!(
            run.project.files[0]
                .source
                .contains("export default () => 42")
        );
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

    #[test]
    fn symbol_not_recovered_from_source_is_rejected_before_emit() {
        let mut rows = rows_with_application_source("export const real = 1;");
        rows.symbols.push(SymbolInput {
            module_id: ModuleId(1),
            name: "missing".to_string(),
        });
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should emit");

        assert!(
            run.audit
                .has(FindingCode::SyntheticReferenceWithoutDeclaration)
        );
        assert!(run.project.files.is_empty());
    }

    #[test]
    fn unresolved_ast_read_is_rejected_by_first_audit_gate() {
        let rows = rows_with_application_source("missing();");
        let input = InputBundle::from_rows(rows).expect("fixture rows should be valid");

        let run = generate_project_from_input(input).expect("fixture should return audit");

        assert!(run.audit.has(FindingCode::MissingDefinition));
        assert!(run.project.files.is_empty());
    }

    #[test]
    fn synthesis_audit_accepts_declared_export_and_rejects_missing_export_binding() {
        let mut file = PlannedFile::new("src/index.ts");
        file.add_binding(PlannedBinding::new(
            BindingName::new("real"),
            BindingName::new("real"),
            BindingShape::Unknown,
            true,
        ));
        file.add_export(BindingName::new("real"));
        file.add_export(BindingName::new("missing"));
        let mut plan = EmitPlan::default();
        plan.push_file(file);

        let audit = audit_emit_plan_synthesis(&plan);

        assert_eq!(audit.findings().len(), 1);
        assert_eq!(
            audit.findings()[0].code,
            FindingCode::SyntheticReferenceWithoutDeclaration
        );
        assert_eq!(audit.findings()[0].binding.as_deref(), Some("missing"));
    }
}
