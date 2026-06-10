use std::error::Error;
use std::fmt;

use reverts_ir::BindingName;
use reverts_js::{
    GeneratedExport, GeneratedImport, format_source_with_module_items, parse_error_message,
    sanitize_identifier,
};
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
    let mut generated_imports = Vec::new();

    for import in &file.imports {
        if import.source_backed {
            continue;
        }
        if let Some(specifier) = import.resolution.specifier() {
            generated_imports.push(GeneratedImport::new(
                emit_binding_name(&import.namespace),
                specifier,
            ));
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

    let formatted = format_source_with_module_items(
        &body_source,
        &generated_imports,
        &generated_exports,
        file.source_strategy().path_hint(file.path.as_str()),
        file.source_strategy().parse_goal(),
    )
    .map_err(|source_error| EmitError::UnparseableOutput {
        path: file.path.clone(),
        message: parse_error_message(&source_error, "output could not be parsed"),
    })?;

    Ok(EmittedFile {
        path: file.path.clone(),
        source: formatted,
    })
}

fn emit_binding_name(binding: &BindingName) -> String {
    sanitize_identifier(binding.as_str())
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
            },
            source_backed: false,
        });
        file.push_source("const answer = __pkg.answer;");
        file.add_export(BindingName::new("answer"));
        let mut plan = EmitPlan::default();
        plan.push_file(file);

        let project = emit_project(&plan).expect("planned file should emit");

        let source = project.files[0].source.as_str();
        assert!(source.contains("import * as __pkg from 'pkg';"));
        assert!(source.contains("const answer = __pkg.answer;"));
        assert!(source.contains("export { answer };"));
    }

    #[test]
    fn source_backed_imports_and_exports_are_not_emitted_twice() {
        let mut file = PlannedFile::new("src/index.ts");
        file.add_import(PlannedImport {
            namespace: BindingName::new("__pkg"),
            resolution: PackageResolution::External {
                package_name: "pkg".to_string(),
                specifier: "pkg".to_string(),
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
    fn synthetic_structural_items_sanitize_identifiers_before_ast_codegen() {
        let mut file = PlannedFile::new("src/index.ts");
        file.add_import(PlannedImport {
            namespace: BindingName::new("pkg-name/value"),
            resolution: PackageResolution::External {
                package_name: "pkg-name".to_string(),
                specifier: "pkg-name/value".to_string(),
            },
            source_backed: false,
        });
        file.push_source("const _class = pkg_name_value.answer;");
        file.add_export(BindingName::new("class"));
        let mut plan = EmitPlan::default();
        plan.push_file(file);

        let project = emit_project(&plan).expect("planned file should emit");

        let source = project.files[0].source.as_str();
        assert!(source.contains("import * as pkg_name_value from 'pkg-name/value';"));
        assert!(source.contains("export { _class };"));
    }
}
