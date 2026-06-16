mod classify;
mod errors;
mod facts;
mod format;
mod identifier;
mod import_coalesce;
mod lazy;
mod lowering;
mod namespace_flatten;
mod namespace_split;
pub mod normalize;
mod parse;

pub use classify::{
    DeclarationCallability, ImportUsageScope, classify_import_usage_scope,
    classify_top_level_bindings, verify_only_immediate_call_references,
};
pub use format::{format_source_minified, format_source_pretty, normalize_source_for_pipeline};
pub use lowering::CompilerLowering;

pub use errors::{JsError, ParseError, ParseGoal, Result, parse_error_message};
pub use facts::{
    IdentifierReadFact, LazyValueSubSnippet, PathBuilderCallFact, SourceLocationRewriteFact,
    StaticTemplateLiteralFact, StringLiteralFact, TopLevelStatementFact, TopLevelStatementKind,
    collect_file_url_source_location_rewrites, collect_identifier_read_facts,
    collect_path_builder_calls, collect_static_resource_specifiers,
    collect_static_template_literals, collect_string_literals, collect_top_level_statement_facts,
    lazy_value_sub_snippets,
};
pub use identifier::{
    is_ascii_identifier_continue, is_ascii_identifier_start, is_identifier_part,
    is_identifier_start, is_js_keyword, sanitize_identifier, skip_block_comment, skip_line_comment,
};
use import_coalesce::{coalesce_imports_in_program, prune_unused_import_specifiers_in_program};
pub use lazy::{
    LazyBodyClassification, classify_lazy_module_body, extract_lazy_module_eager_value,
    extract_lazy_module_eager_value_with_safe_deps,
};
use lowering::{
    BABEL_INTEROP_HELPERS, ESBUILD_RUNTIME_HELPERS, WEBPACK_RUNTIME_HELPERS,
    apply_source_level_lowerings, is_babel_es_module_marker, is_babel_interop_helper_definition,
    program_references_named_identifier, strip_named_declarations_in_program,
    strip_named_var_declarations_in_program, strip_webpack_make_namespace_markers_in_program,
};
use namespace_flatten::flatten_node_builtin_namespace_imports_in_program;
use namespace_split::{merge_and_sort_named_imports, split_safe_namespace_imports};
pub use parse::{parse_options_for, parse_source, source_type_candidates, source_type_for_parse};

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::{
    AstBuilder, NONE, Visit, VisitMut,
    ast::{
        Argument, AssignmentExpression, BindingIdentifier, BindingPatternKind, CallExpression,
        Class, ClassType, ExportNamedDeclaration, Expression, Function, FunctionType,
        IdentifierReference, ImportDeclarationSpecifier, ImportOrExportKind, ModuleExportName,
        ObjectExpression, ObjectProperty, ObjectPropertyKind, Program, PropertyKey, PropertyKind,
        Statement, VariableDeclarationKind,
    },
    visit::walk::{
        walk_assignment_expression, walk_call_expression, walk_export_named_declaration,
        walk_object_property,
    },
    visit::walk_mut::walk_object_property as walk_object_property_mut,
};
use oxc_codegen::{CodeGenerator, CodegenOptions};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::{GetSpan, SPAN};
use oxc_syntax::{reference::ReferenceId, symbol::SymbolId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedImport {
    pub namespace: String,
    pub specifier: String,
    pub attributes: Vec<(String, String)>,
}

impl GeneratedImport {
    #[must_use]
    pub fn new(namespace: impl Into<String>, specifier: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            specifier: specifier.into(),
            attributes: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_attribute(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.attributes.push((key.into(), value.into()));
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedExport {
    pub binding: String,
}

impl GeneratedExport {
    #[must_use]
    pub fn new(binding: impl Into<String>) -> Self {
        Self {
            binding: binding.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedRename {
    pub original: String,
    pub renamed: String,
}

impl GeneratedRename {
    #[must_use]
    pub fn new(original: impl Into<String>, renamed: impl Into<String>) -> Self {
        Self {
            original: original.into(),
            renamed: renamed.into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadabilityReport {
    pub entries: Vec<String>,
}

impl ReadabilityReport {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn push(&mut self, entry: impl Into<String>) {
        self.entries.push(entry.into());
    }
}

pub(crate) fn expression_identifier<'a>(expression: &'a Expression<'a>) -> Option<&'a str> {
    match expression {
        Expression::Identifier(identifier) => Some(identifier.name.as_str()),
        _ => None,
    }
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
    format_source_with_module_items_and_renames_with_report(
        body_source,
        generated_imports,
        generated_exports,
        readability_renames,
        path_hint,
        goal,
        lowering,
    )
    .map(|(source, _)| source)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ReadabilityRenameSource {
    UsagePattern,
    ObjectProperty,
    PackageNamespace,
    ImportExportPublic,
    CommonJsExport,
    ExplicitSemantic,
}

impl ReadabilityRenameSource {
    fn confidence(self) -> u8 {
        match self {
            Self::ExplicitSemantic => 100,
            Self::ImportExportPublic | Self::CommonJsExport => 90,
            Self::PackageNamespace => 80,
            Self::ObjectProperty => 50,
            Self::UsagePattern => 40,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitSemantic => "explicit_semantic",
            Self::ImportExportPublic => "import_export_public",
            Self::CommonJsExport => "commonjs_export",
            Self::PackageNamespace => "package_namespace",
            Self::ObjectProperty => "object_property",
            Self::UsagePattern => "usage_pattern",
        }
    }
}

#[derive(Debug, Clone)]
struct ReadabilityRenameHint {
    original: String,
    renamed: String,
    source: ReadabilityRenameSource,
}

impl ReadabilityRenameHint {
    fn new(original: &str, renamed: &str, source: ReadabilityRenameSource) -> Self {
        Self {
            original: original.trim().to_string(),
            renamed: renamed.trim().to_string(),
            source,
        }
    }
}

#[derive(Debug, Clone)]
struct ResolvedReadabilityRename {
    original: String,
    renamed: String,
    source: ReadabilityRenameSource,
}

fn resolve_readability_rename_hints(
    hints: Vec<ReadabilityRenameHint>,
    report: &mut ReadabilityReport,
) -> Vec<ResolvedReadabilityRename> {
    let mut hints_by_original = BTreeMap::<String, Vec<ReadabilityRenameHint>>::new();
    for hint in hints {
        if hint.original.is_empty() || hint.renamed.is_empty() || hint.original == hint.renamed {
            continue;
        }
        if sanitize_identifier(hint.renamed.as_str()) != hint.renamed {
            report.push(format!(
                "skipped rename {} -> {}, source={}, reason=invalid_target",
                hint.original,
                hint.renamed,
                hint.source.as_str()
            ));
            continue;
        }
        hints_by_original
            .entry(hint.original.clone())
            .or_default()
            .push(hint);
    }

    let mut resolved = Vec::new();
    for (original, hints) in hints_by_original {
        let max_confidence = hints
            .iter()
            .map(|hint| hint.source.confidence())
            .max()
            .unwrap_or(0);
        let top_hints = hints
            .iter()
            .filter(|hint| hint.source.confidence() == max_confidence)
            .collect::<Vec<_>>();
        let top_names = top_hints
            .iter()
            .map(|hint| hint.renamed.as_str())
            .collect::<BTreeSet<_>>();
        if top_names.len() != 1 {
            let candidates = top_hints
                .iter()
                .map(|hint| format!("{}:{}", hint.source.as_str(), hint.renamed))
                .collect::<Vec<_>>()
                .join(", ");
            report.push(format!(
                "skipped rename {}, reason=conflicting_hints, candidates={candidates}",
                original
            ));
            continue;
        }
        let chosen = top_hints[0];
        resolved.push(ResolvedReadabilityRename {
            original,
            renamed: chosen.renamed.clone(),
            source: chosen.source,
        });
    }
    resolved
}

fn apply_readability_renames<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    readability_renames: &[ResolvedReadabilityRename],
    report: &mut ReadabilityReport,
) {
    let requested = readability_renames
        .iter()
        .map(|rename| (rename.original.clone(), rename.clone()))
        .collect::<BTreeMap<_, _>>();
    if requested.is_empty() {
        return;
    }

    let (symbol_renames, reference_renames) = {
        let semantic = SemanticBuilder::new().build(program).semantic;
        let symbols = semantic.symbols();
        let root_scope_id = semantic.scopes().root_scope_id();
        let unresolved_root_names = semantic
            .scopes()
            .root_unresolved_references()
            .keys()
            .map(|name| name.as_str().to_string())
            .collect::<BTreeSet<_>>();
        let root_symbols = symbols
            .symbol_ids()
            .filter(|symbol_id| symbols.get_scope_id(*symbol_id) == root_scope_id)
            .collect::<Vec<_>>();

        let mut symbol_renames = BTreeMap::<SymbolId, String>::new();
        for (original, rename) in &requested {
            let renamed = rename.renamed.as_str();
            // If the desired name is already used as a free global reference,
            // introducing a module-scope binding with that name would change
            // resolution for nested reads. Leave the original name intact.
            if unresolved_root_names.contains(renamed) {
                report.push(format!(
                    "skipped rename {original} -> {renamed}, source={}, reason=would_capture_global",
                    rename.source.as_str()
                ));
                continue;
            }
            let targets = root_symbols
                .iter()
                .copied()
                .filter(|symbol_id| symbols.get_name(*symbol_id) == original)
                .collect::<Vec<_>>();
            if targets.len() != 1 {
                report.push(format!(
                    "skipped rename {original} -> {renamed}, source={}, reason=missing_or_ambiguous_original",
                    rename.source.as_str()
                ));
                continue;
            }
            let target = targets[0];
            let collides = root_symbols
                .iter()
                .copied()
                .any(|symbol_id| symbol_id != target && symbols.get_name(symbol_id) == renamed);
            if collides {
                report.push(format!(
                    "skipped rename {original} -> {renamed}, source={}, reason=name_collision",
                    rename.source.as_str()
                ));
                continue;
            }
            if symbol_renames.values().any(|value| value == renamed) {
                report.push(format!(
                    "skipped rename {original} -> {renamed}, source={}, reason=duplicate_target",
                    rename.source.as_str()
                ));
                continue;
            }
            symbol_renames.insert(target, renamed.to_string());
            report.push(format!(
                "renamed {original} -> {renamed}, source={}",
                rename.source.as_str()
            ));
        }

        let mut reference_renames = BTreeMap::<ReferenceId, String>::new();
        for (symbol_id, renamed) in &symbol_renames {
            for reference_id in symbols.get_resolved_reference_ids(*symbol_id) {
                reference_renames.insert(*reference_id, renamed.clone());
            }
        }

        (symbol_renames, reference_renames)
    };

    if symbol_renames.is_empty() && reference_renames.is_empty() {
        return;
    }

    let mut renamer = ReadabilityRenamer {
        builder: AstBuilder::new(allocator),
        symbol_renames,
        reference_renames,
    };
    renamer.visit_program(program);
}

fn apply_emit_safety_renames<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    report: &mut ReadabilityReport,
) {
    let (symbol_renames, reference_renames) = {
        let semantic = SemanticBuilder::new().build(program).semantic;
        let symbols = semantic.symbols();
        let mut used_names = symbols
            .symbol_ids()
            .map(|symbol_id| symbols.get_name(symbol_id).to_string())
            .collect::<BTreeSet<_>>();
        used_names.extend(
            semantic
                .scopes()
                .root_unresolved_references()
                .keys()
                .map(|name| name.as_str().to_string()),
        );

        let mut symbol_renames = BTreeMap::<SymbolId, String>::new();
        let mut reference_renames = BTreeMap::<ReferenceId, String>::new();
        for symbol_id in symbols.symbol_ids() {
            let original = symbols.get_name(symbol_id);
            let sanitized = sanitize_identifier(original);
            if sanitized == original {
                continue;
            }
            let renamed = unique_safe_identifier(&sanitized, &mut used_names);
            symbol_renames.insert(symbol_id, renamed.clone());
            for reference_id in symbols.get_resolved_reference_ids(symbol_id) {
                reference_renames.insert(*reference_id, renamed.clone());
            }
            report.push(format!(
                "renamed {original} -> {renamed}, source=emit_safety"
            ));
        }

        (symbol_renames, reference_renames)
    };

    if symbol_renames.is_empty() && reference_renames.is_empty() {
        return;
    }

    let mut renamer = ReadabilityRenamer {
        builder: AstBuilder::new(allocator),
        symbol_renames,
        reference_renames,
    };
    renamer.visit_program(program);
}

fn unique_safe_identifier(base: &str, used_names: &mut BTreeSet<String>) -> String {
    if !used_names.contains(base) {
        used_names.insert(base.to_string());
        return base.to_string();
    }
    for suffix in 2.. {
        let candidate = format!("{base}{suffix}");
        if !used_names.contains(&candidate) {
            used_names.insert(candidate.clone());
            return candidate;
        }
    }
    unreachable!("unbounded suffix search should always find an identifier")
}

fn collect_late_readability_rename_hints(program: &Program<'_>) -> Vec<ReadabilityRenameHint> {
    let mut collector = LateReadabilityRenameCollector { hints: Vec::new() };
    collector.visit_program(program);
    collector.hints
}

struct LateReadabilityRenameCollector {
    hints: Vec<ReadabilityRenameHint>,
}

impl LateReadabilityRenameCollector {
    fn push_hint(&mut self, original: &str, renamed: &str, source: ReadabilityRenameSource) {
        self.hints
            .push(ReadabilityRenameHint::new(original, renamed, source));
    }
}

impl<'a> Visit<'a> for LateReadabilityRenameCollector {
    fn visit_program(&mut self, program: &Program<'a>) {
        collect_import_alias_readability_renames(program, self);
        collect_usage_readability_rename_hints(program, self);
        oxc_ast::visit::walk::walk_program(self, program);
    }

    fn visit_export_named_declaration(&mut self, declaration: &ExportNamedDeclaration<'a>) {
        if declaration.source.is_none() {
            for specifier in &declaration.specifiers {
                let Some(local) = module_export_identifier_name(&specifier.local) else {
                    continue;
                };
                let Some(exported) = module_export_identifier_name(&specifier.exported) else {
                    continue;
                };
                if exported == "default" {
                    continue;
                }
                self.push_hint(
                    local.as_str(),
                    exported.as_str(),
                    ReadabilityRenameSource::ImportExportPublic,
                );
            }
        }
        walk_export_named_declaration(self, declaration);
    }

    fn visit_assignment_expression(&mut self, expression: &AssignmentExpression<'a>) {
        if expression.operator.is_assign() {
            if let Some(exported) = commonjs_export_property_name(&expression.left)
                && let Expression::Identifier(identifier) = &expression.right
            {
                self.push_hint(
                    identifier.name.as_str(),
                    exported.as_str(),
                    ReadabilityRenameSource::CommonJsExport,
                );
            }
            if commonjs_module_exports_target(&expression.left)
                && let Expression::ObjectExpression(object) = &expression.right
            {
                collect_object_export_readability_renames(
                    self,
                    object,
                    ReadabilityRenameSource::CommonJsExport,
                );
            }
        }
        walk_assignment_expression(self, expression);
    }

    fn visit_call_expression(&mut self, expression: &CallExpression<'a>) {
        if let Some((exported, local)) = object_define_property_export_getter(expression) {
            self.push_hint(
                local.as_str(),
                exported.as_str(),
                ReadabilityRenameSource::CommonJsExport,
            );
        }
        walk_call_expression(self, expression);
    }

    fn visit_object_property(&mut self, property: &ObjectProperty<'a>) {
        if !property.computed
            && !property.method
            && !property.shorthand
            && let Some(property_name) = property_key_readability_name(&property.key)
            && let Expression::Identifier(identifier) = &property.value
        {
            self.push_hint(
                identifier.name.as_str(),
                property_name.as_str(),
                ReadabilityRenameSource::ObjectProperty,
            );
        }
        walk_object_property(self, property);
    }
}

fn collect_object_export_readability_renames(
    collector: &mut LateReadabilityRenameCollector,
    object: &ObjectExpression<'_>,
    source: ReadabilityRenameSource,
) {
    for property in &object.properties {
        let ObjectPropertyKind::ObjectProperty(property) = property else {
            continue;
        };
        if !property.computed
            && !property.method
            && !property.shorthand
            && let Some(property_name) = property_key_readability_name(&property.key)
            && let Expression::Identifier(identifier) = &property.value
        {
            collector.push_hint(identifier.name.as_str(), property_name.as_str(), source);
        }
    }
}

fn collect_import_alias_readability_renames(
    program: &Program<'_>,
    collector: &mut LateReadabilityRenameCollector,
) {
    for statement in &program.body {
        let Statement::ImportDeclaration(declaration) = statement else {
            continue;
        };
        let Some(specifiers) = &declaration.specifiers else {
            continue;
        };
        for specifier in specifiers {
            match specifier {
                ImportDeclarationSpecifier::ImportSpecifier(specifier) => {
                    let Some(imported) = module_export_identifier_name(&specifier.imported) else {
                        continue;
                    };
                    if sanitize_identifier(imported.as_str()) != imported {
                        continue;
                    }
                    let local = specifier.local.name.as_str();
                    collector.push_hint(
                        local,
                        imported.as_str(),
                        ReadabilityRenameSource::ImportExportPublic,
                    );
                }
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(specifier) => {
                    let local = specifier.local.name.as_str();
                    if !is_generated_package_namespace_alias(local) {
                        continue;
                    }
                    let Some(namespace) =
                        readable_namespace_name_for_import(declaration.source.value.as_str())
                    else {
                        continue;
                    };
                    collector.push_hint(
                        local,
                        namespace.as_str(),
                        ReadabilityRenameSource::PackageNamespace,
                    );
                }
                ImportDeclarationSpecifier::ImportDefaultSpecifier(_) => {}
            }
        }
    }
}

fn collect_usage_readability_rename_hints(
    program: &Program<'_>,
    collector: &mut LateReadabilityRenameCollector,
) {
    for statement in &program.body {
        let Statement::VariableDeclaration(declaration) = statement else {
            continue;
        };
        if declaration.declare || declaration.declarations.len() != 1 {
            continue;
        }
        let declarator = &declaration.declarations[0];
        if declarator.definite || declarator.id.type_annotation.is_some() || declarator.id.optional
        {
            continue;
        }
        let BindingPatternKind::BindingIdentifier(binding) = &declarator.id.kind else {
            continue;
        };
        let local = binding.name.as_str();
        if !looks_generated_binding_name(local) {
            continue;
        }
        let Some(init) = declarator.init.as_ref() else {
            continue;
        };
        let Some(candidate) = usage_based_name_for_initializer(init) else {
            continue;
        };
        collector.push_hint(
            local,
            candidate.as_str(),
            ReadabilityRenameSource::UsagePattern,
        );
    }
}

fn looks_generated_binding_name(name: &str) -> bool {
    let name = name.trim_start_matches('_');
    name.starts_with('$') || name.chars().count() == 1 || looks_like_letter_number_binding(name)
}

fn looks_like_letter_number_binding(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && !chars.as_str().is_empty()
        && chars.as_str().chars().all(|ch| ch.is_ascii_digit())
}

fn usage_based_name_for_initializer(expression: &Expression<'_>) -> Option<String> {
    match expression {
        Expression::NewExpression(expression) => usage_name_from_constructor(&expression.callee),
        Expression::CallExpression(expression) => usage_name_from_call_callee(&expression.callee),
        _ => None,
    }
}

fn usage_name_from_constructor(callee: &Expression<'_>) -> Option<String> {
    match callee {
        Expression::Identifier(identifier) => lower_type_name(identifier.name.as_str()),
        Expression::StaticMemberExpression(member) => {
            lower_type_name(member.property.name.as_str())
        }
        _ => None,
    }
}

fn usage_name_from_call_callee(callee: &Expression<'_>) -> Option<String> {
    let name = match callee {
        Expression::Identifier(identifier) => identifier.name.as_str(),
        Expression::StaticMemberExpression(member) => member.property.name.as_str(),
        _ => return None,
    };
    let suffix = strip_factory_prefix(name)?;
    lower_type_name(suffix)
}

fn strip_factory_prefix(name: &str) -> Option<&str> {
    for prefix in [
        "create", "make", "build", "get", "load", "init", "use", "open", "read", "parse",
    ] {
        let Some(suffix) = name.strip_prefix(prefix) else {
            continue;
        };
        if suffix
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_uppercase())
        {
            return Some(suffix);
        }
    }
    None
}

fn lower_type_name(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let lowered = if name.chars().all(|ch| ch.is_ascii_uppercase()) {
        name.to_ascii_lowercase()
    } else {
        let mut chars = name.chars();
        let first = chars.next()?;
        let mut lowered = String::new();
        lowered.extend(first.to_lowercase());
        lowered.push_str(chars.as_str());
        lowered
    };
    let sanitized = sanitize_identifier(lowered.as_str());
    if sanitized == lowered {
        Some(lowered)
    } else {
        None
    }
}

fn module_export_identifier_name(name: &ModuleExportName<'_>) -> Option<String> {
    match name {
        ModuleExportName::IdentifierName(identifier) => Some(identifier.name.as_str().to_string()),
        ModuleExportName::IdentifierReference(identifier) => {
            Some(identifier.name.as_str().to_string())
        }
        ModuleExportName::StringLiteral(literal) => Some(literal.value.as_str().to_string()),
    }
}

fn property_key_readability_name(key: &PropertyKey<'_>) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(identifier) => Some(identifier.name.as_str().to_string()),
        PropertyKey::StringLiteral(literal) => Some(literal.value.as_str().to_string()),
        _ => None,
    }
}

fn commonjs_export_property_name(target: &oxc_ast::ast::AssignmentTarget<'_>) -> Option<String> {
    match target {
        oxc_ast::ast::AssignmentTarget::StaticMemberExpression(member) => {
            if expression_is_commonjs_exports_object(&member.object) {
                return Some(member.property.name.as_str().to_string());
            }
        }
        oxc_ast::ast::AssignmentTarget::ComputedMemberExpression(member) => {
            if expression_is_commonjs_exports_object(&member.object)
                && let Expression::StringLiteral(property) = &member.expression
            {
                return Some(property.value.as_str().to_string());
            }
        }
        _ => {}
    }
    None
}

fn commonjs_module_exports_target(target: &oxc_ast::ast::AssignmentTarget<'_>) -> bool {
    let oxc_ast::ast::AssignmentTarget::StaticMemberExpression(member) = target else {
        return false;
    };
    expression_identifier(&member.object) == Some("module") && member.property.name == "exports"
}

fn expression_is_commonjs_exports_object(expression: &Expression<'_>) -> bool {
    if expression_identifier(expression) == Some("exports") {
        return true;
    }
    let Expression::StaticMemberExpression(member) = expression else {
        return false;
    };
    expression_identifier(&member.object) == Some("module") && member.property.name == "exports"
}

fn object_define_property_export_getter(call: &CallExpression<'_>) -> Option<(String, String)> {
    let Expression::StaticMemberExpression(callee) = &call.callee else {
        return None;
    };
    if expression_identifier(&callee.object) != Some("Object")
        || callee.property.name != "defineProperty"
        || call.arguments.len() < 3
    {
        return None;
    }
    if !argument_is_commonjs_exports_object(&call.arguments[0]) {
        return None;
    }
    let exported = argument_string_literal(&call.arguments[1])?;
    let descriptor = argument_object_expression(&call.arguments[2])?;
    let local = descriptor_getter_return_identifier(descriptor)?;
    Some((exported, local))
}

fn argument_is_commonjs_exports_object(argument: &Argument<'_>) -> bool {
    match argument {
        Argument::Identifier(identifier) => identifier.name == "exports",
        Argument::StaticMemberExpression(member) => {
            expression_identifier(&member.object) == Some("module")
                && member.property.name == "exports"
        }
        _ => false,
    }
}

fn argument_string_literal(argument: &Argument<'_>) -> Option<String> {
    let Argument::StringLiteral(literal) = argument else {
        return None;
    };
    Some(literal.value.as_str().to_string())
}

fn argument_object_expression<'a>(argument: &'a Argument<'a>) -> Option<&'a ObjectExpression<'a>> {
    let Argument::ObjectExpression(object) = argument else {
        return None;
    };
    Some(object)
}

fn descriptor_getter_return_identifier(descriptor: &ObjectExpression<'_>) -> Option<String> {
    for property in &descriptor.properties {
        let ObjectPropertyKind::ObjectProperty(property) = property else {
            continue;
        };
        if property.computed
            || property_key_readability_name(&property.key).as_deref() != Some("get")
        {
            continue;
        }
        if let Some(identifier) = returned_identifier_from_invokable(&property.value) {
            return Some(identifier);
        }
    }
    None
}

fn returned_identifier_from_invokable(expression: &Expression<'_>) -> Option<String> {
    match expression {
        Expression::ArrowFunctionExpression(arrow) => {
            single_returned_identifier_from_body(&arrow.body)
        }
        Expression::FunctionExpression(function) => function
            .body
            .as_deref()
            .and_then(single_returned_identifier_from_body),
        _ => None,
    }
}

fn single_returned_identifier_from_body(body: &oxc_ast::ast::FunctionBody<'_>) -> Option<String> {
    if !body.directives.is_empty() || body.statements.len() != 1 {
        return None;
    }
    let Statement::ReturnStatement(statement) = &body.statements[0] else {
        return None;
    };
    let Some(Expression::Identifier(identifier)) = &statement.argument else {
        return None;
    };
    Some(identifier.name.as_str().to_string())
}

fn is_generated_package_namespace_alias(local: &str) -> bool {
    local == "__pkg" || local.starts_with("__pkg_")
}

fn readable_namespace_name_for_import(specifier: &str) -> Option<String> {
    let specifier = specifier
        .split(['?', '#'])
        .next()
        .unwrap_or(specifier)
        .trim();
    if specifier.is_empty() || specifier.starts_with('.') {
        return None;
    }
    let mut words = Vec::new();
    for segment in specifier.split('/') {
        let segment = segment.trim_start_matches('@');
        let mut word = String::new();
        for character in segment.chars() {
            if character.is_ascii_alphanumeric() {
                word.push(character);
            } else if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        }
        if !word.is_empty() {
            words.push(word);
        }
    }
    if words.is_empty() {
        return None;
    }
    let mut candidate = String::new();
    for (index, word) in words.iter().enumerate() {
        if index == 0 {
            candidate.push_str(word);
            continue;
        }
        let mut chars = word.chars();
        let Some(first) = chars.next() else {
            continue;
        };
        candidate.extend(first.to_uppercase());
        candidate.push_str(chars.as_str());
    }
    let sanitized = sanitize_identifier(candidate.as_str());
    if sanitized == "_" {
        None
    } else {
        Some(sanitized)
    }
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

#[derive(Debug, Clone)]
struct AliasCandidate {
    statement_index: usize,
    declaration_start: u32,
    alias_name: String,
    alias_symbol: SymbolId,
    source_symbol: SymbolId,
    source_name: String,
}

fn inline_simple_root_aliases<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    report: &mut ReadabilityReport,
) {
    // Alias inlining is intentionally iterative. For chains like
    // `const a = source; const b = a; use(b);`, the first pass removes `a`
    // and rewrites `b`'s initializer, while the second pass can remove `b`.
    for _ in 0..8 {
        let (reference_rewrites, removable_statement_indices, inlined_aliases) = {
            let semantic = SemanticBuilder::new().build(program).semantic;
            let symbols = semantic.symbols();
            let root_scope_id = semantic.scopes().root_scope_id();
            let exported_locals = collect_exported_local_names(program);
            let mut candidates = collect_alias_candidates(program, symbols);
            candidates.retain(|candidate| {
                symbols.get_scope_id(candidate.alias_symbol) == root_scope_id
                    && symbols.get_scope_id(candidate.source_symbol) == root_scope_id
                    && symbols.get_span(candidate.source_symbol).start < candidate.declaration_start
                    && !exported_locals.contains(symbols.get_name(candidate.alias_symbol))
                    && !symbols.symbol_is_mutated(candidate.alias_symbol)
                    && !name_is_shadowed(symbols, root_scope_id, &candidate.source_name)
                    && references_are_safe_to_inline(
                        &semantic,
                        candidate.alias_symbol,
                        candidate.declaration_start,
                    )
            });
            let candidate_alias_symbols = candidates
                .iter()
                .map(|candidate| candidate.alias_symbol)
                .collect::<BTreeSet<_>>();
            candidates
                .retain(|candidate| !candidate_alias_symbols.contains(&candidate.source_symbol));

            let mut reference_rewrites = BTreeMap::<ReferenceId, String>::new();
            let mut removable_statement_indices = BTreeSet::<usize>::new();
            let mut inlined_aliases = Vec::<(String, String)>::new();
            for candidate in &candidates {
                for reference_id in symbols.get_resolved_reference_ids(candidate.alias_symbol) {
                    reference_rewrites.insert(*reference_id, candidate.source_name.clone());
                }
                removable_statement_indices.insert(candidate.statement_index);
                inlined_aliases.push((candidate.alias_name.clone(), candidate.source_name.clone()));
            }
            (
                reference_rewrites,
                removable_statement_indices,
                inlined_aliases,
            )
        };

        if reference_rewrites.is_empty() {
            return;
        }

        let mut inliner = AliasReferenceInliner {
            builder: AstBuilder::new(allocator),
            reference_rewrites,
        };
        inliner.visit_program(program);
        let mut index = 0usize;
        program.body.retain(|_| {
            let keep = !removable_statement_indices.contains(&index);
            index += 1;
            keep
        });
        for (alias, source) in inlined_aliases {
            report.push(format!("inlined alias {alias} -> {source}"));
        }
    }
}

fn collect_exported_local_names(program: &Program<'_>) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for statement in &program.body {
        let Statement::ExportNamedDeclaration(declaration) = statement else {
            continue;
        };
        if declaration.source.is_some() {
            continue;
        }
        for specifier in &declaration.specifiers {
            if let Some(local) = module_export_identifier_name(&specifier.local) {
                names.insert(local);
            }
        }
    }
    let mut cjs_collector = CommonJsExportedLocalCollector {
        names: BTreeSet::new(),
    };
    cjs_collector.visit_program(program);
    names.extend(cjs_collector.names);
    names
}

struct CommonJsExportedLocalCollector {
    names: BTreeSet<String>,
}

impl<'a> Visit<'a> for CommonJsExportedLocalCollector {
    fn visit_assignment_expression(&mut self, expression: &AssignmentExpression<'a>) {
        if expression.operator.is_assign() {
            if commonjs_export_property_name(&expression.left).is_some()
                && let Expression::Identifier(identifier) = &expression.right
            {
                self.names.insert(identifier.name.as_str().to_string());
            }
            if commonjs_module_exports_target(&expression.left)
                && let Expression::ObjectExpression(object) = &expression.right
            {
                for property in &object.properties {
                    let ObjectPropertyKind::ObjectProperty(property) = property else {
                        continue;
                    };
                    if property.computed || property.method || property.shorthand {
                        continue;
                    }
                    if let Expression::Identifier(identifier) = &property.value {
                        self.names.insert(identifier.name.as_str().to_string());
                    }
                }
            }
        }
        walk_assignment_expression(self, expression);
    }

    fn visit_call_expression(&mut self, expression: &CallExpression<'a>) {
        if let Some((_, local)) = object_define_property_export_getter(expression) {
            self.names.insert(local);
        }
        walk_call_expression(self, expression);
    }
}

fn collect_alias_candidates(
    program: &Program<'_>,
    symbols: &oxc_semantic::SymbolTable,
) -> Vec<AliasCandidate> {
    program
        .body
        .iter()
        .enumerate()
        .filter_map(|(statement_index, statement)| {
            let Statement::VariableDeclaration(declaration) = statement else {
                return None;
            };
            if declaration.kind != VariableDeclarationKind::Const
                || declaration.declare
                || declaration.declarations.len() != 1
            {
                return None;
            }
            let declarator = &declaration.declarations[0];
            if declarator.definite
                || declarator.id.type_annotation.is_some()
                || declarator.id.optional
            {
                return None;
            }
            let BindingPatternKind::BindingIdentifier(alias) = &declarator.id.kind else {
                return None;
            };
            let alias_symbol = alias.symbol_id.get()?;
            let Expression::Identifier(source) = declarator.init.as_ref()? else {
                return None;
            };
            if alias.name == source.name {
                return None;
            }
            let source_reference_id = source.reference_id.get()?;
            let source_symbol = symbols.get_reference(source_reference_id).symbol_id()?;
            Some(AliasCandidate {
                statement_index,
                declaration_start: statement.span().start,
                alias_name: alias.name.as_str().to_string(),
                alias_symbol,
                source_symbol,
                source_name: source.name.as_str().to_string(),
            })
        })
        .collect()
}

fn name_is_shadowed(
    symbols: &oxc_semantic::SymbolTable,
    root_scope_id: oxc_syntax::scope::ScopeId,
    name: &str,
) -> bool {
    symbols.symbol_ids().any(|symbol_id| {
        symbols.get_scope_id(symbol_id) != root_scope_id && symbols.get_name(symbol_id) == name
    })
}

fn references_are_safe_to_inline(
    semantic: &oxc_semantic::Semantic<'_>,
    alias_symbol: SymbolId,
    declaration_start: u32,
) -> bool {
    let mut saw_reference = false;
    for reference in semantic.symbols().get_resolved_references(alias_symbol) {
        saw_reference = true;
        if !reference.is_read()
            || reference.is_write()
            || !reference.is_value()
            || semantic.reference_span(reference).start <= declaration_start
        {
            return false;
        }
    }
    saw_reference
}

struct AliasReferenceInliner<'a> {
    builder: AstBuilder<'a>,
    reference_rewrites: BTreeMap<ReferenceId, String>,
}

impl<'a> VisitMut<'a> for AliasReferenceInliner<'a> {
    fn visit_identifier_reference(&mut self, identifier: &mut IdentifierReference<'a>) {
        let Some(reference_id) = identifier.reference_id.get() else {
            return;
        };
        let Some(replacement) = self.reference_rewrites.get(&reference_id) else {
            return;
        };
        identifier.name = self.builder.atom(replacement);
    }
}

#[derive(Debug, Clone)]
struct FunctionDeclarationCandidate {
    statement_index: usize,
    declaration_start: u32,
    binding_name: String,
}

fn recover_function_declarations<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    report: &mut ReadabilityReport,
) {
    let candidates = {
        let semantic = SemanticBuilder::new().build(program).semantic;
        let symbols = semantic.symbols();
        let root_scope_id = semantic.scopes().root_scope_id();
        collect_function_declaration_candidates(program)
            .into_iter()
            .filter(|candidate| {
                symbols
                    .symbol_ids()
                    .find(|symbol_id| {
                        symbols.get_scope_id(*symbol_id) == root_scope_id
                            && symbols.get_name(*symbol_id) == candidate.binding_name
                    })
                    .is_some_and(|symbol_id| {
                        symbols.get_resolved_references(symbol_id).all(|reference| {
                            semantic.reference_span(reference).start > candidate.declaration_start
                        })
                    })
            })
            .collect::<Vec<_>>()
    };
    if candidates.is_empty() {
        return;
    }
    let names_by_statement = candidates
        .into_iter()
        .map(|candidate| (candidate.statement_index, candidate.binding_name))
        .collect::<BTreeMap<_, _>>();
    let builder = AstBuilder::new(allocator);
    for (statement_index, statement) in program.body.iter_mut().enumerate() {
        let Some(binding_name) = names_by_statement.get(&statement_index).cloned() else {
            continue;
        };
        let replacement = function_declaration_replacement(&builder, statement, binding_name);
        if let Some(function) = replacement {
            *statement = Statement::FunctionDeclaration(function);
            if let Some(binding_name) = names_by_statement.get(&statement_index) {
                report.push(format!("recovered function declaration {binding_name}"));
            }
        }
    }
}

fn collect_function_declaration_candidates(
    program: &Program<'_>,
) -> Vec<FunctionDeclarationCandidate> {
    program
        .body
        .iter()
        .enumerate()
        .filter_map(|(statement_index, statement)| {
            let Statement::VariableDeclaration(declaration) = statement else {
                return None;
            };
            if declaration.kind != VariableDeclarationKind::Const
                || declaration.declare
                || declaration.declarations.len() != 1
            {
                return None;
            }
            let declarator = &declaration.declarations[0];
            if declarator.definite
                || declarator.id.type_annotation.is_some()
                || declarator.id.optional
            {
                return None;
            }
            let BindingPatternKind::BindingIdentifier(binding) = &declarator.id.kind else {
                return None;
            };
            let Expression::FunctionExpression(function) = declarator.init.as_ref()? else {
                return None;
            };
            if let Some(function_id) = &function.id
                && function_id.name != binding.name
            {
                return None;
            }
            Some(FunctionDeclarationCandidate {
                statement_index,
                declaration_start: statement.span().start,
                binding_name: binding.name.as_str().to_string(),
            })
        })
        .collect()
}

fn function_declaration_replacement<'a>(
    builder: &AstBuilder<'a>,
    statement: &mut Statement<'a>,
    binding_name: String,
) -> Option<oxc_allocator::Box<'a, Function<'a>>> {
    let Statement::VariableDeclaration(declaration) = statement else {
        return None;
    };
    let declarator = &mut declaration.declarations[0];
    let init = declarator.init.take()?;
    let Expression::FunctionExpression(mut function) = init else {
        declarator.init = Some(init);
        return None;
    };
    function.r#type = FunctionType::FunctionDeclaration;
    function.id = Some(builder.binding_identifier(SPAN, binding_name.as_str()));
    Some(function)
}

#[derive(Debug, Clone)]
struct ClassDeclarationCandidate {
    statement_index: usize,
    declaration_start: u32,
    binding_name: String,
}

fn recover_class_declarations<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    report: &mut ReadabilityReport,
) {
    let candidates = {
        let semantic = SemanticBuilder::new().build(program).semantic;
        let symbols = semantic.symbols();
        let root_scope_id = semantic.scopes().root_scope_id();
        collect_class_declaration_candidates(program)
            .into_iter()
            .filter(|candidate| {
                symbols
                    .symbol_ids()
                    .find(|symbol_id| {
                        symbols.get_scope_id(*symbol_id) == root_scope_id
                            && symbols.get_name(*symbol_id) == candidate.binding_name
                    })
                    .is_some_and(|symbol_id| {
                        symbols.get_resolved_references(symbol_id).all(|reference| {
                            semantic.reference_span(reference).start > candidate.declaration_start
                        })
                    })
            })
            .collect::<Vec<_>>()
    };
    if candidates.is_empty() {
        return;
    }
    let names_by_statement = candidates
        .into_iter()
        .map(|candidate| (candidate.statement_index, candidate.binding_name))
        .collect::<BTreeMap<_, _>>();
    let builder = AstBuilder::new(allocator);
    for (statement_index, statement) in program.body.iter_mut().enumerate() {
        let Some(binding_name) = names_by_statement.get(&statement_index).cloned() else {
            continue;
        };
        let replacement = class_declaration_replacement(&builder, statement, binding_name);
        if let Some(class) = replacement {
            *statement = Statement::ClassDeclaration(class);
            if let Some(binding_name) = names_by_statement.get(&statement_index) {
                report.push(format!("recovered class declaration {binding_name}"));
            }
        }
    }
}

fn collect_class_declaration_candidates(program: &Program<'_>) -> Vec<ClassDeclarationCandidate> {
    program
        .body
        .iter()
        .enumerate()
        .filter_map(|(statement_index, statement)| {
            let Statement::VariableDeclaration(declaration) = statement else {
                return None;
            };
            if declaration.kind != VariableDeclarationKind::Const
                || declaration.declare
                || declaration.declarations.len() != 1
            {
                return None;
            }
            let declarator = &declaration.declarations[0];
            if declarator.definite
                || declarator.id.type_annotation.is_some()
                || declarator.id.optional
            {
                return None;
            }
            let BindingPatternKind::BindingIdentifier(binding) = &declarator.id.kind else {
                return None;
            };
            let Expression::ClassExpression(class) = declarator.init.as_ref()? else {
                return None;
            };
            if let Some(class_id) = &class.id
                && class_id.name != binding.name
            {
                return None;
            }
            Some(ClassDeclarationCandidate {
                statement_index,
                declaration_start: statement.span().start,
                binding_name: binding.name.as_str().to_string(),
            })
        })
        .collect()
}

fn class_declaration_replacement<'a>(
    builder: &AstBuilder<'a>,
    statement: &mut Statement<'a>,
    binding_name: String,
) -> Option<oxc_allocator::Box<'a, Class<'a>>> {
    let Statement::VariableDeclaration(declaration) = statement else {
        return None;
    };
    let declarator = &mut declaration.declarations[0];
    let init = declarator.init.take()?;
    let Expression::ClassExpression(mut class) = init else {
        declarator.init = Some(init);
        return None;
    };
    class.r#type = ClassType::ClassDeclaration;
    class.id = Some(builder.binding_identifier(SPAN, binding_name.as_str()));
    Some(class)
}

fn apply_object_property_readability(program: &mut Program<'_>, report: &mut ReadabilityReport) {
    let mut shorthand = ObjectPropertyReadability { report };
    shorthand.visit_program(program);
}

struct ObjectPropertyReadability<'report> {
    report: &'report mut ReadabilityReport,
}

impl<'a> VisitMut<'a> for ObjectPropertyReadability<'_> {
    fn visit_object_property(&mut self, property: &mut ObjectProperty<'a>) {
        if !property.computed
            && !property.method
            && !property.shorthand
            && let Some(property_name) = property_key_readability_name(&property.key)
            && let Expression::Identifier(identifier) = &property.value
            && identifier.name.as_str() == property_name
        {
            property.shorthand = true;
            self.report
                .push(format!("applied object shorthand {property_name}"));
        }
        if property.kind == PropertyKind::Init
            && !property.computed
            && !property.method
            && !property.shorthand
            && let Expression::FunctionExpression(function) = &property.value
            && function.id.is_none()
        {
            if let Some(property_name) = property_key_readability_name(&property.key) {
                self.report
                    .push(format!("recovered object method {property_name}"));
            }
            property.method = true;
        }
        walk_object_property_mut(self, property);
    }
}

#[derive(Debug, Clone)]
struct ObjectDestructureCandidate {
    statement_index: usize,
    object_name: String,
    property_name: String,
    local_name: String,
}

fn recover_object_destructuring<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    report: &mut ReadabilityReport,
) {
    let candidates = collect_object_destructure_candidates(program);
    if candidates.len() < 2 {
        return;
    }
    let candidates_by_index = candidates
        .into_iter()
        .map(|candidate| (candidate.statement_index, candidate))
        .collect::<BTreeMap<_, _>>();
    let builder = AstBuilder::new(allocator);
    let mut replacements = BTreeMap::<usize, Statement<'a>>::new();
    let mut removals = BTreeSet::<usize>::new();
    let mut index = 0usize;
    while index < program.body.len() {
        let Some(first) = candidates_by_index.get(&index) else {
            index += 1;
            continue;
        };
        let mut group = vec![first.clone()];
        let mut next_index = index + 1;
        while let Some(candidate) = candidates_by_index.get(&next_index) {
            if candidate.object_name != first.object_name {
                break;
            }
            group.push(candidate.clone());
            next_index += 1;
        }
        if group.len() < 2 || !object_destructure_group_is_safe(&group) {
            index += 1;
            continue;
        }
        let replacement = object_destructure_statement(&builder, &group);
        replacements.insert(index, replacement);
        for candidate in group.iter().skip(1) {
            removals.insert(candidate.statement_index);
        }
        report.push(format!(
            "recovered object destructuring {} -> {{{}}}",
            first.object_name,
            group
                .iter()
                .map(|candidate| {
                    if candidate.property_name == candidate.local_name {
                        candidate.property_name.clone()
                    } else {
                        format!("{}: {}", candidate.property_name, candidate.local_name)
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        ));
        index = next_index;
    }
    if replacements.is_empty() {
        return;
    }

    let mut next_body = builder.vec();
    for (statement_index, statement) in program.body.drain(..).enumerate() {
        if let Some(replacement) = replacements.remove(&statement_index) {
            next_body.push(replacement);
            continue;
        }
        if removals.contains(&statement_index) {
            continue;
        }
        next_body.push(statement);
    }
    program.body = next_body;
}

fn collect_object_destructure_candidates(program: &Program<'_>) -> Vec<ObjectDestructureCandidate> {
    program
        .body
        .iter()
        .enumerate()
        .filter_map(|(statement_index, statement)| {
            let Statement::VariableDeclaration(declaration) = statement else {
                return None;
            };
            if declaration.kind != VariableDeclarationKind::Const
                || declaration.declare
                || declaration.declarations.len() != 1
            {
                return None;
            }
            let declarator = &declaration.declarations[0];
            if declarator.definite
                || declarator.id.type_annotation.is_some()
                || declarator.id.optional
            {
                return None;
            }
            let BindingPatternKind::BindingIdentifier(local) = &declarator.id.kind else {
                return None;
            };
            let Expression::StaticMemberExpression(member) = declarator.init.as_ref()? else {
                return None;
            };
            if member.optional {
                return None;
            }
            let Expression::Identifier(object) = &member.object else {
                return None;
            };
            let object_name = object.name.as_str();
            let property_name = member.property.name.as_str();
            let local_name = local.name.as_str();
            if object_name == local_name
                || sanitize_identifier(property_name) != property_name
                || sanitize_identifier(local_name) != local_name
            {
                return None;
            }
            Some(ObjectDestructureCandidate {
                statement_index,
                object_name: object_name.to_string(),
                property_name: property_name.to_string(),
                local_name: local_name.to_string(),
            })
        })
        .collect()
}

fn object_destructure_group_is_safe(group: &[ObjectDestructureCandidate]) -> bool {
    let mut properties = BTreeSet::new();
    let mut locals = BTreeSet::new();
    group.iter().all(|candidate| {
        properties.insert(candidate.property_name.as_str())
            && locals.insert(candidate.local_name.as_str())
    })
}

fn object_destructure_statement<'a>(
    builder: &AstBuilder<'a>,
    group: &[ObjectDestructureCandidate],
) -> Statement<'a> {
    let mut properties = builder.vec();
    for candidate in group {
        let key = builder.property_key_identifier_name(SPAN, candidate.property_name.as_str());
        let value_kind =
            builder.binding_pattern_kind_binding_identifier(SPAN, candidate.local_name.as_str());
        let value = builder.binding_pattern(value_kind, NONE, false);
        properties.push(builder.binding_property(
            SPAN,
            key,
            value,
            candidate.property_name == candidate.local_name,
            false,
        ));
    }
    let pattern_kind = builder.binding_pattern_kind_object_pattern(SPAN, properties, NONE);
    let pattern = builder.binding_pattern(pattern_kind, NONE, false);
    let init = Some(builder.expression_identifier_reference(SPAN, group[0].object_name.as_str()));
    let mut declarations = builder.vec();
    declarations.push(builder.variable_declarator(
        SPAN,
        VariableDeclarationKind::Const,
        pattern,
        init,
        false,
    ));
    Statement::VariableDeclaration(builder.alloc_variable_declaration(
        SPAN,
        VariableDeclarationKind::Const,
        declarations,
        false,
    ))
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

struct ReadabilityRenamer<'a> {
    builder: AstBuilder<'a>,
    symbol_renames: BTreeMap<SymbolId, String>,
    reference_renames: BTreeMap<ReferenceId, String>,
}

impl<'a> VisitMut<'a> for ReadabilityRenamer<'a> {
    fn visit_binding_identifier(&mut self, identifier: &mut BindingIdentifier<'a>) {
        let Some(symbol_id) = identifier.symbol_id.get() else {
            return;
        };
        let Some(renamed) = self.symbol_renames.get(&symbol_id) else {
            return;
        };
        identifier.name = self.builder.atom(renamed);
    }

    fn visit_identifier_reference(&mut self, identifier: &mut IdentifierReference<'a>) {
        let Some(reference_id) = identifier.reference_id.get() else {
            return;
        };
        let Some(renamed) = self.reference_renames.get(&reference_id) else {
            return;
        };
        identifier.name = self.builder.atom(renamed);
    }
}

fn generated_import_statement<'a>(
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

fn generated_export_statement<'a>(
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

fn empty_export_statement<'a>(builder: &AstBuilder<'a>) -> Statement<'a> {
    Statement::ExportNamedDeclaration(builder.alloc_export_named_declaration(
        SPAN,
        None,
        builder.vec(),
        None,
        ImportOrExportKind::Value,
        NONE,
    ))
}

fn coalesce_simple_local_named_exports_in_program<'a>(
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        CompilerLowering, GeneratedExport, GeneratedImport, GeneratedRename, ImportUsageScope,
        JsError, LazyBodyClassification, ParseGoal, TopLevelStatementKind,
        classify_import_usage_scope, classify_lazy_module_body,
        collect_file_url_source_location_rewrites, collect_identifier_read_facts,
        collect_path_builder_calls, collect_static_resource_specifiers,
        collect_static_template_literals, collect_string_literals,
        collect_top_level_statement_facts, extract_lazy_module_eager_value, format_source_minified,
        format_source_pretty, format_source_with_module_items,
        format_source_with_module_items_and_renames,
        format_source_with_module_items_and_renames_with_report, lazy_value_sub_snippets,
        normalize_source_for_pipeline, parse_error_message, parse_options_for, parse_source,
        sanitize_identifier, skip_block_comment, skip_line_comment,
        verify_only_immediate_call_references,
    };
    use std::collections::BTreeSet;

    #[test]
    fn shared_parser_options_allow_top_level_return() {
        let source_type = super::source_type_for_parse(None, ParseGoal::JavaScript);
        assert!(parse_options_for(source_type).allow_return_outside_function);
    }

    #[test]
    fn shared_identifier_helpers_cover_contextual_keywords_and_comments() {
        assert!(super::is_ascii_identifier_start(b'$'));
        assert!(super::is_ascii_identifier_continue(b'9'));
        assert!(super::is_js_keyword("async"));
        assert_eq!(skip_line_comment(b"// comment\nnext", 2), 10);
        assert_eq!(skip_block_comment(b"/* block */next", 2), 11);
    }

    #[test]
    fn parses_typescript_without_external_tooling() {
        let source = "const answer: number = 42;";

        assert!(parse_source(source, Some(Path::new("fixture.ts")), ParseGoal::TypeScript).is_ok());
    }

    #[test]
    fn collects_string_literal_facts_from_ast_only() {
        let source =
            "import './tree-sitter.wasm';\nconst native = require('/$bunfs/root/addon.node');";

        let literals =
            collect_string_literals(source, Some(Path::new("fixture.ts")), ParseGoal::TypeScript)
                .expect("string literals should be collected from parseable source");

        let values = literals
            .iter()
            .map(|literal| literal.value.as_str())
            .collect::<Vec<_>>();
        assert!(values.contains(&"./tree-sitter.wasm"));
        assert!(values.contains(&"/$bunfs/root/addon.node"));
        assert!(
            literals
                .iter()
                .all(|literal| literal.byte_end > literal.byte_start)
        );
    }

    #[test]
    fn collects_static_template_literals_without_touching_tagged_or_interpolated() {
        let source = r#"
            const docs = `one
two`;
            const nested = `${`inner
value`}-${name}`;
            const tagged = tag`raw
value`;
        "#;

        let literals = collect_static_template_literals(
            source,
            Some(Path::new("fixture.ts")),
            ParseGoal::TypeScript,
        )
        .expect("template literals should be collected from parseable source");

        let values = literals
            .iter()
            .map(|literal| literal.value.as_str())
            .collect::<Vec<_>>();
        assert!(values.contains(&"one\ntwo"));
        assert!(values.contains(&"inner\nvalue"));
        assert!(!values.contains(&"raw\nvalue"));
        assert!(
            literals
                .iter()
                .all(|literal| literal.byte_end > literal.byte_start)
        );
    }

    #[test]
    fn collects_top_level_statement_facts_for_runtime_attribution() {
        let source = "import { dep } from './dep.js';\n\
                      var data = lazyValue(() => ({ dep }));\n\
                      var regular = 1;\n\
                      function __reverts_set_data(value) { data = value; return value; }\n\
                      async function run() { return data(); }\n\
                      class Box {}\n\
                      export { data, run };\n";

        let facts = collect_top_level_statement_facts(
            source,
            Some(Path::new("runtime.ts")),
            ParseGoal::TypeScript,
        )
        .expect("runtime statements should parse");

        let kinds = facts.iter().map(|fact| fact.kind).collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                TopLevelStatementKind::Import,
                TopLevelStatementKind::LazyValue,
                TopLevelStatementKind::Variable,
                TopLevelStatementKind::Setter,
                TopLevelStatementKind::Function,
                TopLevelStatementKind::Class,
                TopLevelStatementKind::Export,
            ]
        );
        assert_eq!(facts[1].bindings, vec!["data"]);
        assert_eq!(facts[3].bindings, vec!["__reverts_set_data"]);
        assert_eq!(facts[4].bindings, vec!["run"]);
        assert!(
            facts.iter().all(|fact| fact.byte_end > fact.byte_start),
            "statement spans must be non-empty: {facts:?}"
        );
    }

    #[test]
    fn collects_identifier_reads_from_ast_without_string_scanner() {
        let source = r#"
            const { local } = source;
            const copy = [...shared, local];
            class Transport extends Base {
                field = Buffer.alloc(0);
                method(event) { return event.ready + shared; }
            }
            packageInit();
            object.packageInit();
            new Constructed();
            const template = `${source}-${shared}`;
            const ignored = "packageInit()";
            export { shared as exportedShared };
            export { externalOnly } from "./external";
        "#;

        let facts = collect_identifier_read_facts(
            source,
            Some(Path::new("fixture.ts")),
            ParseGoal::TypeScript,
        )
        .expect("identifier reads should be collected from parseable source");
        let names = facts
            .iter()
            .map(|fact| fact.name.as_str())
            .collect::<BTreeSet<_>>();
        assert!(names.contains("source"));
        assert!(names.contains("shared"));
        assert!(names.contains("Base"));
        assert!(names.contains("Buffer"));
        assert!(names.contains("event"));
        assert!(names.contains("packageInit"));
        assert!(names.contains("object"));
        assert!(names.contains("Constructed"));
        assert!(names.contains("shared"));
        assert!(!names.contains("Transport"));
        assert!(!names.contains("field"));
        assert!(!names.contains("method"));
        assert!(!names.contains("ready"));
        assert!(!names.contains("ignored"));
        assert!(!names.contains("externalOnly"));

        let callees = facts
            .iter()
            .filter(|fact| fact.is_call_callee)
            .map(|fact| fact.name.as_str())
            .collect::<BTreeSet<_>>();
        assert!(callees.contains("packageInit"));
        assert!(callees.contains("Constructed"));
        assert!(!callees.contains("alloc"));
        assert_eq!(
            facts
                .iter()
                .find(|fact| fact.name == "packageInit" && fact.is_call_callee)
                .and_then(|fact| fact.call_arg_count),
            Some(0)
        );
        assert_eq!(
            facts
                .iter()
                .find(|fact| fact.name == "Constructed" && fact.is_call_callee)
                .and_then(|fact| fact.call_arg_count),
            None
        );
    }

    #[test]
    fn collects_static_resource_specifiers_from_ast_contexts_only() {
        let source = r#"
            import './style.css';
            export * from './icons.svg';
            const native = require('/$bunfs/root/addon.node');
            const wasm = new URL('./parser.wasm', import.meta.url);
            const ignored = 'bash.exe';
        "#;

        let specifiers = collect_static_resource_specifiers(
            source,
            Some(Path::new("fixture.ts")),
            ParseGoal::TypeScript,
        )
        .expect("static resource specifiers should be collected");

        let values = specifiers
            .iter()
            .map(|literal| literal.value.as_str())
            .collect::<Vec<_>>();
        assert!(values.contains(&"./style.css"));
        assert!(values.contains(&"./icons.svg"));
        assert!(values.contains(&"/$bunfs/root/addon.node"));
        assert!(values.contains(&"./parser.wasm"));
        assert!(!values.contains(&"bash.exe"));
    }

    #[test]
    fn collects_file_url_source_location_rewrite_spans_from_ast_context() {
        let source = "const here = url.fileURLToPath('file:///home/runner/work/app/src/tool.ts');";

        let rewrites = collect_file_url_source_location_rewrites(
            source,
            Some(Path::new("fixture.ts")),
            ParseGoal::TypeScript,
        )
        .expect("file url source location should parse");

        assert_eq!(rewrites.len(), 1);
        assert_eq!(
            rewrites[0].value,
            "file:///home/runner/work/app/src/tool.ts"
        );
        assert_eq!(
            &source[rewrites[0].byte_start as usize..rewrites[0].byte_end as usize],
            "'file:///home/runner/work/app/src/tool.ts'"
        );
    }

    #[test]
    fn collects_path_builder_string_arguments_from_ast_context() {
        let source = "\
            const vendor = path.resolve(root, 'vendor', 'ripgrep');\n\
            const command = path.resolve(vendor, 'x64-linux', 'rg');\n\
            const inert = ['vendor', 'ripgrep', 'rg'];";

        let calls = collect_path_builder_calls(
            source,
            Some(Path::new("fixture.ts")),
            ParseGoal::TypeScript,
        )
        .expect("path builder calls should parse");

        let arguments = calls
            .iter()
            .map(|call| call.string_arguments.as_slice())
            .collect::<Vec<_>>();
        assert!(arguments.contains(&["vendor".to_string(), "ripgrep".to_string()].as_slice()));
        assert!(arguments.contains(&["x64-linux".to_string(), "rg".to_string()].as_slice()));
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn reports_parse_failure_without_panicking() {
        let error = parse_source("const =", None, ParseGoal::TypeScript);

        assert!(matches!(error, Err(JsError::ParseFailed(errors)) if !errors.is_empty()));
    }

    #[test]
    fn shared_parse_error_message_uses_first_diagnostic() {
        let error = parse_source("const =", None, ParseGoal::TypeScript)
            .expect_err("fixture should not parse");

        let message = parse_error_message(&error, "source could not be parsed");

        assert!(message.starts_with("source could not be parsed as"));
    }

    #[test]
    fn formats_typescript_through_oxc_codegen() {
        let formatted = format_source_pretty("const x:number=1", None, ParseGoal::TypeScript)
            .expect("fixture should parse");

        assert!(formatted.contains("const x: number = 1"));
    }

    #[test]
    fn minifies_typescript_through_oxc_codegen() {
        let formatted = format_source_minified(
            "const x = { alpha: ['a', 'b c'] };",
            None,
            ParseGoal::TypeScript,
        )
        .expect("fixture should parse");

        assert_eq!(formatted, "const x={alpha:['a','b c']};");
    }

    #[test]
    fn pipeline_normalization_uses_ast_codegen() {
        let normalized =
            normalize_source_for_pipeline("export function add(a,b){return a+b}", None)
                .expect("fixture should normalize");

        assert!(normalized.contains("export function add(a, b)"));
        assert!(normalized.contains("return a + b;"));
    }

    #[test]
    fn module_item_formatting_builds_imports_and_exports_as_ast_nodes() {
        let formatted = format_source_with_module_items(
            "const answer = __pkg.answer;",
            &[GeneratedImport::new("__pkg", "pkg")],
            &[GeneratedExport::new("answer")],
            Some(Path::new("modules/runtime/source-1-helpers.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import * as pkg from 'pkg';"));
        assert!(formatted.contains("const answer = pkg.answer;"));
        assert!(formatted.contains("export { answer };"));
    }

    #[test]
    fn module_item_formatting_emits_import_attributes() {
        let formatted = format_source_with_module_items(
            "const aliceblue = colors.default.aliceblue;",
            &[GeneratedImport::new("colors", "css-color-names").with_attribute("type", "json")],
            &[],
            Some(Path::new("modules/runtime/source-1-colors.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(
            formatted.contains("import * as colors from 'css-color-names' with { type: 'json' };")
        );
        assert!(formatted.contains("const aliceblue = colors.default.aliceblue;"));
    }

    #[test]
    fn module_item_formatting_coalesces_named_imports_by_source() {
        let formatted = format_source_with_module_items(
            "import { join as localJoin } from 'path';\nimport * as pathNS from 'path';\nimport { dirname as localDir, join as otherJoin } from 'path';\nconsole.log(pathNS, localJoin, localDir, otherJoin);",
            &[],
            &[],
            Some(Path::new("modules/runtime/source-1-helpers.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import { dirname, join, join as otherJoin } from 'path';"));
        assert!(formatted.contains("import * as pathNS from 'path';"));
        assert!(formatted.contains("console.log(pathNS, join, dirname, otherJoin);"));
        assert_eq!(formatted.matches("from 'path'").count(), 2);
    }

    #[test]
    fn module_item_formatting_keeps_namespace_and_named_imports_separate() {
        let formatted = format_source_with_module_items(
            "import * as pkgNS from 'pkg';\nimport { alpha } from 'pkg';\nimport { beta } from 'pkg';\nconsole.log(pkgNS, alpha, beta);",
            &[],
            &[],
            Some(Path::new("modules/runtime/source-1-helpers.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import * as pkgNS from 'pkg';"));
        assert!(formatted.contains("import { alpha, beta } from 'pkg';"));
        assert_eq!(formatted.matches("from 'pkg'").count(), 2);
    }

    #[test]
    fn module_item_formatting_merges_default_and_named_imports() {
        let formatted = format_source_with_module_items(
            "import defaultPkg from 'pkg';\nimport { alpha } from 'pkg';\nimport { beta as localBeta } from 'pkg';\nconsole.log(defaultPkg, alpha, localBeta);",
            &[],
            &[],
            Some(Path::new("modules/runtime/source-1-helpers.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import defaultPkg, { alpha, beta } from 'pkg';"));
        assert!(formatted.contains("console.log(defaultPkg, alpha, beta);"));
        assert_eq!(formatted.matches("from 'pkg'").count(), 1);
    }

    #[test]
    fn module_item_formatting_prunes_unused_named_import_specifiers() {
        let formatted = format_source_with_module_items(
            "import { used, unused } from 'pkg';\nconsole.log(used);",
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import { used } from 'pkg';"));
        assert!(!formatted.contains("unused"));
    }

    #[test]
    fn module_item_formatting_keeps_imports_used_by_local_exports() {
        let formatted = format_source_with_module_items(
            "import { used, exported, unused } from 'pkg';\nconsole.log(used);\nexport { exported };",
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import { exported, used } from 'pkg';"));
        assert!(formatted.contains("export { exported };"));
        assert!(!formatted.contains("unused"));
    }

    #[test]
    fn module_item_formatting_keeps_all_unused_import_statement_for_side_effects() {
        let formatted = format_source_with_module_items(
            "import { unused } from 'pkg';\nconsole.log(1);",
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import { unused } from 'pkg';"));
    }

    #[test]
    fn module_item_formatting_prunes_unused_default_from_mixed_import() {
        let formatted = format_source_with_module_items(
            "import defaultPkg, { used, unused } from 'pkg';\nconsole.log(used);",
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import { used } from 'pkg';"));
        assert!(!formatted.contains("defaultPkg"));
        assert!(!formatted.contains("unused"));
    }

    #[test]
    fn module_item_formatting_merges_default_and_namespace_imports() {
        let formatted = format_source_with_module_items(
            "import defaultPkg from 'pkg';\nimport * as pkgNS from 'pkg';\nconsole.log(defaultPkg, pkgNS);",
            &[],
            &[],
            Some(Path::new("modules/runtime/source-1-helpers.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import defaultPkg, * as pkgNS from 'pkg';"));
        assert_eq!(formatted.matches("from 'pkg'").count(), 1);
    }

    #[test]
    fn module_item_formatting_keeps_default_named_import_when_namespace_exists() {
        let formatted = format_source_with_module_items(
            "import defaultPkg, { alpha } from 'pkg';\nimport * as pkgNS from 'pkg';\nconsole.log(defaultPkg, alpha, pkgNS);",
            &[],
            &[],
            Some(Path::new("modules/runtime/source-1-helpers.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import defaultPkg, { alpha } from 'pkg';"));
        assert!(formatted.contains("import * as pkgNS from 'pkg';"));
        assert_eq!(formatted.matches("from 'pkg'").count(), 2);
    }

    #[test]
    fn module_item_formatting_merges_multiple_default_aliases_as_named_default() {
        let formatted = format_source_with_module_items(
            "import first from 'pkg';\nimport second from 'pkg';\nimport { alpha } from 'pkg';\nconsole.log(first, second, alpha);",
            &[],
            &[],
            Some(Path::new("modules/runtime/source-1-helpers.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import first, { alpha, default as second } from 'pkg';"));
        assert_eq!(formatted.matches("from 'pkg'").count(), 1);
    }

    #[test]
    fn module_item_formatting_merges_duplicate_default_named_imports() {
        let formatted = format_source_with_module_items(
            "import first, { alpha } from 'pkg';\nimport second, { beta } from 'pkg';\nconsole.log(first, second, alpha, beta);",
            &[],
            &[],
            Some(Path::new("modules/runtime/source-1-helpers.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import first, { alpha, beta, default as second } from 'pkg';"));
        assert_eq!(formatted.matches("from 'pkg'").count(), 1);
    }

    #[test]
    fn module_item_formatting_keeps_duplicate_default_namespace_imports() {
        let formatted = format_source_with_module_items(
            "import firstDefault, * as firstNS from 'pkg';\nimport secondDefault, * as secondNS from 'pkg';\nconsole.log(firstDefault, firstNS, secondDefault, secondNS);",
            &[],
            &[],
            Some(Path::new("modules/runtime/source-1-helpers.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import firstDefault, * as firstNS from 'pkg';"));
        assert!(formatted.contains("import secondDefault, * as secondNS from 'pkg';"));
        assert_eq!(formatted.matches("from 'pkg'").count(), 2);
    }

    #[test]
    fn module_item_formatting_flattens_node_builtin_namespace_members() {
        let formatted = format_source_with_module_items(
            "import * as pathNS from 'path';\nconsole.log(pathNS.join('a', 'b'), pathNS.resolve('x'));",
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import { join, resolve } from 'path';"));
        assert!(formatted.contains("console.log(join('a', 'b'), resolve('x'));"));
        assert!(!formatted.contains("pathNS."));
    }

    #[test]
    fn module_item_formatting_reuses_existing_named_alias_for_namespace_member() {
        let formatted = format_source_with_module_items(
            "import { join as j } from 'path';\nimport * as pathNS from 'path';\nconsole.log(pathNS.join('a', 'b'));",
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import { join } from 'path';"));
        assert!(formatted.contains("console.log(join('a', 'b'));"));
        assert_eq!(formatted.matches("from 'path'").count(), 1);
    }

    #[test]
    fn module_item_formatting_flattens_default_namespace_builtin_import() {
        let formatted = format_source_with_module_items(
            "import pathDefault, * as pathNS from 'path';\nconsole.log(pathNS.join('a', 'b'), pathDefault.sep);",
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(
            formatted
                .contains("import pathDefault, { join as __reverts_pathNS_join } from 'path';"),
            "{formatted}"
        );
        assert!(
            formatted.contains("console.log(__reverts_pathNS_join('a', 'b'), pathDefault.sep);")
        );
        assert!(!formatted.contains("pathNS."));
        assert_eq!(formatted.matches("from 'path'").count(), 1);
    }

    #[test]
    fn module_item_formatting_keeps_namespace_import_used_as_value() {
        let formatted = format_source_with_module_items(
            "import * as pathNS from 'path';\nconsole.log(pathNS, pathNS.join('a', 'b'));",
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import * as pathNS from 'path';"));
        assert!(formatted.contains("pathNS.join"));
    }

    #[test]
    fn module_item_formatting_keeps_exported_namespace_imports() {
        let formatted = format_source_with_module_items(
            "import * as pathNS from 'path';\nconsole.log(pathNS.join('a', 'b'));\nexport { pathNS };",
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import * as pathNS from 'path';"));
        assert!(formatted.contains("pathNS.join"));
        assert!(formatted.contains("export { pathNS };"));
    }

    #[test]
    fn module_item_formatting_keeps_non_builtin_namespace_imports() {
        let formatted = format_source_with_module_items(
            "import * as pkgNS from 'pkg';\nconsole.log(pkgNS.join('a', 'b'));",
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import { join } from 'pkg';"));
        assert!(formatted.contains("console.log(join('a', 'b'));"));
    }

    #[test]
    fn module_item_formatting_inlines_single_use_tiny_return_helper() {
        let formatted = format_source_with_module_items(
            "const value = 1;\nfunction readValue() { return value; }\nconsole.log(readValue());",
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(!formatted.contains("function readValue"));
        assert!(formatted.contains("console.log(value);"));
    }

    #[test]
    fn module_item_formatting_inlines_single_use_const_arrow_helper() {
        let formatted = format_source_with_module_items(
            "const value = 1;\nconst readValue = () => value;\nconsole.log(readValue());",
            &[],
            &[],
            Some(Path::new("modules/runtime/source-1-helpers.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(!formatted.contains("readValue ="));
        assert!(formatted.contains("console.log(value);"));
    }

    #[test]
    fn module_item_formatting_inlines_single_use_function_expression_helper() {
        let formatted = format_source_with_module_items(
            "const value = 1;\nconst readValue = function() { return value; };\nconsole.log(readValue());",
            &[],
            &[],
            Some(Path::new("modules/runtime/source-1-helpers.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(!formatted.contains("readValue ="));
        assert!(formatted.contains("console.log(value);"));
    }

    #[test]
    fn module_item_formatting_inlines_single_use_one_param_helper() {
        let formatted = format_source_with_module_items(
            "function readName(item) { return item.name; }\nconst user = { name: 'Ada' };\nconsole.log(readName(user));",
            &[],
            &[],
            Some(Path::new("modules/runtime/source-1-helpers.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(!formatted.contains("function readName"));
        assert!(formatted.contains("console.log(user.name);"));
    }

    #[test]
    fn module_item_formatting_keeps_multi_use_tiny_return_helper() {
        let formatted = format_source_with_module_items(
            "const value = 1;\nfunction readValue() { return value; }\nconsole.log(readValue(), readValue());",
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("function readValue"));
        assert!(formatted.contains("readValue(), readValue()"));
    }

    #[test]
    fn module_item_formatting_keeps_exported_tiny_return_helper() {
        let formatted = format_source_with_module_items(
            "const value = 1;\nfunction readValue() { return value; }\nconsole.log(readValue());\nexport { readValue };",
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("function readValue"));
        assert!(formatted.contains("export { readValue };"));
    }

    #[test]
    fn module_item_formatting_keeps_this_based_tiny_return_helper() {
        let formatted = format_source_with_module_items(
            "function readThis() { return this.value; }\nconsole.log(readThis());",
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("function readThis"));
        assert!(formatted.contains("readThis()"));
    }

    #[test]
    fn module_item_formatting_keeps_one_param_helper_with_impure_arg() {
        let formatted = format_source_with_module_items(
            "function readName(item) { return item.name; }\nconsole.log(readName(makeUser()));",
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("function readName"));
        assert!(formatted.contains("readName(makeUser())"));
    }

    #[test]
    fn module_item_formatting_keeps_enhanced_tiny_helper_outside_runtime() {
        let formatted = format_source_with_module_items(
            "function compute(input) { return input * 2; }\nvar entry = compute(21);",
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("function compute"));
        assert!(formatted.contains("var entry = compute(21);"));
    }

    #[test]
    fn module_item_formatting_keeps_one_param_helper_with_object_shorthand() {
        let formatted = format_source_with_module_items(
            "const wrap = (item) => ({ item });\nconsole.log(wrap(user));",
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("const wrap ="));
        assert!(formatted.contains("wrap(user)"));
    }

    #[test]
    fn module_item_formatting_coalesces_local_named_exports() {
        let formatted = format_source_with_module_items(
            "const alpha = 1;\nconst beta = 2;\nexport { beta };\nconsole.log(alpha, beta);",
            &[],
            &[GeneratedExport::new("alpha"), GeneratedExport::new("beta")],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("export { alpha, beta };"));
        assert_eq!(formatted.matches("export {").count(), 1);
        assert!(formatted.contains("console.log(alpha, beta);"));
    }

    #[test]
    fn module_item_formatting_keeps_alias_and_reexports_separate() {
        let formatted = format_source_with_module_items(
            "const alpha = 1;\nconst beta = 2;\nexport { beta };\nexport { beta as renamed };\nexport { gamma } from './gamma.js';",
            &[],
            &[GeneratedExport::new("alpha")],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("const renamed = 2;"));
        assert!(formatted.contains("export { renamed as beta };"));
        assert!(formatted.contains("export { alpha, renamed };"));
        assert!(formatted.contains("export { gamma } from './gamma.js';"));
        assert_eq!(formatted.matches("export {").count(), 3);
    }

    #[test]
    fn empty_module_item_formatting_emits_parseable_empty_module() {
        let formatted = format_source_with_module_items(
            "",
            &[],
            &[],
            Some(Path::new("src/empty.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("empty module should format");

        assert_eq!(formatted.trim(), "export {};");
    }

    #[test]
    fn readability_renames_source_backed_binding_before_codegen() {
        let formatted = format_source_with_module_items_and_renames(
            "var $F1 = 1; console.log($F1); export { $F1 };",
            &[],
            &[],
            &[GeneratedRename::new("$F1", "lodashGlobalObjectInit")],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("var lodashGlobalObjectInit = 1;"));
        assert!(formatted.contains("console.log(lodashGlobalObjectInit);"));
        assert!(formatted.contains("export { lodashGlobalObjectInit as $F1 };"));
    }

    #[test]
    fn readability_renames_every_resolved_reference_but_not_shadowed_text() {
        let formatted = format_source_with_module_items_and_renames(
            "var $F1 = 1; function outer() { console.log($F1); function inner($F1) { return $F1; } return inner; } var obj = {}; obj.$F1 = \"$F1\";",
            &[],
            &[],
            &[GeneratedRename::new("$F1", "readableValue")],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("var readableValue = 1;"));
        assert!(formatted.contains("console.log(readableValue);"));
        assert!(formatted.contains("function inner($F1)"));
        assert!(formatted.contains("return $F1;"));
        assert!(formatted.contains("obj.$F1 = '$F1';"));
    }

    #[test]
    fn readability_renames_named_import_alias_to_imported_name() {
        let formatted = format_source_with_module_items_and_renames(
            "import { map as $F1 } from 'lodash'; console.log($F1); export { $F1 };",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import { map } from 'lodash';"));
        assert!(formatted.contains("console.log(map);"));
        assert!(formatted.contains("export { map as $F1 };"));
        assert!(!formatted.contains("$F1);"));
    }

    #[test]
    fn readability_renames_named_import_alias_skips_collisions() {
        let formatted = format_source_with_module_items_and_renames(
            "import { map as $F1 } from 'lodash'; const map = 1; console.log($F1, map);",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import { map as $F1 } from 'lodash';"));
        assert!(formatted.contains("const map = 1;"));
        assert!(formatted.contains("console.log($F1, map);"));
    }

    #[test]
    fn readability_renames_generated_namespace_import_alias_from_specifier() {
        let formatted = format_source_with_module_items_and_renames(
            "const answer = __pkg_lodash_map.answer;",
            &[GeneratedImport::new("__pkg_lodash_map", "lodash/map")],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import * as lodashMap from 'lodash/map';"));
        assert!(formatted.contains("const answer = lodashMap.answer;"));
    }

    #[test]
    fn readability_renames_namespace_import_keeps_handwritten_alias() {
        let formatted = format_source_with_module_items_and_renames(
            "import * as utilities from 'lodash'; console.log(utilities, utilities.map);",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import * as utilities from 'lodash';"));
        assert!(formatted.contains("console.log(utilities, utilities.map);"));
    }

    #[test]
    fn readability_renames_explicit_hint_takes_precedence_over_import_alias_cleanup() {
        let formatted = format_source_with_module_items_and_renames(
            "import { map as $F1 } from 'lodash'; console.log($F1);",
            &[],
            &[],
            &[GeneratedRename::new("$F1", "lodashMap")],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import { map as lodashMap } from 'lodash';"));
        assert!(formatted.contains("console.log(lodashMap);"));
    }

    #[test]
    fn readability_renames_from_export_specifier_and_uses_object_shorthand() {
        let formatted = format_source_with_module_items_and_renames(
            "const a = 1; const obj = { createClient: a }; console.log(obj); export { a as createClient };",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("const createClient = 1;"));
        assert!(formatted.contains("const obj = { createClient };"));
        assert!(formatted.contains("export { createClient };"));
    }

    #[test]
    fn readability_hint_resolver_prefers_export_name_over_later_object_property() {
        let formatted = format_source_with_module_items_and_renames(
            "const a = 1; export { a as createClient }; const obj = { internalName: a };",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("const createClient = 1;"));
        assert!(formatted.contains("export { createClient };"));
        assert!(formatted.contains("const obj = { internalName: createClient };"));
        assert!(!formatted.contains("const internalName = 1;"));
    }

    #[test]
    fn readability_hint_resolver_skips_conflicting_object_property_names() {
        let (formatted, report) = format_source_with_module_items_and_renames_with_report(
            "const a = 1; const first = { foo: a }; const second = { bar: a }; console.log(a);",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("const a = 1;"));
        assert!(formatted.contains("foo: a"));
        assert!(formatted.contains("bar: a"));
        assert!(formatted.contains("console.log(a);"));
        assert!(
            report
                .entries
                .iter()
                .any(|entry| entry.contains("reason=conflicting_hints"))
        );
    }

    #[test]
    fn readability_renames_from_commonjs_export_property_and_recovers_function_declaration() {
        let formatted = format_source_with_module_items_and_renames(
            "const a = function() { return 1; }; exports.createClient = a;",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("function createClient()"));
        assert!(formatted.contains("exports.createClient = createClient;"));
        assert!(!formatted.contains("const a = function"));
    }

    #[test]
    fn readability_renames_from_commonjs_bracket_export_property() {
        let formatted = format_source_with_module_items_and_renames(
            "const a = 1; exports['createClient'] = a;",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("const createClient = 1;"));
        assert!(formatted.contains("exports['createClient'] = createClient;"));
    }

    #[test]
    fn readability_renames_from_module_exports_object_and_uses_shorthand() {
        let formatted = format_source_with_module_items_and_renames(
            "const a = 1; module.exports = { createClient: a };",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("const createClient = 1;"));
        assert!(formatted.contains("module.exports = { createClient };"));
    }

    #[test]
    fn readability_renames_api_object_exports_and_recovers_functions() {
        let formatted = format_source_with_module_items_and_renames(
            "const a = function() { return 1; }; const b = function() { return 2; }; module.exports = { createClient: a, close: b };",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("function createClient()"));
        assert!(formatted.contains("function close()"));
        let compact = formatted.split_whitespace().collect::<String>();
        assert!(compact.contains("module.exports={createClient,close};"));
        assert!(!formatted.contains("const a = function"));
        assert!(!formatted.contains("const b = function"));
    }

    #[test]
    fn readability_renames_from_object_define_property_getter() {
        let formatted = format_source_with_module_items_and_renames(
            "const a = 1; Object.defineProperty(exports, 'createClient', { get: function() { return a; } });",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("const createClient = 1;"));
        assert!(formatted.contains("get()"));
        assert!(formatted.contains("return createClient;"));
    }

    #[test]
    fn readability_report_records_applied_rename_and_polish() {
        let (formatted, report) = format_source_with_module_items_and_renames_with_report(
            "const a = function() { return 1; }; exports.createClient = a;",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("function createClient()"));
        assert!(
            report.entries.iter().any(|entry| {
                entry.contains("renamed a -> createClient, source=commonjs_export")
            })
        );
        assert!(
            report
                .entries
                .iter()
                .any(|entry| entry.contains("recovered function declaration createClient"))
        );
    }

    #[test]
    fn readability_polish_inlines_safe_aliases() {
        let formatted = format_source_with_module_items_and_renames(
            "const settings = loadSettings(); const alias = settings; console.log(alias);",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("const settings = loadSettings();"));
        assert!(formatted.contains("console.log(settings);"));
        assert!(!formatted.contains("const alias = settings;"));
    }

    #[test]
    fn readability_usage_based_names_generated_bindings_from_initializers() {
        let formatted = format_source_with_module_items_and_renames(
            "const a = new Client(); const b = createLogger(); console.log(a, b);",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("const client = new Client();"));
        assert!(formatted.contains("const logger = createLogger();"));
        assert!(formatted.contains("console.log(client, logger);"));
    }

    #[test]
    fn readability_usage_based_name_does_not_override_public_export_name() {
        let formatted = format_source_with_module_items_and_renames(
            "const a = createLogger(); export { a as createClient };",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("const createClient = createLogger();"));
        assert!(formatted.contains("export { createClient };"));
        assert!(!formatted.contains("const logger = createLogger();"));
    }

    #[test]
    fn readability_usage_based_names_keep_readable_short_bindings() {
        let formatted = format_source_with_module_items_and_renames(
            "const id = new Client(); const v1 = createLogger(); console.log(id, v1);",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("const id = new Client();"));
        assert!(formatted.contains("const logger = createLogger();"));
        assert!(formatted.contains("console.log(id, logger);"));
        assert!(!formatted.contains("const client = new Client();"));
    }

    #[test]
    fn readability_polish_keeps_exported_aliases() {
        let formatted = format_source_with_module_items_and_renames(
            "const settings = 1; const alias = settings; console.log(alias); export { alias };",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("const alias = settings;"));
        assert!(formatted.contains("console.log(alias);"));
        assert!(formatted.contains("export { alias };"));
    }

    #[test]
    fn readability_polish_keeps_alias_when_source_name_is_shadowed() {
        let formatted = format_source_with_module_items_and_renames(
            "const settings = 1; const alias = settings; function f(settings) { return alias; }",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("const alias = settings;"));
        assert!(formatted.contains("return alias;"));
    }

    #[test]
    fn readability_polish_recovers_object_destructuring() {
        let (formatted, report) = format_source_with_module_items_and_renames_with_report(
            "const createClient = api.createClient; const close = api.close; console.log(createClient, close);",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("const { createClient, close } = api;"));
        assert!(formatted.contains("console.log(createClient, close);"));
        assert!(!formatted.contains("const close = api.close;"));
        assert!(
            report
                .entries
                .iter()
                .any(|entry| entry.contains("recovered object destructuring api"))
        );
    }

    #[test]
    fn readability_polish_recovers_aliased_object_destructuring() {
        let formatted = format_source_with_module_items_and_renames(
            "const client = api.createClient; const close = api.close; console.log(client, close);",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("const { createClient: client, close } = api;"));
        assert!(formatted.contains("console.log(client, close);"));
        assert!(!formatted.contains("const client = api.createClient;"));
        assert!(!formatted.contains("const close = api.close;"));
    }

    #[test]
    fn readability_polish_merges_and_sorts_duplicate_named_imports() {
        let formatted = format_source_with_module_items_and_renames(
            "import { z } from 'pkg'; import { a } from 'pkg'; console.log(z, a);",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert_eq!(formatted.matches("from 'pkg'").count(), 1);
        assert!(formatted.contains("import { a, z } from 'pkg';"));
    }

    #[test]
    fn readability_polish_merges_default_and_named_imports() {
        let formatted = format_source_with_module_items_and_renames(
            "import React from 'react'; import { useMemo } from 'react'; console.log(React, useMemo);",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert_eq!(formatted.matches("from 'react'").count(), 1);
        assert!(formatted.contains("import React, { useMemo } from 'react';"));
        assert!(formatted.contains("console.log(React, useMemo);"));
    }

    #[test]
    fn readability_polish_splits_safe_namespace_imports() {
        let formatted = format_source_with_module_items_and_renames(
            "import * as lodash from 'lodash'; console.log(lodash.map(items, fn), lodash.filter(items, fn));",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import { filter, map } from 'lodash';"));
        assert!(formatted.contains("console.log(map(items, fn), filter(items, fn));"));
        assert!(!formatted.contains("lodash.map"));
    }

    #[test]
    fn readability_polish_keeps_namespace_import_when_namespace_escapes() {
        let formatted = format_source_with_module_items_and_renames(
            "import * as lodash from 'lodash'; console.log(lodash, lodash.map);",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import * as lodash from 'lodash';"));
        assert!(formatted.contains("console.log(lodash, lodash.map);"));
    }

    #[test]
    fn readability_polish_recovers_class_declaration() {
        let formatted = format_source_with_module_items_and_renames(
            "const Client = class { connect() { return 1; } }; console.log(new Client());",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("class Client"));
        assert!(formatted.contains("connect()"));
        assert!(formatted.contains("console.log(new Client());"));
        assert!(!formatted.contains("const Client = class"));
    }

    #[test]
    fn readability_polish_recovers_object_method_shorthand() {
        let formatted = format_source_with_module_items_and_renames(
            "const api = { createClient: function() { return 1; } };",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("createClient()"));
        assert!(!formatted.contains("createClient: function"));
    }

    #[test]
    fn readability_polish_is_idempotent_after_late_transforms() {
        let source = "\
            import * as lodash from 'lodash';\n\
            const a = function() { return lodash.map([1], x => x); };\n\
            const Client = class { connect() { return a(); } };\n\
            const api = { createClient: function() { return a(); } };\n\
            exports.createClient = a;\n\
            console.log(lodash.filter([1], Boolean), Client, api);";
        let first = format_source_with_module_items_and_renames(
            source,
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");
        let second = format_source_with_module_items_and_renames(
            first.as_str(),
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format again");

        assert_eq!(second, first);
        assert!(first.contains("import { filter, map } from 'lodash';"));
        assert!(first.contains("function createClient()"));
        assert!(first.contains("class Client"));
        assert!(first.contains("createClient()"));
    }

    #[test]
    fn readability_polish_recovers_function_declaration_when_not_used_before_declaration() {
        let formatted = format_source_with_module_items_and_renames(
            "const createClient = function() { return 1; }; console.log(createClient());",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("function createClient()"));
        assert!(formatted.contains("console.log(createClient());"));
    }

    #[test]
    fn readability_polish_does_not_recover_hoisted_function_declaration() {
        let formatted = format_source_with_module_items_and_renames(
            "console.log(createClient); const createClient = function() { return 1; };",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("const createClient = function()"));
    }

    #[test]
    fn readability_renames_skip_root_scope_collisions() {
        let formatted = format_source_with_module_items_and_renames(
            "var a = 1; var settings = 2; console.log(a, settings);",
            &[],
            &[],
            &[GeneratedRename::new("a", "settings")],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("var a = 1;"));
        assert!(formatted.contains("var settings = 2;"));
        assert!(formatted.contains("console.log(a, settings);"));
    }

    #[test]
    fn readability_renames_skip_generated_import_collisions() {
        let formatted = format_source_with_module_items_and_renames(
            "var a = 1; console.log(a);",
            &[GeneratedImport::new("settings", "pkg")],
            &[],
            &[GeneratedRename::new("a", "settings")],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import * as settings from 'pkg';"));
        assert!(formatted.contains("var a = 1;"));
        assert!(formatted.contains("console.log(a);"));
    }

    #[test]
    fn readability_renames_skip_duplicate_targets() {
        let formatted = format_source_with_module_items_and_renames(
            "var a = 1; var b = 2; console.log(a, b);",
            &[],
            &[],
            &[
                GeneratedRename::new("a", "settings"),
                GeneratedRename::new("b", "settings"),
            ],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("var settings = 1;"));
        assert!(formatted.contains("var b = 2;"));
        assert!(formatted.contains("console.log(settings, b);"));
    }

    #[test]
    fn readability_renames_skip_names_that_would_capture_globals() {
        let formatted = format_source_with_module_items_and_renames(
            "var a = 1; function f() { return settings; }",
            &[],
            &[],
            &[GeneratedRename::new("a", "settings")],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("var a = 1;"));
        assert!(formatted.contains("return settings;"));
    }

    #[test]
    fn emit_safety_renames_strict_reserved_bindings_before_esm_output() {
        let formatted = format_source_with_module_items_and_renames(
            "var package = 1; function read() { var private = package + 1; return private; } console.log(package, read());",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("var _package = 1;"));
        assert!(formatted.contains("var _private = _package + 1;"));
        assert!(formatted.contains("return _private;"));
        assert!(formatted.contains("console.log(_package, read());"));
        assert!(!formatted.contains("var package"));
        assert!(!formatted.contains("var private"));
    }

    #[test]
    fn emit_safety_renames_avoid_existing_binding_collisions() {
        let formatted = format_source_with_module_items_and_renames(
            "var _package = 1; var package = 2; console.log(_package, package);",
            &[],
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("var _package = 1;"));
        assert!(formatted.contains("_package2 = 2"));
        assert!(formatted.contains("console.log(_package, _package2);"));
    }

    #[test]
    fn pipeline_normalization_accepts_commonjs_bin_sources() {
        let normalized = normalize_source_for_pipeline(
            "if (require.main === module) {\n  return;\n}\nmodule.exports = {};\n",
            Some(Path::new("bin/which.js")),
        )
        .expect("commonjs package source should normalize");

        assert!(normalized.contains("module.exports"));
    }

    #[test]
    fn sanitizes_package_and_minifier_fragments_into_identifiers() {
        assert_eq!(sanitize_identifier("@smithy/XY7"), "_smithy_XY7");
        assert_eq!(sanitize_identifier("9patch-name"), "_9patch_name");
        assert_eq!(sanitize_identifier("class"), "_class");
        assert_eq!(sanitize_identifier("package"), "_package");
        assert_eq!(sanitize_identifier("private"), "_private");
        assert_eq!(sanitize_identifier("arguments"), "_arguments");
    }

    fn binding_set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    #[test]
    fn classifies_top_level_reference_in_statement() {
        let source = "import { foo } from './x.js';\nconst y = foo;";
        let scope = classify_import_usage_scope(
            source,
            &binding_set(&["foo"]),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(scope.get("foo"), Some(&ImportUsageScope::TopLevel));
    }

    #[test]
    fn classifies_reference_inside_function_body_as_nested() {
        let source = "import { foo } from './x.js';\nexport function call() { return foo(); }";
        let scope = classify_import_usage_scope(
            source,
            &binding_set(&["foo"]),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(scope.get("foo"), Some(&ImportUsageScope::NestedOnly));
    }

    #[test]
    fn classifies_reference_inside_arrow_body_as_nested() {
        let source = "import { foo } from './x.js';\nconst trigger = () => foo();";
        let scope = classify_import_usage_scope(
            source,
            &binding_set(&["foo"]),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(scope.get("foo"), Some(&ImportUsageScope::NestedOnly));
    }

    #[test]
    fn classifies_reference_inside_method_body_as_nested() {
        let source = "import { foo } from './x.js';\nclass S { method() { return foo; } }";
        let scope = classify_import_usage_scope(
            source,
            &binding_set(&["foo"]),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(scope.get("foo"), Some(&ImportUsageScope::NestedOnly));
    }

    #[test]
    fn classifies_reference_inside_class_static_block_as_top_level() {
        // `static { ... }` runs at class-declaration time. If the class
        // is declared at module top level, the static block code is on
        // the module-load critical path.
        let source = "import { foo } from './x.js';\nclass S { static { foo(); } }";
        let scope = classify_import_usage_scope(
            source,
            &binding_set(&["foo"]),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(scope.get("foo"), Some(&ImportUsageScope::TopLevel));
    }

    #[test]
    fn classifies_reference_inside_class_field_initializer_as_top_level() {
        // Class field initializers in `class C { x = foo; }` run at
        // `new C()` time, not class-decl time. But the simple visitor
        // can't distinguish "runs at instantiation" from "runs at decl"
        // for instance fields — and instance fields conservatively
        // appearing TopLevel keeps us safe (we'll keep more thunks
        // lazy rather than fewer). Static initializers, on the other
        // hand, run at class declaration and are correctly TopLevel.
        let source = "import { foo } from './x.js';\nclass S { static defaultFoo = foo; }";
        let scope = classify_import_usage_scope(
            source,
            &binding_set(&["foo"]),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(scope.get("foo"), Some(&ImportUsageScope::TopLevel));
    }

    #[test]
    fn unreferenced_bindings_default_to_nested_only() {
        // No occurrence of `foo` anywhere — zero references vacuously
        // satisfies "every reference is nested-only".
        let source = "import { foo } from './x.js';\nconst y = 42;";
        let scope = classify_import_usage_scope(
            source,
            &binding_set(&["foo"]),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(scope.get("foo"), Some(&ImportUsageScope::NestedOnly));
    }

    #[test]
    #[should_panic(expected = "import usage scope classification requires parseable source")]
    fn import_usage_scope_rejects_unparseable_source() {
        let _ = classify_import_usage_scope(
            "function entry(",
            &binding_set(&["foo"]),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
    }

    #[test]
    fn promotes_to_top_level_on_first_top_level_occurrence() {
        // `foo` appears both inside a function (nested) and at the top
        // level (the expression statement `foo;`). The classification
        // must reflect the most restrictive observation — TopLevel.
        let source = "import { foo } from './x.js';\nfunction call() { return foo(); }\nfoo;";
        let scope = classify_import_usage_scope(
            source,
            &binding_set(&["foo"]),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(scope.get("foo"), Some(&ImportUsageScope::TopLevel));
    }

    #[test]
    fn ignores_property_keys_named_same_as_target_binding() {
        // `obj.foo` and `{ foo: 1 }` are property-key uses, not
        // references to a binding named `foo`. The visitor must not
        // misclassify them.
        let source = concat!(
            "import { foo } from './x.js';\n",
            "const obj = { foo: 1 };\n",
            "console.log(obj.foo);\n",
        );
        let scope = classify_import_usage_scope(
            source,
            &binding_set(&["foo"]),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(scope.get("foo"), Some(&ImportUsageScope::NestedOnly));
    }

    #[test]
    fn verifies_immediate_call_form_when_every_reference_is_x_zero_args() {
        let source = concat!(
            "import { foo } from './x.js';\n",
            "console.log(foo());\n",
            "function call() { return foo(); }\n",
        );
        let result = verify_only_immediate_call_references(
            source,
            &binding_set(&["foo"]),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(result.get("foo"), Some(&true));
    }

    #[test]
    fn rejects_immediate_call_form_when_binding_used_as_value() {
        // `register(foo)` passes `foo` as a value, not invoking it.
        let source = concat!(
            "import { foo } from './x.js';\n",
            "register(foo);\n",
            "foo();\n",
        );
        let result = verify_only_immediate_call_references(
            source,
            &binding_set(&["foo"]),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(result.get("foo"), Some(&false));
    }

    #[test]
    fn rejects_immediate_call_form_when_called_with_arguments() {
        // `foo(1)` is calling foo with an argument — not the zero-arg
        // thunk-call pattern. The binding is being used as a callable
        // value directly; eagerifying would change the call semantics.
        let source = concat!("import { foo } from './x.js';\n", "foo();\n", "foo(1);\n",);
        let result = verify_only_immediate_call_references(
            source,
            &binding_set(&["foo"]),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(result.get("foo"), Some(&false));
    }

    #[test]
    fn rejects_immediate_call_form_when_constructed() {
        let source = concat!("import { foo } from './x.js';\n", "new foo();\n",);
        let result = verify_only_immediate_call_references(
            source,
            &binding_set(&["foo"]),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(result.get("foo"), Some(&false));
    }

    #[test]
    fn rejects_immediate_call_form_on_typeof_check() {
        let source = concat!(
            "import { foo } from './x.js';\n",
            "if (typeof foo === 'function') foo();\n",
        );
        let result = verify_only_immediate_call_references(
            source,
            &binding_set(&["foo"]),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(result.get("foo"), Some(&false));
    }

    #[test]
    fn rejects_immediate_call_form_on_chained_call_result_use() {
        // `foo()` is followed by `.bar` access. The first call is the
        // expected zero-arg form, but `.bar` access on its result is
        // still a separate operation. Identifier count: 1 (foo).
        // Call count: 1 (foo()). Result: total == calls → true.
        //
        // However if there's ALSO `foo` used elsewhere as a value, the
        // result flips to false. This test confirms the chained form
        // alone is still treated as rewritable.
        let source = concat!(
            "import { foo } from './x.js';\n",
            "const value = foo().bar;\n",
            "console.log(foo().baz);\n",
        );
        let result = verify_only_immediate_call_references(
            source,
            &binding_set(&["foo"]),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(result.get("foo"), Some(&true));
    }

    #[test]
    fn vacuously_safe_when_binding_is_never_referenced() {
        let source = "import { foo } from './x.js';\nconst y = 42;";
        let result = verify_only_immediate_call_references(
            source,
            &binding_set(&["foo"]),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(result.get("foo"), Some(&true));
    }

    #[test]
    #[should_panic(expected = "call-reference verification requires parseable source")]
    fn immediate_call_reference_verification_rejects_unparseable_source() {
        let _ = verify_only_immediate_call_references(
            "function entry(",
            &binding_set(&["foo"]),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
    }

    #[test]
    fn lazy_body_classifier_extracts_direct_module_exports_value() {
        let value = extract_lazy_module_eager_value(
            "module.exports = 42;",
            "exports",
            Some("module"),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(value.as_deref(), Some("42"));
    }

    #[test]
    fn lazy_body_classifier_extracts_chain_assignment_rightmost_value() {
        // The chain `module.exports = A = class Foo {}` writes the
        // class expression to both the local `A` and `module.exports`.
        // The classifier extracts the rightmost pure expression; the
        // intermediate locals are discarded with their `var` declaration.
        let value = extract_lazy_module_eager_value(
            "var A;\nmodule.exports = A = class Foo { constructor() {} };",
            "exports",
            Some("module"),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(value.as_deref(), Some("class Foo { constructor() {} }"));
    }

    #[test]
    fn lazy_body_classifier_unwraps_iife_call_wrapper() {
        // The pattern `(function() { body }).call(this)` is a common
        // CJS shape that hides a simple `module.exports = X`
        // assignment behind an IIFE. The classifier recursively
        // descends into the IIFE body.
        let body = "(function() { var A; module.exports = A = class { hello() { return 1; } }; }).call(exports);";
        let value = extract_lazy_module_eager_value(
            body,
            "exports",
            Some("module"),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(value.as_deref(), Some("class { hello() { return 1; } }"));
    }

    #[test]
    fn lazy_body_classifier_unwraps_arrow_iife_wrapper() {
        let body = "(() => { module.exports = { ok: true }; })();";
        let value = extract_lazy_module_eager_value(
            body,
            "exports",
            Some("module"),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(value.as_deref(), Some("{ ok: true }"));
    }

    #[test]
    fn lazy_body_classifier_handles_object_define_property_with_pure_value() {
        let body = "Object.defineProperty(exports, \"value\", { value: 99, configurable: true });";
        let value = extract_lazy_module_eager_value(
            body,
            "exports",
            Some("module"),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(value.as_deref(), Some("{ value: 99 }"));
    }

    #[test]
    fn lazy_body_classifier_accepts_return_for_lazy_value_shape() {
        // `lazyValue(() => { return PURE; })` — for lazy-value
        // (no `module` parameter), the body is a `return` of a pure
        // expression.
        let value = extract_lazy_module_eager_value(
            "return { primary: '#abc', secondary: '#def' };",
            "",
            None,
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(
            value.as_deref(),
            Some("{ primary: '#abc', secondary: '#def' }")
        );
    }

    #[test]
    fn lazy_body_classifier_rejects_function_call_in_body() {
        // The body has a top-level call to `initSetup()` — could have
        // any side effect, can't hoist to module load.
        let body = "initSetup();\nmodule.exports = 42;";
        let value = extract_lazy_module_eager_value(
            body,
            "exports",
            Some("module"),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(value, None);
    }

    #[test]
    fn lazy_body_classifier_rejects_assignment_to_other_target() {
        // Assignment to a non-`module.exports`/`exports.k` target — could
        // have side effects on globals or other observable state.
        let body = "globalThis.config = 99;\nmodule.exports = 42;";
        let value = extract_lazy_module_eager_value(
            body,
            "exports",
            Some("module"),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(value, None);
    }

    #[test]
    fn lazy_body_classifier_rejects_multiple_module_exports_assignments() {
        // Two separate `module.exports = ...` writes — the final value
        // would depend on evaluation order, which conservative
        // classification refuses to pick from.
        let body = "module.exports = 1; module.exports = 2;";
        let value = extract_lazy_module_eager_value(
            body,
            "exports",
            Some("module"),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(value, None);
    }

    #[test]
    fn lazy_body_classifier_collapses_multi_key_exports_to_object_literal() {
        let body = "exports.parse = function(s) { return s; };\nexports.stringify = function(o) { return o; };";
        let value = extract_lazy_module_eager_value(
            body,
            "exports",
            Some("module"),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(
            value.as_deref(),
            Some("{ parse: function(s) { return s; }, stringify: function(o) { return o; } }")
        );
    }

    #[test]
    fn lazy_body_classifier_rejects_impure_chain_value() {
        // `module.exports = A = computeStuff()` — the final value is
        // a function call, which can have side effects. Reject.
        let body = "var A;\nmodule.exports = A = computeStuff();";
        let value = extract_lazy_module_eager_value(
            body,
            "exports",
            Some("module"),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(value, None);
    }

    #[test]
    fn lazy_body_classifier_accepts_reverts_setter_call_with_pure_arg() {
        // Setter call alongside an exports write — common in CJS
        // wrappers where some helper bindings are set as side effects
        // before the main exports value. Phase 8e folds the setter
        // into a leading comma expression so the side effect still
        // runs at module load.
        let body = "__reverts_set_helper(42); module.exports = 'value';";
        let value = extract_lazy_module_eager_value(
            body,
            "exports",
            Some("module"),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(
            value.as_deref(),
            Some("(__reverts_set_helper(42), 'value')")
        );
    }

    #[test]
    fn lazy_body_classifier_accepts_bare_identifier_statement() {
        // `x;` as a standalone expression statement is a no-op the
        // bundler emits to keep imports from being tree-shaken. Drop
        // it on collapse.
        let body = "bareImport;\nmodule.exports = 1;";
        let value = extract_lazy_module_eager_value(
            body,
            "exports",
            Some("module"),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(value.as_deref(), Some("1"));
    }

    #[test]
    fn lazy_body_classifier_collapses_exports_property_assignments() {
        let body = "exports.foo = 1; exports.bar = 2;";
        let value = extract_lazy_module_eager_value(
            body,
            "exports",
            Some("module"),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );

        assert_eq!(value.as_deref(), Some("{ foo: 1, bar: 2 }"));
    }

    #[test]
    fn lazy_body_classifier_emits_init_only_lazy_module_as_empty_exports() {
        // A `lazyModule` body that only invokes setters (no
        // `module.exports = ...` write) eagerifies to `({}, ...,
        // emptyExports)` where emptyExports is the wrapper's default
        // — observers of `X()` previously got `{}` (the untouched
        // exports object), so the rewrite preserves that.
        let body = "__reverts_set_a(1); __reverts_set_b(2);";
        let value = extract_lazy_module_eager_value(
            body,
            "exports",
            Some("module"),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(
            value.as_deref(),
            Some("(__reverts_set_a(1), __reverts_set_b(2), {})")
        );
    }

    #[test]
    fn lazy_body_classifier_emits_init_only_lazy_value_as_void_zero() {
        // For `lazyValue` bodies (no module param), an init-only
        // body returned `undefined` originally; the eagerified form
        // is `(setters..., void 0)`.
        let body = "__reverts_set_a(1);\n__reverts_set_b(2);";
        let value = extract_lazy_module_eager_value(
            body,
            "",
            None,
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(
            value.as_deref(),
            Some("(__reverts_set_a(1), __reverts_set_b(2), void 0)")
        );
    }

    #[test]
    fn lazy_body_classifier_accepts_comma_separated_setter_sequence() {
        // esbuild commonly emits multi-setter init as one statement
        // joined by commas: `setterA(1), setterB(2), setterC(3);` —
        // this is a SequenceExpression in the AST. Phase 8e walks
        // each element and accepts the whole sequence when every
        // comma-separated call is a setter (or a bare ident).
        let body = "__reverts_set_a(1), __reverts_set_b(true), __reverts_set_c({ key: 'v' });";
        let value = extract_lazy_module_eager_value(
            body,
            "",
            None,
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        // Note: the comma-separated setters are pushed individually
        // into the prologue, so the emitted comma expression flattens
        // them: `(setter_a(...), setter_b(...), setter_c(...), void 0)`.
        assert_eq!(
            value.as_deref(),
            Some(
                "(__reverts_set_a(1), __reverts_set_b(true), __reverts_set_c({ key: 'v' }), void 0)"
            )
        );
    }

    #[test]
    fn lazy_body_classifier_rejects_setter_with_function_call_arg() {
        // `__reverts_set_X(otherThunk())` — the argument is a function
        // call, which could have side effects we can't see. The
        // existing impure-call rejection is the safety floor for
        // Phase 8e; inter-procedural classification is a separate
        // future pass.
        let body = "__reverts_set_a(loadData()); module.exports = 1;";
        let value = extract_lazy_module_eager_value(
            body,
            "exports",
            Some("module"),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(value, None);
    }

    #[test]
    fn lazy_body_classifier_rejects_non_setter_call_in_body() {
        let body = "initSomething();\nmodule.exports = 1;";
        let value = extract_lazy_module_eager_value(
            body,
            "exports",
            Some("module"),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(value, None);
    }

    #[test]
    fn classify_lazy_body_returns_deps_for_zero_arg_thunk_calls() {
        // Body has bare zero-arg calls to imported thunks alongside an
        // exports write. These become inter-procedural dependencies —
        // the fixpoint resolves them; the value still composes.
        let body = "initOne(); initTwo(); module.exports = 42;";
        let result = classify_lazy_module_body(
            body,
            "exports",
            Some("module"),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        match result {
            LazyBodyClassification::EagerWithDeps { value, call_deps } => {
                // Thunk calls are NOT in the prologue — they're handled
                // by their producer's eagerification.
                assert_eq!(value, "42");
                assert!(call_deps.contains("initOne"));
                assert!(call_deps.contains("initTwo"));
                assert_eq!(call_deps.len(), 2);
            }
            other => panic!("expected EagerWithDeps, got {other:?}"),
        }
    }

    #[test]
    fn classify_lazy_body_treats_setter_calls_alongside_thunk_deps_correctly() {
        // Mix of setter calls (go into prologue, run at module load)
        // and zero-arg thunk calls (become deps, handled by their own
        // eagerification). The value composes the setters + the
        // captured exports write.
        let body = "thunkA(); __reverts_set_foo(1); thunkB(); module.exports = bar;";
        // module.exports = bar — `bar` is an identifier (not pure) so
        // captured_value rejects → Impure overall.
        let result = classify_lazy_module_body(
            body,
            "exports",
            Some("module"),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(result, LazyBodyClassification::Impure);
    }

    #[test]
    fn classify_lazy_body_thunk_only_init_returns_deps_with_empty_exports() {
        // No exports write, no return — just thunk calls. lazyModule
        // bodies yield `{}` (the wrapper's empty exports object).
        let body = "thunkA();\nthunkB();";
        let result = classify_lazy_module_body(
            body,
            "exports",
            Some("module"),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        match result {
            LazyBodyClassification::EagerWithDeps { value, call_deps } => {
                assert_eq!(value, "{}");
                assert_eq!(call_deps.len(), 2);
            }
            other => panic!("expected EagerWithDeps, got {other:?}"),
        }
    }

    #[test]
    fn classify_lazy_body_rejects_call_with_arguments_as_unknown_effect() {
        // `foo(1)` — call with an argument is NOT a zero-arg thunk
        // invocation; it could be calling a regular function with side
        // effects we can't classify. Stay impure.
        let body = "foo(1); module.exports = 42;";
        let result = classify_lazy_module_body(
            body,
            "exports",
            Some("module"),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(result, LazyBodyClassification::Impure);
    }

    #[test]
    fn classify_lazy_body_eager_when_body_has_no_calls() {
        let body = "module.exports = { a: 1, b: 2 };";
        let result = classify_lazy_module_body(
            body,
            "exports",
            Some("module"),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        match result {
            LazyBodyClassification::Eager { value } => {
                assert_eq!(value, "{ a: 1, b: 2 }");
            }
            other => panic!("expected Eager, got {other:?}"),
        }
    }

    #[test]
    fn classifies_multiple_bindings_independently() {
        let source = concat!(
            "import { eager, lazy } from './x.js';\n",
            "const result = eager;\n",
            "export function trigger() { return lazy(); }\n",
        );
        let scope = classify_import_usage_scope(
            source,
            &binding_set(&["eager", "lazy"]),
            Some(Path::new("entry.ts")),
            ParseGoal::TypeScript,
        );
        assert_eq!(scope.get("eager"), Some(&ImportUsageScope::TopLevel));
        assert_eq!(scope.get("lazy"), Some(&ImportUsageScope::NestedOnly));
    }

    #[test]
    fn lazy_value_sub_snippets_slices_arrow_body_statements() {
        let source = "var X = lazyValue(() => {\n\
                      \tvar a = 1;\n\
                      \tvar b = a + 1;\n\
                      \tfunction c() { return a; }\n\
                      });";

        let slices = lazy_value_sub_snippets(source, None, ParseGoal::TypeScript)
            .expect("recognised lazyValue shape");

        assert_eq!(slices.len(), 3, "{slices:?}");
        assert_eq!(slices[0].kind, TopLevelStatementKind::Variable);
        assert_eq!(slices[0].bindings, vec!["a".to_string()]);
        assert_eq!(slices[1].kind, TopLevelStatementKind::Variable);
        assert_eq!(slices[1].bindings, vec!["b".to_string()]);
        assert_eq!(slices[2].kind, TopLevelStatementKind::Function);
        assert_eq!(slices[2].bindings, vec!["c".to_string()]);
        assert!(slices[0].source.contains("var a = 1;"));
        assert!(slices[1].source.contains("var b = a + 1;"));
        assert!(slices[2].source.contains("function c()"));
    }

    #[test]
    fn lazy_value_sub_snippets_returns_none_for_non_lazy_shape() {
        assert!(
            lazy_value_sub_snippets("var X = 1;", None, ParseGoal::TypeScript).is_none(),
            "plain var declaration is not a lazyValue"
        );
        assert!(
            lazy_value_sub_snippets("var X = lazyValue(() => 42);", None, ParseGoal::TypeScript)
                .is_none(),
            "expression-only arrow body is not slice-able"
        );
        assert!(
            lazy_value_sub_snippets(
                "var X = otherFn(() => { var a = 1; });",
                None,
                ParseGoal::TypeScript
            )
            .is_none(),
            "non-lazyValue callee is not a lazy block"
        );
    }
}
