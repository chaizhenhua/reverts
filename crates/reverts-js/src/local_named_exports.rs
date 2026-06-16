use std::collections::BTreeSet;

use oxc_ast::{
    AstBuilder, NONE,
    ast::{ExportNamedDeclaration, ImportOrExportKind, ModuleExportName, Program, Statement},
};
use oxc_span::SPAN;

pub(crate) fn coalesce_simple_local_named_exports_in_program<'a>(
    program: &mut Program<'a>,
    builder: &AstBuilder<'a>,
) {
    let mut bindings = BTreeSet::<String>::new();
    let mut first_index = None::<usize>;
    let mut duplicate_indices = Vec::<usize>::new();

    for (index, statement) in program.body.iter().enumerate() {
        let Statement::ExportNamedDeclaration(declaration) = statement else {
            continue;
        };
        let Some(statement_bindings) = simple_local_named_export_bindings(declaration) else {
            continue;
        };
        bindings.extend(statement_bindings);
        if first_index.is_none() {
            first_index = Some(index);
        } else {
            duplicate_indices.push(index);
        }
    }

    if duplicate_indices.is_empty() {
        return;
    }
    let Some(first_index) = first_index else {
        return;
    };

    let mut specifiers = builder.vec();
    for binding in bindings {
        let local = builder.module_export_name_identifier_reference(SPAN, binding.as_str());
        let exported = builder.module_export_name_identifier_name(SPAN, binding.as_str());
        specifiers.push(builder.export_specifier(SPAN, local, exported, ImportOrExportKind::Value));
    }
    program.body[first_index] =
        Statement::ExportNamedDeclaration(builder.alloc_export_named_declaration(
            SPAN,
            None,
            specifiers,
            None,
            ImportOrExportKind::Value,
            NONE,
        ));

    for index in duplicate_indices.iter().rev() {
        program.body.remove(*index);
    }
}

fn simple_local_named_export_bindings(
    declaration: &ExportNamedDeclaration<'_>,
) -> Option<Vec<String>> {
    if declaration.declaration.is_some()
        || declaration.source.is_some()
        || declaration.export_kind != ImportOrExportKind::Value
        || declaration.with_clause.is_some()
        || declaration.specifiers.is_empty()
    {
        return None;
    }

    let mut bindings = Vec::<String>::new();
    for specifier in &declaration.specifiers {
        if specifier.export_kind != ImportOrExportKind::Value {
            return None;
        }
        let local = module_export_name_text(&specifier.local)?;
        let exported = module_export_name_text(&specifier.exported)?;
        if local != exported {
            return None;
        }
        bindings.push(local);
    }
    Some(bindings)
}

pub(crate) fn module_export_name_text(name: &ModuleExportName<'_>) -> Option<String> {
    match name {
        ModuleExportName::IdentifierName(identifier) => Some(identifier.name.as_str().to_string()),
        ModuleExportName::IdentifierReference(identifier) => {
            Some(identifier.name.as_str().to_string())
        }
        ModuleExportName::StringLiteral(_) => None,
    }
}
