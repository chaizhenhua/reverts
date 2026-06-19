use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::{AstBuilder, ast::Program};
use oxc_codegen::{CodeGenerator, CodegenOptions};
use oxc_parser::Parser;

use crate::errors::{JsError, ParseError, ParseGoal, Result};
use crate::generated_statements::{
    empty_export_statement, generated_export_statement, generated_import_statement,
};
use crate::import_coalesce::{
    coalesce_imports_in_program, prune_unused_import_specifiers_in_program,
};
use crate::local_named_exports::coalesce_simple_local_named_exports_in_program;
use crate::lowering::{
    BABEL_INTEROP_HELPERS, CompilerLowering, ESBUILD_RUNTIME_HELPERS, WEBPACK_RUNTIME_HELPERS,
    apply_source_level_lowerings, is_babel_es_module_marker, is_babel_interop_helper_definition,
    program_references_named_identifier, strip_named_declarations_in_program,
    strip_named_var_declarations_in_program, strip_webpack_make_namespace_markers_in_program,
};
use crate::namespace_flatten::flatten_node_builtin_namespace_imports_in_program;
use crate::namespace_split::{merge_and_sort_named_imports, split_safe_namespace_imports};
use crate::parse::{parse_options_for, source_type_candidates};
use crate::recover::{
    apply_object_property_readability, inline_simple_root_aliases, recover_class_declarations,
    recover_function_declarations, recover_object_destructuring,
};
use crate::rename_apply::{
    ReadabilityRenameHint, ReadabilityRenameSource, apply_emit_safety_renames,
    apply_readability_renames, resolve_readability_rename_hints,
};
use crate::rename_hints::collect_late_readability_rename_hints;
use crate::type_annotations::{
    apply_import_member_type_queries_to_program, apply_type_annotations_to_program,
};
use crate::{
    GeneratedExport, GeneratedImport, GeneratedRename, GeneratedTypeAnnotation, ReadabilityReport,
};

#[derive(Debug, Clone, Copy)]
pub struct FormatSourceRequest<'a> {
    pub body_source: &'a str,
    pub generated_imports: &'a [GeneratedImport],
    pub generated_exports: &'a [GeneratedExport],
    pub readability_renames: &'a [GeneratedRename],
    pub type_annotations: &'a [GeneratedTypeAnnotation],
    pub infer_literal_types: bool,
    pub path_hint: Option<&'a Path>,
    pub goal: ParseGoal,
    pub lowering: CompilerLowering,
}

pub fn format_source_with_module_items(
    body_source: &str,
    generated_imports: &[GeneratedImport],
    generated_exports: &[GeneratedExport],
    path_hint: Option<&Path>,
    goal: ParseGoal,
    lowering: CompilerLowering,
) -> Result<String> {
    format_source_with_module_items_and_renames(
        body_source,
        generated_imports,
        generated_exports,
        &[],
        path_hint,
        goal,
        lowering,
    )
}

pub fn format_source_with_module_items_and_renames(
    body_source: &str,
    generated_imports: &[GeneratedImport],
    generated_exports: &[GeneratedExport],
    readability_renames: &[GeneratedRename],
    path_hint: Option<&Path>,
    goal: ParseGoal,
    lowering: CompilerLowering,
) -> Result<String> {
    format_source_with_module_items_request(FormatSourceRequest {
        body_source,
        generated_imports,
        generated_exports,
        readability_renames,
        type_annotations: &[],
        infer_literal_types: false,
        path_hint,
        goal,
        lowering,
    })
}

pub fn format_source_with_module_items_request(request: FormatSourceRequest<'_>) -> Result<String> {
    format_source_with_module_items_request_with_report(request).map(|(source, _)| source)
}

pub fn format_source_with_module_items_and_renames_with_report(
    body_source: &str,
    generated_imports: &[GeneratedImport],
    generated_exports: &[GeneratedExport],
    readability_renames: &[GeneratedRename],
    path_hint: Option<&Path>,
    goal: ParseGoal,
    lowering: CompilerLowering,
) -> Result<(String, ReadabilityReport)> {
    format_source_with_module_items_request_with_report(FormatSourceRequest {
        body_source,
        generated_imports,
        generated_exports,
        readability_renames,
        type_annotations: &[],
        infer_literal_types: false,
        path_hint,
        goal,
        lowering,
    })
}

pub fn format_source_with_module_items_request_with_report(
    request: FormatSourceRequest<'_>,
) -> Result<(String, ReadabilityReport)> {
    let FormatSourceRequest {
        body_source,
        generated_imports,
        generated_exports,
        readability_renames,
        type_annotations,
        infer_literal_types,
        path_hint,
        goal,
        lowering,
    } = request;
    // Source-level pre-rewrites: applied before the main parse/codegen path so
    // that subsequent steps (audit, codegen) see the lowered form. The
    // rewriter parses once, collects span-aware edits, and returns the
    // unchanged source if it cannot parse — in which case the regular parse
    // below will surface a faithful diagnostic.
    let lowered = apply_source_level_lowerings(body_source, path_hint, goal, lowering);
    let body_source = lowered.as_str();

    let mut errors = Vec::new();
    let mut report = ReadabilityReport::default();

    for source_type in source_type_candidates(path_hint, goal) {
        let allocator = Allocator::default();
        let mut parsed = Parser::new(&allocator, body_source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }

        if matches!(lowering, CompilerLowering::Babel) {
            parsed
                .program
                .body
                .retain(|statement| !is_babel_es_module_marker(statement));
            for helper in BABEL_INTEROP_HELPERS {
                if !program_references_named_identifier(&parsed.program, helper.name) {
                    parsed
                        .program
                        .body
                        .retain(|statement| !is_babel_interop_helper_definition(statement, helper));
                }
            }
        }
        if matches!(lowering, CompilerLowering::Esbuild) {
            let mut unreferenced = Vec::new();
            for helper_name in ESBUILD_RUNTIME_HELPERS {
                if !program_references_named_identifier(&parsed.program, helper_name) {
                    unreferenced.push(*helper_name);
                }
            }
            strip_named_var_declarations_in_program(&mut parsed.program, &unreferenced);
        }
        if matches!(lowering, CompilerLowering::Webpack) {
            strip_webpack_make_namespace_markers_in_program(&mut parsed.program);
            let mut unreferenced = Vec::new();
            for helper_name in WEBPACK_RUNTIME_HELPERS {
                if !program_references_named_identifier(&parsed.program, helper_name) {
                    unreferenced.push(*helper_name);
                }
            }
            strip_named_declarations_in_program(&mut parsed.program, &unreferenced);
        }

        let builder = AstBuilder::new(&allocator);
        for generated_import in generated_imports.iter().rev() {
            parsed
                .program
                .body
                .insert(0, generated_import_statement(&builder, generated_import));
        }
        for generated_export in generated_exports {
            parsed
                .program
                .body
                .push(generated_export_statement(&builder, generated_export));
        }
        apply_type_annotations_to_program(
            &allocator,
            &mut parsed.program,
            type_annotations,
            infer_literal_types,
        );
        let mut readability_hints = collect_late_readability_rename_hints(&parsed.program);
        readability_hints.extend(readability_renames.iter().map(|rename| {
            ReadabilityRenameHint::new(
                rename.original.as_str(),
                rename.renamed.as_str(),
                ReadabilityRenameSource::ExplicitSemantic,
            )
        }));
        let readability_renames_with_imports =
            resolve_readability_rename_hints(readability_hints, &mut report);
        apply_readability_renames(
            &allocator,
            &mut parsed.program,
            &readability_renames_with_imports,
            &mut report,
        );
        apply_emit_safety_renames(&allocator, &mut parsed.program, &mut report);
        apply_emit_readability_polish(&allocator, &mut parsed.program, &mut report);
        normalize_imports_after_emit(&mut parsed.program, &builder);
        if parsed.program.body.is_empty() {
            parsed.program.body.push(empty_export_statement(&builder));
        }
        coalesce_simple_local_named_exports_in_program(&mut parsed.program, &builder);
        if infer_literal_types {
            apply_import_member_type_queries_to_program(&allocator, &mut parsed.program);
        }

        let output = CodeGenerator::new()
            .with_options(CodegenOptions {
                single_quote: true,
                minify: false,
                ..Default::default()
            })
            .build(&parsed.program);
        return Ok((output.code, report));
    }

    Err(JsError::ParseFailed(errors))
}

fn apply_emit_readability_polish<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    report: &mut ReadabilityReport,
) {
    recover_function_declarations(allocator, program, report);
    recover_class_declarations(allocator, program, report);
    inline_simple_root_aliases(allocator, program, report);
    recover_object_destructuring(allocator, program, report);
    apply_object_property_readability(program, report);
    split_safe_namespace_imports(allocator, program, report);
    merge_and_sort_named_imports(allocator, program, report);
}

fn normalize_imports_after_emit<'a>(program: &mut Program<'a>, builder: &AstBuilder<'a>) {
    // These passes intentionally run in phases rather than as one monolithic
    // import rewriter:
    //
    // 1. merge the import surface created by source + generated imports;
    // 2. flatten safe Node builtin namespace member reads, which may synthesize
    //    new named imports;
    // 3. merge again so those synthesized imports join existing imports;
    // 4. prune unused specifiers after readability renames/flattening;
    // 5. merge once more because pruning can convert mixed imports into a
    //    shape that is mergeable with a sibling import.
    coalesce_imports_in_program(program, builder);
    flatten_node_builtin_namespace_imports_in_program(program, builder);
    coalesce_imports_in_program(program, builder);
    prune_unused_import_specifiers_in_program(program, builder);
    coalesce_imports_in_program(program, builder);
}
