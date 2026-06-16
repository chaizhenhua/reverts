use oxc_ast::{
    AstBuilder, NONE,
    ast::{ImportOrExportKind, Statement},
};
use oxc_span::SPAN;

use crate::{GeneratedExport, GeneratedImport};

pub(crate) fn generated_import_statement<'a>(
    builder: &AstBuilder<'a>,
    generated_import: &GeneratedImport,
) -> Statement<'a> {
    let local = builder.binding_identifier(SPAN, generated_import.namespace.as_str());
    let specifier = builder.import_declaration_specifier_import_namespace_specifier(SPAN, local);
    let specifiers = Some(builder.vec1(specifier));
    let source = builder.string_literal(SPAN, generated_import.specifier.as_str(), None);
    let with_clause = if generated_import.attributes.is_empty() {
        None
    } else {
        let mut entries = builder.vec();
        for (key, value) in &generated_import.attributes {
            entries.push(builder.import_attribute(
                SPAN,
                builder.import_attribute_key_identifier_name(SPAN, key.as_str()),
                builder.string_literal(SPAN, value.as_str(), None),
            ));
        }
        Some(builder.alloc_with_clause(SPAN, builder.identifier_name(SPAN, "with"), entries))
    };
    Statement::ImportDeclaration(builder.alloc_import_declaration(
        SPAN,
        specifiers,
        source,
        None,
        with_clause,
        ImportOrExportKind::Value,
    ))
}

pub(crate) fn generated_export_statement<'a>(
    builder: &AstBuilder<'a>,
    generated_export: &GeneratedExport,
) -> Statement<'a> {
    let local =
        builder.module_export_name_identifier_reference(SPAN, generated_export.binding.as_str());
    let exported =
        builder.module_export_name_identifier_name(SPAN, generated_export.binding.as_str());
    let specifier = builder.export_specifier(SPAN, local, exported, ImportOrExportKind::Value);
    let specifiers = builder.vec1(specifier);
    Statement::ExportNamedDeclaration(builder.alloc_export_named_declaration(
        SPAN,
        None,
        specifiers,
        None,
        ImportOrExportKind::Value,
        NONE,
    ))
}

pub(crate) fn empty_export_statement<'a>(builder: &AstBuilder<'a>) -> Statement<'a> {
    Statement::ExportNamedDeclaration(builder.alloc_export_named_declaration(
        SPAN,
        None,
        builder.vec(),
        None,
        ImportOrExportKind::Value,
        NONE,
    ))
}
