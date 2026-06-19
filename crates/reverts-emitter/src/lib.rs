use std::error::Error;
use std::fmt;

use reverts_ir::{BindingName, InferredType};
use reverts_js::{
    CompilerLowering, FormatSourceRequest, GeneratedExport, GeneratedImport, GeneratedRename,
    GeneratedRenameScope, GeneratedTypeAnnotation, GeneratedTypeKind,
    format_source_with_module_items_request, sanitize_identifier,
};
use reverts_observe::{AuditFinding, FindingCode};
use reverts_planner::{
    CompilerPreservationAction, EmitPlan, PlannedFile, PlannedRenameScope, ValidatedEmitPlan,
};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EmittedProject {
    pub files: Vec<EmittedFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmittedFile {
    pub path: String,
    pub source: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EmitOutcome {
    pub project: EmittedProject,
    pub findings: Vec<AuditFinding>,
}

pub fn emit_project(plan: &EmitPlan) -> Result<EmitOutcome, EmitError> {
    emit_project_unchecked(plan)
}

pub fn emit_validated_project(plan: &ValidatedEmitPlan) -> Result<EmitOutcome, EmitError> {
    let plan_files = plan.files().collect::<Vec<_>>();
    let mut files = Vec::with_capacity(plan_files.len());
    let mut findings = Vec::new();
    for file in plan_files {
        let (emitted, finding) = emit_file(file)?;
        files.push(emitted);
        if let Some(finding) = finding {
            findings.push(finding);
        }
    }
    Ok(EmitOutcome {
        project: EmittedProject { files },
        findings,
    })
}

fn emit_project_unchecked(plan: &EmitPlan) -> Result<EmitOutcome, EmitError> {
    let mut files = Vec::with_capacity(plan.files.len());
    let mut findings = Vec::new();
    for file in &plan.files {
        let (emitted, finding) = emit_file(file)?;
        files.push(emitted);
        if let Some(finding) = finding {
            findings.push(finding);
        }
    }
    Ok(EmitOutcome {
        project: EmittedProject { files },
        findings,
    })
}

fn emit_file(file: &PlannedFile) -> Result<(EmittedFile, Option<AuditFinding>), EmitError> {
    let mut generated_imports = Vec::new();

    for import in &file.imports {
        if import.source_backed {
            continue;
        }
        if let Some(specifier) = import.resolution.specifier() {
            let mut generated_import =
                GeneratedImport::new(emit_binding_name(&import.namespace), specifier);
            if let Some(import_attributes) = import.resolution.import_attributes() {
                for (key, value) in import_attributes {
                    generated_import =
                        generated_import.with_attribute(key.as_str(), value.as_str());
                }
            }
            generated_imports.push(generated_import);
        }
    }

    let body_source = file.body.join("\n");
    let mut generated_exports = Vec::new();

    for export in &file.exports {
        if export.source_backed {
            continue;
        }
        generated_exports.push(GeneratedExport::new(emit_binding_name(&export.binding)));
    }
    let generated_renames = file
        .readability_renames
        .iter()
        .map(|rename| {
            let mut generated = GeneratedRename::new(
                emit_binding_name(&rename.original),
                emit_binding_name(&rename.renamed),
            );
            match rename.scope {
                PlannedRenameScope::Module => {}
                PlannedRenameScope::All => {
                    generated.scope = GeneratedRenameScope::All;
                }
                PlannedRenameScope::BindingIndex(binding_index) => {
                    generated.scope = GeneratedRenameScope::BindingIndex(binding_index);
                }
            }
            generated
        })
        .collect::<Vec<_>>();
    let generated_type_annotations = file
        .type_annotations
        .iter()
        .filter_map(|annotation| {
            generated_type_kind(annotation.ty).map(|kind| {
                GeneratedTypeAnnotation::new(emit_binding_name(&annotation.binding), kind)
            })
        })
        .collect::<Vec<_>>();
    let lowering = compiler_lowering(file.compiler_preservation.action);

    // Per ADR 0002 the emitter is faithful, not corrective. Two paths reach
    // raw-body emission:
    //   1. `should_preserve_raw_source_body` — template-literal preservation:
    //      OXC codegen would normalize whitespace inside quasis. In this mode
    //      we may still apply AST-guided identifier-span renames, but the
    //      source-preserving pass reparses the output and proves template raw
    //      chunks are unchanged before accepting it. No finding; this is
    //      deliberate preservation, not an implicit repair path.
    //   2. `format_source_with_module_items_and_renames` returns Err — the
    //      injection pass refused the body (e.g. `const X;`, JSX comma
    //      patterns). The raw body ships without the planned imports /
    //      exports / renames, so we record a Warning finding listing what
    //      was dropped. Downstream `audit_emitted_project_parse` still
    //      reports the unparseable bytes as an Error.
    let (formatted, finding) = if should_preserve_raw_source_body(
        body_source.as_str(),
        &generated_imports,
        &generated_exports,
        &generated_renames,
        &generated_type_annotations,
        lowering,
    ) {
        (body_source, None)
    } else {
        match format_source_with_module_items_request(FormatSourceRequest {
            body_source: &body_source,
            generated_imports: &generated_imports,
            generated_exports: &generated_exports,
            readability_renames: &generated_renames,
            type_annotations: &generated_type_annotations,
            infer_literal_types: true,
            path_hint: file.source_strategy().path_hint(file.path.as_str()),
            goal: file.source_strategy().parse_goal(),
            lowering,
        }) {
            Ok(formatted) => (formatted, None),
            Err(error) => {
                let finding = AuditFinding::warning(
                    FindingCode::EmitterRawBodyPreservedAfterInjectionFailure,
                    format!(
                        "dropped {} import / {} export / {} rename injection(s); raw body retained ({error})",
                        generated_imports.len(),
                        generated_exports.len(),
                        generated_renames.len(),
                    ),
                )
                .with_module(file.path.clone());
                (body_source, Some(finding))
            }
        }
    };

    Ok((
        EmittedFile {
            path: file.path.clone(),
            source: add_typescript_compat_header(
                formatted,
                file.compiler_preservation.action.preservation_banner(),
            ),
        },
        finding,
    ))
}

fn should_preserve_raw_source_body(
    body_source: &str,
    generated_imports: &[GeneratedImport],
    generated_exports: &[GeneratedExport],
    generated_renames: &[GeneratedRename],
    generated_type_annotations: &[GeneratedTypeAnnotation],
    lowering: CompilerLowering,
) -> bool {
    // Template literal raw chunks are runtime-observable. OXC codegen
    // reprints a parsed AST and can normalize whitespace-only template
    // quasis, which changes strings such as prompt/system-message bodies.
    // If the file is otherwise already source-backed (no synthetic module
    // items, no explicit renames, no compiler lowering), validate that it
    // parses but keep the original bytes for the body.
    body_source.contains('`')
        && generated_imports.is_empty()
        && generated_exports.is_empty()
        && generated_renames.is_empty()
        && generated_type_annotations.is_empty()
        && lowering == CompilerLowering::None
}

fn emit_binding_name(binding: &BindingName) -> String {
    sanitize_identifier(binding.as_str())
}

const fn generated_type_kind(ty: InferredType) -> Option<GeneratedTypeKind> {
    match ty {
        InferredType::Unknown => None,
        InferredType::Never => Some(GeneratedTypeKind::Never),
        InferredType::String => Some(GeneratedTypeKind::String),
        InferredType::Number => Some(GeneratedTypeKind::Number),
        InferredType::Boolean => Some(GeneratedTypeKind::Boolean),
        InferredType::BigInt => Some(GeneratedTypeKind::BigInt),
        InferredType::Null => Some(GeneratedTypeKind::Null),
        InferredType::Undefined => Some(GeneratedTypeKind::Undefined),
    }
}

const fn compiler_lowering(action: CompilerPreservationAction) -> CompilerLowering {
    match action {
        CompilerPreservationAction::PreserveBabelTranspiledOutput => CompilerLowering::Babel,
        CompilerPreservationAction::PreserveEsbuildHelpers => CompilerLowering::Esbuild,
        CompilerPreservationAction::PreserveWebpackRuntime => CompilerLowering::Webpack,
        CompilerPreservationAction::DirectModuleSource
        | CompilerPreservationAction::PreserveRollupFacade
        | CompilerPreservationAction::PreserveTerserMinifiedOutput => CompilerLowering::None,
    }
}

fn add_typescript_compat_header(source: String, preservation_banner: Option<&str>) -> String {
    let banner_line = preservation_banner
        .map(|banner| format!("// {banner}\n"))
        .unwrap_or_default();

    if source
        .lines()
        .take(3)
        .any(|line| line.contains("@ts-nocheck"))
    {
        return if banner_line.is_empty() {
            source
        } else {
            format!("{banner_line}{source}")
        };
    }

    if let Some(rest) = source.strip_prefix("#!")
        && let Some((hashbang, body)) = rest.split_once('\n')
    {
        return format!("#!{hashbang}\n{banner_line}// @ts-nocheck\n{body}");
    }

    format!("{banner_line}// @ts-nocheck\n{source}")
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
    use std::collections::BTreeMap;

    use reverts_ir::BindingName;
    use reverts_package::PackageResolution;
    use reverts_planner::{EmitPlan, PlannedFile, PlannedImport};

    use super::emit_project;

    #[test]
    fn planned_source_body_emits_parseable_source_without_synthetic_implementation() {
        let mut file = PlannedFile::new("src/index.ts");
        file.push_source("export const answer = 42;");
        let mut plan = EmitPlan::default();
        plan.push_file(file);

        let project = emit_project(&plan)
            .expect("planned file should emit")
            .project;

        assert_eq!(project.files[0].path, "src/index.ts");
        assert!(project.files[0].source.starts_with("// @ts-nocheck"));
        assert!(
            project.files[0]
                .source
                .contains("export const answer: number = 42")
        );
        assert!(!project.files[0].source.contains("undefined as any"));
    }

    #[test]
    fn planned_structural_imports_and_exports_are_emitted_through_ast_codegen() {
        let mut file = PlannedFile::new("src/index.ts");
        file.add_import(PlannedImport {
            namespace: BindingName::new("__pkg"),
            resolution: PackageResolution::External {
                package_name: "pkg".to_string(),
                specifier: "pkg".to_string(),
                import_attributes: Default::default(),
            },
            source_backed: false,
        });
        file.push_source("const answer = __pkg.answer;");
        file.add_export(BindingName::new("answer"));
        let mut plan = EmitPlan::default();
        plan.push_file(file);

        let project = emit_project(&plan)
            .expect("planned file should emit")
            .project;

        let source = project.files[0].source.as_str();
        assert!(source.contains("import * as pkg from 'pkg';"));
        assert!(source.contains("const answer: typeof pkg.answer = pkg.answer;"));
        assert!(source.contains("export { answer };"));
    }

    #[test]
    fn planned_json_package_import_emits_import_attributes() {
        let mut file = PlannedFile::new("src/index.ts");
        file.add_import(PlannedImport {
            namespace: BindingName::new("colors"),
            resolution: PackageResolution::External {
                package_name: "css-color-names".to_string(),
                specifier: "css-color-names".to_string(),
                import_attributes: BTreeMap::from([("type".to_string(), "json".to_string())]),
            },
            source_backed: false,
        });
        file.push_source("const aliceblue = colors.default.aliceblue;");
        let mut plan = EmitPlan::default();
        plan.push_file(file);

        let project = emit_project(&plan)
            .expect("planned file should emit")
            .project;

        let source = project.files[0].source.as_str();
        assert!(
            source.contains("import * as colors from 'css-color-names' with { type: 'json' };"),
            "{source}"
        );
        assert!(source.contains(
            "const aliceblue: typeof colors.default.aliceblue = colors.default.aliceblue;"
        ));
    }

    #[test]
    fn source_backed_imports_and_exports_are_not_emitted_twice() {
        let mut file = PlannedFile::new("src/index.ts");
        file.add_import(PlannedImport {
            namespace: BindingName::new("__pkg"),
            resolution: PackageResolution::External {
                package_name: "pkg".to_string(),
                specifier: "pkg".to_string(),
                import_attributes: Default::default(),
            },
            source_backed: true,
        });
        file.push_source("import { answer } from 'pkg'; export { answer };");
        file.add_export_with_source_backed(BindingName::new("answer"), true);
        let mut plan = EmitPlan::default();
        plan.push_file(file);

        let project = emit_project(&plan)
            .expect("planned file should emit")
            .project;

        let source = project.files[0].source.as_str();
        assert_eq!(source.matches("from 'pkg'").count(), 1);
        assert_eq!(source.matches("export { answer };").count(), 1);
        assert!(!source.contains("import * as __pkg"));
    }

    #[test]
    fn source_backed_template_literal_raw_whitespace_is_preserved() {
        let mut file = PlannedFile::new("src/index.ts");
        file.push_source(
            "const prompt = `<system-reminder>\n${items.join(`\n`)}\n\n      IMPORTANT\n</system-reminder>\n`;\nexport { prompt };",
        );
        let mut plan = EmitPlan::default();
        plan.push_file(file);

        let project = emit_project(&plan)
            .expect("planned file should emit")
            .project;

        let source = project.files[0].source.as_str();
        assert!(
            source.contains("${items.join(`\n`)}\n\n      IMPORTANT"),
            "{source}"
        );
    }

    #[test]
    fn synthetic_structural_items_sanitize_identifiers_before_ast_codegen() {
        let mut file = PlannedFile::new("src/index.ts");
        file.add_import(PlannedImport {
            namespace: BindingName::new("pkg-name/value"),
            resolution: PackageResolution::External {
                package_name: "pkg-name".to_string(),
                specifier: "pkg-name/value".to_string(),
                import_attributes: Default::default(),
            },
            source_backed: false,
        });
        file.push_source("const _class = pkg_name_value.answer; console.log(pkg_name_value);");
        file.add_export(BindingName::new("class"));
        let mut plan = EmitPlan::default();
        plan.push_file(file);

        let project = emit_project(&plan)
            .expect("planned file should emit")
            .project;

        let source = project.files[0].source.as_str();
        assert!(source.contains("import * as pkg_name_value from 'pkg-name/value';"));
        assert!(source.contains("console.log(pkg_name_value);"));
        assert!(source.contains("export { _class };"));
    }

    #[test]
    fn unparseable_body_with_planned_export_emits_raw_body_plus_audit_finding() {
        use reverts_observe::{FindingCode, Severity};

        let mut file = PlannedFile::new("src/broken.ts");
        file.push_source("const X;");
        file.add_export(BindingName::new("X"));
        let mut plan = EmitPlan::default();
        plan.push_file(file);

        let outcome = emit_project(&plan).expect("emitter never aborts on body parse failure");

        // Raw body still ships; planned export injection is dropped.
        let source = outcome.project.files[0].source.as_str();
        assert!(source.contains("const X;"), "{source}");
        assert!(!source.contains("export { X };"), "{source}");

        // Audit finding tells the consumer one export injection was dropped.
        assert_eq!(outcome.findings.len(), 1, "{:?}", outcome.findings);
        let finding = &outcome.findings[0];
        assert_eq!(
            finding.code,
            FindingCode::EmitterRawBodyPreservedAfterInjectionFailure
        );
        assert_eq!(finding.severity, Severity::Warning);
        assert_eq!(finding.module.as_deref(), Some("src/broken.ts"));
        assert!(finding.message.contains("1 export"), "{}", finding.message);
    }

    #[test]
    fn typescript_compat_header_preserves_hashbang_first() {
        let mut file = PlannedFile::new("src/bin.ts");
        file.push_source("#!/usr/bin/env node\nconsole.log('ok');");
        let mut plan = EmitPlan::default();
        plan.push_file(file);

        let project = emit_project(&plan)
            .expect("planned file should emit")
            .project;

        assert!(
            project.files[0]
                .source
                .starts_with("#!/usr/bin/env node\n// @ts-nocheck")
        );
    }
}
