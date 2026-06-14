use std::error::Error;
use std::fmt;

use reverts_ir::BindingName;
use reverts_js::{
    CompilerLowering, GeneratedExport, GeneratedImport, GeneratedRename,
    format_source_with_module_items_and_renames, parse_error_message, parse_source,
    sanitize_identifier,
};
use reverts_planner::{CompilerRecoveryAction, EmitPlan, PlannedFile};

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
            GeneratedRename::new(
                emit_binding_name(&rename.original),
                emit_binding_name(&rename.renamed),
            )
        })
        .collect::<Vec<_>>();
    let lowering = compiler_lowering(file.compiler_recovery.action);

    let formatted = if should_preserve_raw_source_body(
        body_source.as_str(),
        &generated_imports,
        &generated_exports,
        &generated_renames,
        lowering,
    ) {
        parse_source(
            body_source.as_str(),
            file.source_strategy().path_hint(file.path.as_str()),
            file.source_strategy().parse_goal(),
        )
        .map_err(|source_error| EmitError::UnparseableOutput {
            path: file.path.clone(),
            message: parse_error_message(&source_error, "output could not be parsed"),
        })?;
        body_source
    } else {
        format_source_with_module_items_and_renames(
            &body_source,
            &generated_imports,
            &generated_exports,
            &generated_renames,
            file.source_strategy().path_hint(file.path.as_str()),
            file.source_strategy().parse_goal(),
            lowering,
        )
        .map_err(|source_error| EmitError::UnparseableOutput {
            path: file.path.clone(),
            message: parse_error_message(&source_error, "output could not be parsed"),
        })?
    };

    Ok(EmittedFile {
        path: file.path.clone(),
        source: add_typescript_compat_header(
            formatted,
            file.compiler_recovery.action.recovery_banner(),
        ),
    })
}

fn should_preserve_raw_source_body(
    body_source: &str,
    generated_imports: &[GeneratedImport],
    generated_exports: &[GeneratedExport],
    generated_renames: &[GeneratedRename],
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
        && lowering == CompilerLowering::None
}

fn emit_binding_name(binding: &BindingName) -> String {
    sanitize_identifier(binding.as_str())
}

const fn compiler_lowering(action: CompilerRecoveryAction) -> CompilerLowering {
    match action {
        CompilerRecoveryAction::PreserveBabelTranspiledOutput => CompilerLowering::Babel,
        CompilerRecoveryAction::PreserveEsbuildHelpers => CompilerLowering::Esbuild,
        CompilerRecoveryAction::PreserveWebpackRuntime => CompilerLowering::Webpack,
        CompilerRecoveryAction::DirectModuleSource
        | CompilerRecoveryAction::PreserveRollupFacade
        | CompilerRecoveryAction::PreserveTerserMinifiedOutput => CompilerLowering::None,
    }
}

fn add_typescript_compat_header(source: String, recovery_banner: Option<&str>) -> String {
    let banner_line = recovery_banner
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

        let project = emit_project(&plan).expect("planned file should emit");

        assert_eq!(project.files[0].path, "src/index.ts");
        assert!(project.files[0].source.starts_with("// @ts-nocheck"));
        assert!(project.files[0].source.contains("export const answer = 42"));
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

        let project = emit_project(&plan).expect("planned file should emit");

        let source = project.files[0].source.as_str();
        assert!(source.contains("import * as pkg from 'pkg';"));
        assert!(source.contains("const answer = pkg.answer;"));
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

        let project = emit_project(&plan).expect("planned file should emit");

        let source = project.files[0].source.as_str();
        assert!(
            source.contains("import * as colors from 'css-color-names' with { type: 'json' };"),
            "{source}"
        );
        assert!(source.contains("const aliceblue = colors.default.aliceblue;"));
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

        let project = emit_project(&plan).expect("planned file should emit");

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

        let project = emit_project(&plan).expect("planned file should emit");

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

        let project = emit_project(&plan).expect("planned file should emit");

        let source = project.files[0].source.as_str();
        assert!(source.contains("import * as pkg_name_value from 'pkg-name/value';"));
        assert!(source.contains("console.log(pkg_name_value);"));
        assert!(source.contains("export { _class };"));
    }

    #[test]
    fn typescript_compat_header_preserves_hashbang_first() {
        let mut file = PlannedFile::new("src/bin.ts");
        file.push_source("#!/usr/bin/env node\nconsole.log('ok');");
        let mut plan = EmitPlan::default();
        plan.push_file(file);

        let project = emit_project(&plan).expect("planned file should emit");

        assert!(
            project.files[0]
                .source
                .starts_with("#!/usr/bin/env node\n// @ts-nocheck")
        );
    }
}
