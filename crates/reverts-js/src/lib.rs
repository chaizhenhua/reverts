pub mod normalize;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::{
    AstBuilder, NONE, Visit, VisitMut,
    ast::{
        Argument, ArrowFunctionExpression, AssignmentExpression, BindingIdentifier,
        BindingPatternKind, CallExpression, Class, ClassType, Declaration, ExportAllDeclaration,
        ExportDefaultDeclarationKind, ExportNamedDeclaration, Expression, Function, FunctionType,
        IdentifierReference, ImportDeclaration, ImportDeclarationSpecifier, ImportExpression,
        ImportOrExportKind, ModuleExportName, NewExpression, ObjectExpression, ObjectProperty,
        ObjectPropertyKind, Program, PropertyKey, PropertyKind, SimpleAssignmentTarget, Statement,
        StaticMemberExpression, StringLiteral, UnaryExpression, VariableDeclarationKind,
        VariableDeclarator,
    },
    visit::walk::{
        walk_arrow_function_expression, walk_assignment_expression, walk_call_expression,
        walk_export_all_declaration, walk_export_named_declaration, walk_function,
        walk_import_declaration, walk_import_expression, walk_new_expression, walk_object_property,
        walk_static_member_expression, walk_string_literal, walk_unary_expression,
        walk_update_expression,
    },
    visit::walk_mut::{
        walk_expression as walk_expression_mut, walk_object_property as walk_object_property_mut,
    },
};
use oxc_codegen::{CodeGenerator, CodegenOptions};
use oxc_parser::{ParseOptions, Parser};
use oxc_semantic::SemanticBuilder;
use oxc_span::{GetSpan, SPAN, SourceType, Span};
use oxc_syntax::{
    operator::UnaryOperator, reference::ReferenceId, scope::ScopeFlags, symbol::SymbolId,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub source_type: String,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsError {
    ParseFailed(Vec<ParseError>),
}

pub type Result<T> = std::result::Result<T, JsError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseGoal {
    JavaScript,
    TypeScript,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedImport {
    pub namespace: String,
    pub specifier: String,
}

impl GeneratedImport {
    #[must_use]
    pub fn new(namespace: impl Into<String>, specifier: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            specifier: specifier.into(),
        }
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

    fn push(&mut self, entry: impl Into<String>) {
        self.entries.push(entry.into());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringLiteralFact {
    pub value: String,
    pub byte_start: u32,
    pub byte_end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocationRewriteFact {
    pub value: String,
    pub byte_start: u32,
    pub byte_end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathBuilderCallFact {
    pub string_arguments: Vec<String>,
    pub byte_start: u32,
    pub byte_end: u32,
}

#[must_use]
pub fn source_type_candidates(path_hint: Option<&Path>, goal: ParseGoal) -> Vec<SourceType> {
    let mut candidates = Vec::new();
    if let Some(path_hint) = path_hint
        && let Ok(source_type) = SourceType::from_path(path_hint)
    {
        push_unique(&mut candidates, source_type);
    }

    match goal {
        ParseGoal::JavaScript => {
            push_unique(&mut candidates, SourceType::mjs());
            push_unique(&mut candidates, SourceType::cjs());
            push_unique(&mut candidates, SourceType::jsx());
        }
        ParseGoal::TypeScript => {
            push_unique(&mut candidates, SourceType::tsx());
            push_unique(&mut candidates, SourceType::ts());
            push_unique(&mut candidates, SourceType::mjs().with_typescript(true));
            push_unique(&mut candidates, SourceType::mjs());
            push_unique(&mut candidates, SourceType::cjs());
            push_unique(&mut candidates, SourceType::jsx());
        }
    }

    candidates
}

fn push_unique(candidates: &mut Vec<SourceType>, source_type: SourceType) {
    if !candidates.contains(&source_type) {
        candidates.push(source_type);
    }
}

pub fn parse_source(source: &str, path_hint: Option<&Path>, goal: ParseGoal) -> Result<()> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if parsed.errors.is_empty() && !parsed.panicked {
            return Ok(());
        }
        errors.push(ParseError {
            source_type: format!("{source_type:?}"),
            diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
        });
    }

    Err(JsError::ParseFailed(errors))
}

pub fn collect_string_literals(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<Vec<StringLiteralFact>> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }

        let mut collector = StringLiteralCollector::default();
        collector.visit_program(&parsed.program);
        return Ok(collector.literals);
    }

    Err(JsError::ParseFailed(errors))
}

pub fn collect_static_resource_specifiers(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<Vec<StringLiteralFact>> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }

        let mut collector = StaticResourceSpecifierCollector::default();
        collector.visit_program(&parsed.program);
        return Ok(collector.specifiers);
    }

    Err(JsError::ParseFailed(errors))
}

pub fn collect_file_url_source_location_rewrites(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<Vec<SourceLocationRewriteFact>> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }

        let mut collector = FileUrlSourceLocationCollector::default();
        collector.visit_program(&parsed.program);
        return Ok(collector.rewrites);
    }

    Err(JsError::ParseFailed(errors))
}

pub fn collect_path_builder_calls(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<Vec<PathBuilderCallFact>> {
    let allocator = Allocator::default();
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }

        let mut collector = PathBuilderCallCollector::default();
        collector.visit_program(&parsed.program);
        return Ok(collector.calls);
    }

    Err(JsError::ParseFailed(errors))
}

#[derive(Debug, Default)]
struct StringLiteralCollector {
    literals: Vec<StringLiteralFact>,
}

impl<'a> Visit<'a> for StringLiteralCollector {
    fn visit_string_literal(&mut self, literal: &StringLiteral<'a>) {
        self.literals.push(StringLiteralFact {
            value: literal.value.as_str().to_string(),
            byte_start: literal.span.start,
            byte_end: literal.span.end,
        });
        walk_string_literal(self, literal);
    }
}

#[derive(Debug, Default)]
struct StaticResourceSpecifierCollector {
    specifiers: Vec<StringLiteralFact>,
}

impl<'a> Visit<'a> for StaticResourceSpecifierCollector {
    fn visit_import_declaration(&mut self, declaration: &ImportDeclaration<'a>) {
        self.push_literal(&declaration.source);
        walk_import_declaration(self, declaration);
    }

    fn visit_export_named_declaration(&mut self, declaration: &ExportNamedDeclaration<'a>) {
        if let Some(source) = declaration.source.as_ref() {
            self.push_literal(source);
        }
        walk_export_named_declaration(self, declaration);
    }

    fn visit_export_all_declaration(&mut self, declaration: &ExportAllDeclaration<'a>) {
        self.push_literal(&declaration.source);
        walk_export_all_declaration(self, declaration);
    }

    fn visit_import_expression(&mut self, expression: &ImportExpression<'a>) {
        if let Expression::StringLiteral(source) = &expression.source {
            self.push_literal(source);
        }
        walk_import_expression(self, expression);
    }

    fn visit_call_expression(&mut self, expression: &CallExpression<'a>) {
        if call_callee_accepts_static_resource(&expression.callee)
            && let Some(Argument::StringLiteral(source)) = expression.arguments.first()
        {
            self.push_literal(source);
        }
        walk_call_expression(self, expression);
    }

    fn visit_new_expression(&mut self, expression: &NewExpression<'a>) {
        if expression_identifier(&expression.callee) == Some("URL")
            && let Some(Argument::StringLiteral(source)) = expression.arguments.first()
        {
            self.push_literal(source);
        }
        walk_new_expression(self, expression);
    }
}

impl StaticResourceSpecifierCollector {
    fn push_literal(&mut self, literal: &StringLiteral<'_>) {
        self.specifiers.push(StringLiteralFact {
            value: literal.value.as_str().to_string(),
            byte_start: literal.span.start,
            byte_end: literal.span.end,
        });
    }
}

fn call_callee_accepts_static_resource(callee: &Expression<'_>) -> bool {
    if expression_identifier(callee) == Some("require") {
        return true;
    }

    let Expression::StaticMemberExpression(member) = callee else {
        return false;
    };
    let property = member.property.name.as_str();
    matches!(
        (expression_identifier(&member.object), property),
        (Some("require"), "resolve")
            | (Some("Bun"), "file")
            | (Some("fs"), "readFile")
            | (Some("fs"), "readFileSync")
            | (Some("fs"), "createReadStream")
    )
}

#[derive(Debug, Default)]
struct FileUrlSourceLocationCollector {
    rewrites: Vec<SourceLocationRewriteFact>,
}

impl<'a> Visit<'a> for FileUrlSourceLocationCollector {
    fn visit_call_expression(&mut self, expression: &CallExpression<'a>) {
        if call_callee_property_or_identifier(&expression.callee) == Some("fileURLToPath")
            && let Some(Argument::StringLiteral(source)) = expression.arguments.first()
            && is_file_url_source_location(source.value.as_str())
        {
            self.rewrites.push(SourceLocationRewriteFact {
                value: source.value.as_str().to_string(),
                byte_start: source.span.start,
                byte_end: source.span.end,
            });
        }
        walk_call_expression(self, expression);
    }
}

#[derive(Debug, Default)]
struct PathBuilderCallCollector {
    calls: Vec<PathBuilderCallFact>,
}

impl<'a> Visit<'a> for PathBuilderCallCollector {
    fn visit_call_expression(&mut self, expression: &CallExpression<'a>) {
        if matches!(
            call_callee_property_or_identifier(&expression.callee),
            Some("join" | "resolve")
        ) {
            let string_arguments = expression
                .arguments
                .iter()
                .filter_map(argument_string_literal_value)
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            if !string_arguments.is_empty() {
                self.calls.push(PathBuilderCallFact {
                    string_arguments,
                    byte_start: expression.span.start,
                    byte_end: expression.span.end,
                });
            }
        }
        walk_call_expression(self, expression);
    }
}

fn argument_string_literal_value<'a>(argument: &'a Argument<'a>) -> Option<&'a str> {
    match argument {
        Argument::StringLiteral(literal) => Some(literal.value.as_str()),
        _ => None,
    }
}

fn call_callee_property_or_identifier<'a>(callee: &'a Expression<'a>) -> Option<&'a str> {
    match callee {
        Expression::Identifier(identifier) => Some(identifier.name.as_str()),
        Expression::StaticMemberExpression(member) => Some(member.property.name.as_str()),
        _ => None,
    }
}

fn is_file_url_source_location(value: &str) -> bool {
    let Some(path) = value.strip_prefix("file://") else {
        return false;
    };
    let path = path.strip_prefix("localhost").unwrap_or(path);
    let extension = Path::new(path)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .map(str::to_ascii_lowercase);
    matches!(
        extension.as_deref(),
        Some("js" | "mjs" | "cjs" | "ts" | "mts" | "cts" | "jsx" | "tsx")
    )
}

fn expression_identifier<'a>(expression: &'a Expression<'a>) -> Option<&'a str> {
    match expression {
        Expression::Identifier(identifier) => Some(identifier.name.as_str()),
        _ => None,
    }
}

pub fn format_source_pretty(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<String> {
    let mut errors = Vec::new();

    for source_type in source_type_candidates(path_hint, goal) {
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            errors.push(ParseError {
                source_type: format!("{source_type:?}"),
                diagnostics: parsed.errors.iter().map(ToString::to_string).collect(),
            });
            continue;
        }

        let output = CodeGenerator::new()
            .with_options(CodegenOptions {
                single_quote: true,
                minify: false,
                ..Default::default()
            })
            .build(&parsed.program);
        return Ok(output.code);
    }

    Err(JsError::ParseFailed(errors))
}

/// Compiler-specific lowering applied when re-emitting a parsed module body.
/// Each variant names a recognised bundler/transpiler whose semantically
/// no-op artefacts can be stripped from the emitted output.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CompilerLowering {
    #[default]
    None,
    Babel,
    Esbuild,
    Webpack,
}

/// Canonical webpack runtime helper names. Webpack 5 emits these at the top
/// of its IIFE bootstrap; when `output.iife: false` the same definitions
/// appear at module scope where this strip applies directly. The IIFE-
/// wrapped case is left for a future slice that descends into the IIFE.
const WEBPACK_RUNTIME_HELPERS: &[&str] = &[
    "__webpack_require__",
    "__webpack_exports__",
    "__webpack_modules__",
    "__webpack_module_cache__",
    "webpackChunk",
    "webpackJsonp",
];

/// Canonical esbuild runtime helper names. Each is declared at the top of
/// esbuild bundles whether or not it is referenced; the lowering pass strips
/// the declaration once we can prove the helper is unused.
const ESBUILD_RUNTIME_HELPERS: &[&str] = &[
    "__commonJS",
    "__toCommonJS",
    "__defProp",
    "__defProps",
    "__export",
    "__copyProps",
    "__toESM",
    "__require",
    "__esm",
    "__getOwnPropDesc",
    "__getOwnPropNames",
    "__hasOwnProp",
];

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
        coalesce_simple_named_imports_in_program(&mut parsed.program, &builder);
        coalesce_compatible_mixed_imports_in_program(&mut parsed.program, &builder);
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

#[derive(Debug, Clone)]
struct NamespaceImportCandidate {
    local_name: String,
    local_symbol: SymbolId,
}

#[derive(Debug, Clone)]
struct NamespaceImportSplit {
    namespace: String,
    members: BTreeSet<String>,
    member_by_reference: BTreeMap<ReferenceId, String>,
}

fn split_safe_namespace_imports<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    report: &mut ReadabilityReport,
) {
    let splits = plan_safe_namespace_import_splits(program);
    if splits.is_empty() {
        return;
    }
    let builder = AstBuilder::new(allocator);
    let members_by_namespace = splits
        .iter()
        .map(|split| (split.namespace.clone(), split.members.clone()))
        .collect::<BTreeMap<_, _>>();
    for statement in program.body.iter_mut() {
        let Statement::ImportDeclaration(declaration) = statement else {
            continue;
        };
        let Some(namespace) = splittable_namespace_import_name(declaration) else {
            continue;
        };
        let Some(members) = members_by_namespace.get(&namespace) else {
            continue;
        };
        replace_namespace_import_with_named_specifiers(&builder, declaration, members);
        report.push(format!(
            "split namespace import {namespace} -> {{{}}}",
            members.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }

    let member_by_reference = splits
        .into_iter()
        .flat_map(|split| split.member_by_reference)
        .collect::<BTreeMap<_, _>>();
    let mut rewriter = NamespaceImportMemberRewriter {
        builder,
        member_by_reference,
    };
    rewriter.visit_program(program);
}

fn plan_safe_namespace_import_splits(program: &Program<'_>) -> Vec<NamespaceImportSplit> {
    let semantic = SemanticBuilder::new().build(program).semantic;
    let symbols = semantic.symbols();
    let root_scope_id = semantic.scopes().root_scope_id();
    let candidates = collect_namespace_import_candidates(program);
    if candidates.is_empty() {
        return Vec::new();
    }
    let candidate_symbols = candidates
        .iter()
        .map(|candidate| candidate.local_symbol)
        .collect::<BTreeSet<_>>();
    let mut occupied_names = symbols
        .symbol_ids()
        .filter(|symbol_id| {
            symbols.get_scope_id(*symbol_id) == root_scope_id
                && !candidate_symbols.contains(symbol_id)
        })
        .map(|symbol_id| symbols.get_name(symbol_id).to_string())
        .collect::<BTreeSet<_>>();
    occupied_names.extend(
        semantic
            .scopes()
            .root_unresolved_references()
            .keys()
            .map(|name| name.as_str().to_string()),
    );

    let mut reference_to_namespace = BTreeMap::<ReferenceId, String>::new();
    for candidate in &candidates {
        for reference_id in symbols.get_resolved_reference_ids(candidate.local_symbol) {
            reference_to_namespace.insert(*reference_id, candidate.local_name.clone());
        }
    }
    let usage = collect_namespace_import_usage(program, reference_to_namespace);
    let mut splits = Vec::new();
    for candidate in candidates {
        if usage.invalid_namespaces.contains(&candidate.local_name) {
            continue;
        }
        let expected_references = symbols
            .get_resolved_reference_ids(candidate.local_symbol)
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let member_by_reference = usage
            .member_by_reference
            .get(&candidate.local_name)
            .cloned()
            .unwrap_or_default();
        let observed_references = member_by_reference.keys().copied().collect::<BTreeSet<_>>();
        if expected_references.is_empty() || expected_references != observed_references {
            continue;
        }
        let members = member_by_reference
            .values()
            .cloned()
            .collect::<BTreeSet<_>>();
        if members.iter().any(|member| {
            member == "default"
                || sanitize_identifier(member) != *member
                || occupied_names.contains(member)
        }) {
            continue;
        }
        occupied_names.extend(members.iter().cloned());
        splits.push(NamespaceImportSplit {
            namespace: candidate.local_name,
            members,
            member_by_reference,
        });
    }
    splits
}

fn collect_namespace_import_candidates(program: &Program<'_>) -> Vec<NamespaceImportCandidate> {
    program
        .body
        .iter()
        .filter_map(|statement| {
            let Statement::ImportDeclaration(declaration) = statement else {
                return None;
            };
            if !import_is_splittable_namespace_only(declaration) {
                return None;
            }
            let specifiers = declaration.specifiers.as_ref()?;
            let ImportDeclarationSpecifier::ImportNamespaceSpecifier(specifier) = &specifiers[0]
            else {
                return None;
            };
            Some(NamespaceImportCandidate {
                local_name: specifier.local.name.as_str().to_string(),
                local_symbol: specifier.local.symbol_id.get()?,
            })
        })
        .collect()
}

#[derive(Default)]
struct NamespaceImportUsage {
    member_by_reference: BTreeMap<String, BTreeMap<ReferenceId, String>>,
    invalid_namespaces: BTreeSet<String>,
}

fn collect_namespace_import_usage(
    program: &Program<'_>,
    reference_to_namespace: BTreeMap<ReferenceId, String>,
) -> NamespaceImportUsage {
    let mut collector = NamespaceImportUsageCollector {
        reference_to_namespace,
        usage: NamespaceImportUsage::default(),
    };
    collector.visit_program(program);
    collector.usage
}

struct NamespaceImportUsageCollector {
    reference_to_namespace: BTreeMap<ReferenceId, String>,
    usage: NamespaceImportUsage,
}

impl NamespaceImportUsageCollector {
    fn namespace_for_reference(&self, reference_id: ReferenceId) -> Option<&str> {
        self.reference_to_namespace
            .get(&reference_id)
            .map(String::as_str)
    }

    fn invalidate_reference(&mut self, reference_id: ReferenceId) {
        if let Some(namespace) = self.namespace_for_reference(reference_id) {
            self.usage.invalid_namespaces.insert(namespace.to_string());
        }
    }
}

impl<'a> Visit<'a> for NamespaceImportUsageCollector {
    fn visit_assignment_expression(&mut self, expression: &AssignmentExpression<'a>) {
        if let Some(reference_id) =
            assignment_target_namespace_object_reference_id(&expression.left)
        {
            self.invalidate_reference(reference_id);
        }
        walk_assignment_expression(self, expression);
    }

    fn visit_update_expression(&mut self, expression: &oxc_ast::ast::UpdateExpression<'a>) {
        if let Some(reference_id) =
            simple_assignment_target_namespace_object_reference_id(&expression.argument)
        {
            self.invalidate_reference(reference_id);
        }
        walk_update_expression(self, expression);
    }

    fn visit_unary_expression(&mut self, expression: &UnaryExpression<'a>) {
        if expression.operator == UnaryOperator::Delete
            && let Some(reference_id) =
                expression_static_namespace_member_reference_id(&expression.argument)
        {
            self.invalidate_reference(reference_id);
        }
        walk_unary_expression(self, expression);
    }

    fn visit_static_member_expression(&mut self, member: &StaticMemberExpression<'a>) {
        let Some(reference_id) = expression_identifier_reference_id(&member.object) else {
            walk_static_member_expression(self, member);
            return;
        };
        if let Some(namespace) = self.namespace_for_reference(reference_id) {
            let property = member.property.name.as_str();
            if member.optional || property == "default" || sanitize_identifier(property) != property
            {
                self.usage.invalid_namespaces.insert(namespace.to_string());
            } else {
                self.usage
                    .member_by_reference
                    .entry(namespace.to_string())
                    .or_default()
                    .insert(reference_id, property.to_string());
            }
        }
        walk_static_member_expression(self, member);
    }
}

fn assignment_target_namespace_object_reference_id(
    target: &oxc_ast::ast::AssignmentTarget<'_>,
) -> Option<ReferenceId> {
    match target {
        oxc_ast::ast::AssignmentTarget::StaticMemberExpression(member) => {
            expression_identifier_reference_id(&member.object)
        }
        oxc_ast::ast::AssignmentTarget::ComputedMemberExpression(member) => {
            expression_identifier_reference_id(&member.object)
        }
        _ => None,
    }
}

fn simple_assignment_target_namespace_object_reference_id(
    target: &SimpleAssignmentTarget<'_>,
) -> Option<ReferenceId> {
    match target {
        SimpleAssignmentTarget::StaticMemberExpression(member) => {
            expression_identifier_reference_id(&member.object)
        }
        SimpleAssignmentTarget::ComputedMemberExpression(member) => {
            expression_identifier_reference_id(&member.object)
        }
        _ => None,
    }
}

fn expression_static_namespace_member_reference_id(
    expression: &Expression<'_>,
) -> Option<ReferenceId> {
    let Expression::StaticMemberExpression(member) = expression else {
        return None;
    };
    expression_identifier_reference_id(&member.object)
}

fn expression_identifier_reference_id(expression: &Expression<'_>) -> Option<ReferenceId> {
    let Expression::Identifier(identifier) = expression else {
        return None;
    };
    identifier.reference_id.get()
}

fn import_is_splittable_namespace_only(declaration: &ImportDeclaration<'_>) -> bool {
    if declaration.phase.is_some()
        || declaration.with_clause.is_some()
        || declaration.import_kind.is_type()
    {
        return false;
    }
    let Some(specifiers) = &declaration.specifiers else {
        return false;
    };
    specifiers.len() == 1
        && matches!(
            specifiers[0],
            ImportDeclarationSpecifier::ImportNamespaceSpecifier(_)
        )
}

fn splittable_namespace_import_name(declaration: &ImportDeclaration<'_>) -> Option<String> {
    if !import_is_splittable_namespace_only(declaration) {
        return None;
    }
    let specifiers = declaration.specifiers.as_ref()?;
    let ImportDeclarationSpecifier::ImportNamespaceSpecifier(specifier) = &specifiers[0] else {
        return None;
    };
    Some(specifier.local.name.as_str().to_string())
}

fn replace_namespace_import_with_named_specifiers<'a>(
    builder: &AstBuilder<'a>,
    declaration: &mut ImportDeclaration<'a>,
    members: &BTreeSet<String>,
) {
    let Some(specifiers) = &mut declaration.specifiers else {
        return;
    };
    specifiers.clear();
    for member in members {
        let imported = builder.module_export_name_identifier_name(SPAN, member.as_str());
        let local = builder.binding_identifier(SPAN, member.as_str());
        specifiers.push(builder.import_declaration_specifier_import_specifier(
            SPAN,
            imported,
            local,
            ImportOrExportKind::Value,
        ));
    }
}

struct NamespaceImportMemberRewriter<'a> {
    builder: AstBuilder<'a>,
    member_by_reference: BTreeMap<ReferenceId, String>,
}

impl<'a> VisitMut<'a> for NamespaceImportMemberRewriter<'a> {
    fn visit_expression(&mut self, expression: &mut Expression<'a>) {
        let replacement = match expression {
            Expression::StaticMemberExpression(member) if !member.optional => {
                expression_identifier_reference_id(&member.object)
                    .and_then(|reference_id| self.member_by_reference.get(&reference_id).cloned())
            }
            _ => None,
        };
        if let Some(replacement) = replacement {
            *expression = self
                .builder
                .expression_identifier_reference(SPAN, replacement.as_str());
            return;
        }
        walk_expression_mut(self, expression);
    }
}

fn merge_and_sort_named_imports<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    report: &mut ReadabilityReport,
) {
    let builder = AstBuilder::new(allocator);
    let mut next = builder.vec();
    let mut first_by_source = BTreeMap::<String, usize>::new();
    for statement in program.body.drain(..) {
        if let Statement::ImportDeclaration(mut declaration) = statement {
            if import_is_mergeable_value_import(&declaration) {
                let source = declaration.source.value.as_str().to_string();
                if let Some(first_index) = first_by_source.get(&source).copied() {
                    if let Statement::ImportDeclaration(first) = &next[first_index]
                        && can_merge_import_declarations(first, &declaration)
                        && let Statement::ImportDeclaration(first) = &mut next[first_index]
                        && let (Some(first_specifiers), Some(specifiers)) =
                            (&mut first.specifiers, &mut declaration.specifiers)
                    {
                        first_specifiers.extend(specifiers.drain(..));
                        sort_and_dedup_import_specifiers(first_specifiers);
                        report.push(format!("merged imports from {source}"));
                        continue;
                    }
                }
                sort_and_dedup_import_specifiers(
                    declaration
                        .specifiers
                        .as_mut()
                        .expect("mergeable import has specifiers"),
                );
                let index = next.len();
                first_by_source.insert(source, index);
                next.push(Statement::ImportDeclaration(declaration));
                continue;
            }
            next.push(Statement::ImportDeclaration(declaration));
        } else {
            next.push(statement);
        }
    }
    program.body = next;
}

fn import_is_mergeable_value_import(declaration: &ImportDeclaration<'_>) -> bool {
    if declaration.phase.is_some()
        || declaration.with_clause.is_some()
        || declaration.import_kind.is_type()
    {
        return false;
    }
    let Some(specifiers) = &declaration.specifiers else {
        return false;
    };
    !specifiers.is_empty()
        && specifiers.iter().all(|specifier| {
            matches!(
                specifier,
                ImportDeclarationSpecifier::ImportSpecifier(_)
                    | ImportDeclarationSpecifier::ImportDefaultSpecifier(_)
            )
        })
}

fn can_merge_import_declarations(
    first: &ImportDeclaration<'_>,
    next: &ImportDeclaration<'_>,
) -> bool {
    !(import_has_default_specifier(first) && import_has_default_specifier(next))
}

fn import_has_default_specifier(declaration: &ImportDeclaration<'_>) -> bool {
    declaration.specifiers.as_ref().is_some_and(|specifiers| {
        specifiers.iter().any(|specifier| {
            matches!(
                specifier,
                ImportDeclarationSpecifier::ImportDefaultSpecifier(_)
            )
        })
    })
}

fn sort_and_dedup_import_specifiers(
    specifiers: &mut oxc_allocator::Vec<'_, ImportDeclarationSpecifier<'_>>,
) {
    specifiers.sort_by(|left, right| {
        import_specifier_sort_key(left).cmp(&import_specifier_sort_key(right))
    });
    let mut seen = BTreeSet::<(String, String)>::new();
    specifiers.retain(|specifier| {
        let key = import_specifier_sort_key(specifier);
        seen.insert(key)
    });
}

fn import_specifier_sort_key(specifier: &ImportDeclarationSpecifier<'_>) -> (String, String) {
    match specifier {
        ImportDeclarationSpecifier::ImportDefaultSpecifier(specifier) => {
            (String::new(), specifier.local.name.as_str().to_string())
        }
        ImportDeclarationSpecifier::ImportSpecifier(specifier) => (
            module_export_sort_name(&specifier.imported),
            specifier.local.name.as_str().to_string(),
        ),
        ImportDeclarationSpecifier::ImportNamespaceSpecifier(specifier) => {
            ("*".to_string(), specifier.local.name.as_str().to_string())
        }
    }
}

fn module_export_sort_name(name: &ModuleExportName<'_>) -> String {
    match name {
        ModuleExportName::IdentifierName(identifier) => identifier.name.as_str().to_string(),
        ModuleExportName::IdentifierReference(identifier) => identifier.name.as_str().to_string(),
        ModuleExportName::StringLiteral(literal) => literal.value.as_str().to_string(),
    }
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

/// Catalog of Babel CJS interop helpers we know how to lower. Each entry
/// names the helper, the legal argument-count window for its call site, and
/// the source-level replacement template used when rewriting a
/// `var X = HELPER(require("Y"))` declarator init.
struct BabelInteropHelper {
    name: &'static str,
    min_call_args: usize,
    max_call_args: usize,
    replacement: fn(&str) -> String,
}

fn replace_with_default_wrapped_require(require_arg: &str) -> String {
    format!("{{ default: require({require_arg}) }}")
}

fn replace_with_bare_require(require_arg: &str) -> String {
    format!("require({require_arg})")
}

const BABEL_INTEROP_HELPERS: &[BabelInteropHelper] = &[
    BabelInteropHelper {
        name: "_interopRequireDefault",
        min_call_args: 1,
        max_call_args: 1,
        replacement: replace_with_default_wrapped_require,
    },
    BabelInteropHelper {
        name: "_interopRequireWildcard",
        min_call_args: 1,
        max_call_args: 2,
        replacement: replace_with_bare_require,
    },
];

/// Apply compiler-specific source-level rewrites before the main format pass.
/// Currently handles Babel's `_interopRequireDefault(require("X"))` and
/// `_interopRequireWildcard(require("X"))` patterns: the helper call is
/// dropped, leaving a literal `{ default: require(...) }` or a bare
/// `require(...)` respectively. Both keep downstream `.<member>` access
/// valid.
fn apply_source_level_lowerings(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
    lowering: CompilerLowering,
) -> String {
    if !matches!(lowering, CompilerLowering::Babel) {
        return source.to_string();
    }
    let allocator = Allocator::default();
    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            continue;
        }
        let mut replacements = Vec::<(u32, u32, String)>::new();
        for statement in &parsed.program.body {
            let Statement::VariableDeclaration(declaration) = statement else {
                continue;
            };
            for declarator in &declaration.declarations {
                let Some(init) = declarator.init.as_ref() else {
                    continue;
                };
                for helper in BABEL_INTEROP_HELPERS {
                    let Some(arg_span) = babel_interop_helper_arg_span(init, helper) else {
                        continue;
                    };
                    let init_span = init.span();
                    let arg_text = &source[arg_span.start as usize..arg_span.end as usize];
                    replacements.push((
                        init_span.start,
                        init_span.end,
                        (helper.replacement)(arg_text),
                    ));
                    break;
                }
            }
        }
        if replacements.is_empty() {
            return source.to_string();
        }
        replacements.sort_by_key(|edit| edit.0);
        let mut output = source.to_string();
        for (start, end, new_text) in replacements.iter().rev() {
            output.replace_range(*start as usize..*end as usize, new_text);
        }
        return output;
    }
    // No source type parsed cleanly. Defer the parse-failure surface to the
    // downstream format pass.
    source.to_string()
}

/// Recognise the canonical Babel interop wrapper `HELPER(require("X"))` for
/// the supplied `helper` entry. Returns the span of the inner string-literal
/// specifier so the caller can re-use the original quoting and escaping.
fn babel_interop_helper_arg_span(
    expression: &Expression<'_>,
    helper: &BabelInteropHelper,
) -> Option<Span> {
    let Expression::CallExpression(outer) = expression else {
        return None;
    };
    let Expression::Identifier(callee) = &outer.callee else {
        return None;
    };
    if callee.name.as_str() != helper.name
        || outer.arguments.len() < helper.min_call_args
        || outer.arguments.len() > helper.max_call_args
    {
        return None;
    }
    let Argument::CallExpression(inner) = &outer.arguments[0] else {
        return None;
    };
    let Expression::Identifier(inner_callee) = &inner.callee else {
        return None;
    };
    if inner_callee.name.as_str() != "require" || inner.arguments.len() != 1 {
        return None;
    }
    let Argument::StringLiteral(specifier) = &inner.arguments[0] else {
        return None;
    };
    Some(specifier.span())
}

/// Returns true if any expression in the program refers to `name` as an
/// identifier (i.e. uses it as a value). Function declarations' own
/// `BindingIdentifier` does NOT show up as an `IdentifierReference`, so when
/// the count is zero the matching helper declaration is genuinely dead.
fn program_references_named_identifier(program: &Program<'_>, name: &str) -> bool {
    let mut counter = NamedIdentifierReferenceCounter {
        target: name,
        count: 0,
    };
    counter.visit_program(program);
    counter.count > 0
}

struct NamedIdentifierReferenceCounter<'name> {
    target: &'name str,
    count: usize,
}

impl<'a, 'name> Visit<'a> for NamedIdentifierReferenceCounter<'name> {
    fn visit_identifier_reference(&mut self, identifier: &IdentifierReference<'a>) {
        if identifier.name.as_str() == self.target {
            self.count += 1;
        }
    }
}

/// Recognise a Babel interop helper declaration that we can strip once dead.
/// The strict-name + non-empty-params + single-return-statement check stays
/// narrow enough that a user-written function with the same name but a
/// different shape is left alone.
fn is_babel_interop_helper_definition(
    statement: &Statement<'_>,
    helper: &BabelInteropHelper,
) -> bool {
    let Statement::FunctionDeclaration(function) = statement else {
        return false;
    };
    let Some(name) = function.id.as_ref() else {
        return false;
    };
    if name.name.as_str() != helper.name {
        return false;
    }
    if function.params.items.is_empty() {
        return false;
    }
    let Some(body) = function.body.as_ref() else {
        return false;
    };
    body.statements.len() == 1 && matches!(body.statements[0], Statement::ReturnStatement(_))
}

/// Recognise a top-level `var <name> = ...;` declaration (one declarator)
/// whose binding identifier matches `target_name`. Used by the esbuild
/// helper-strip pass — the helper bodies are too varied to match
/// structurally, so we identify them solely by their canonical names.
fn is_top_level_var_declaration_named(statement: &Statement<'_>, target_name: &str) -> bool {
    let Statement::VariableDeclaration(declaration) = statement else {
        return false;
    };
    if declaration.declarations.len() != 1 {
        return false;
    }
    let declarator = &declaration.declarations[0];
    let BindingPatternKind::BindingIdentifier(identifier) = &declarator.id.kind else {
        return false;
    };
    identifier.name.as_str() == target_name && declarator.init.is_some()
}

/// Recognise a top-level `function NAME(...)` declaration whose name matches
/// `target_name`. Used by the webpack helper-strip pass for helpers that
/// webpack emits as function declarations rather than var initializers
/// (e.g. `function __webpack_require__(id) { ... }`).
fn is_top_level_function_declaration_named(statement: &Statement<'_>, target_name: &str) -> bool {
    let Statement::FunctionDeclaration(function) = statement else {
        return false;
    };
    function
        .id
        .as_ref()
        .is_some_and(|id| id.name.as_str() == target_name)
}

/// Strip `var <name> = ...` declarations matching `helper_names` from both
/// the program top-level and the body of any top-level IIFE. Real esbuild
/// bundles wrap the runtime in `(() => { ... })()`, so module-scope strip
/// alone never fires on them; descending into the IIFE handles that case.
fn strip_named_var_declarations_in_program(program: &mut Program<'_>, helper_names: &[&str]) {
    program.body.retain(|statement| {
        !helper_names
            .iter()
            .any(|name| is_top_level_var_declaration_named(statement, name))
    });
    for statement in program.body.iter_mut() {
        let Some(iife_body) = top_level_iife_body_statements_mut(statement) else {
            continue;
        };
        iife_body.retain(|statement| {
            !helper_names
                .iter()
                .any(|name| is_top_level_var_declaration_named(statement, name))
        });
    }
}

/// Strip `var <name> = ...` AND `function <name>(...) { ... }` declarations
/// matching `helper_names` from program top-level and any top-level IIFE
/// body. Webpack helpers come in both shapes (`__webpack_modules__` is a var,
/// `__webpack_require__` is a function declaration).
fn strip_named_declarations_in_program(program: &mut Program<'_>, helper_names: &[&str]) {
    let matches_helper = |statement: &Statement<'_>| {
        helper_names.iter().any(|name| {
            is_top_level_var_declaration_named(statement, name)
                || is_top_level_function_declaration_named(statement, name)
        })
    };
    program.body.retain(|statement| !matches_helper(statement));
    for statement in program.body.iter_mut() {
        let Some(iife_body) = top_level_iife_body_statements_mut(statement) else {
            continue;
        };
        iife_body.retain(|statement| !matches_helper(statement));
    }
}

/// Return a mutable reference to the body statements of a top-level IIFE
/// expressed as `(() => { ... })()` or `(function () { ... })()`. The IIFE
/// must take no arguments — the arg-bearing webpack module-table form
/// (`(function(modules){...})([...])`) is intentionally excluded so we never
/// strip helpers from a parameterised module table.
fn top_level_iife_body_statements_mut<'a, 'p>(
    statement: &'p mut Statement<'a>,
) -> Option<&'p mut oxc_allocator::Vec<'a, Statement<'a>>> {
    let Statement::ExpressionStatement(expression_statement) = statement else {
        return None;
    };
    let Expression::CallExpression(call) = &mut expression_statement.expression else {
        return None;
    };
    if !call.arguments.is_empty() {
        return None;
    }
    let callee = unwrap_parenthesized_mut(&mut call.callee);
    match callee {
        Expression::ArrowFunctionExpression(arrow) => Some(&mut arrow.body.statements),
        Expression::FunctionExpression(function) => {
            function.body.as_mut().map(|body| &mut body.statements)
        }
        _ => None,
    }
}

fn unwrap_parenthesized_mut<'a, 'p>(
    mut expression: &'p mut Expression<'a>,
) -> &'p mut Expression<'a> {
    while let Expression::ParenthesizedExpression(parenthesized) = expression {
        expression = &mut parenthesized.expression;
    }
    expression
}

/// Strip webpack's `__webpack_require__.r(exports)` make-namespace marker
/// from program top-level and any top-level IIFE body. The call sets
/// `__esModule` on its argument; in an ESM emit context it is at best a
/// no-op and at worst a runtime ReferenceError, so dropping it always
/// improves output.
fn strip_webpack_make_namespace_markers_in_program(program: &mut Program<'_>) {
    program
        .body
        .retain(|statement| !is_webpack_make_namespace_marker(statement));
    for statement in program.body.iter_mut() {
        let Some(iife_body) = top_level_iife_body_statements_mut(statement) else {
            continue;
        };
        iife_body.retain(|statement| !is_webpack_make_namespace_marker(statement));
    }
}

/// Match a top-level `__webpack_require__.r(<single-arg>)` expression
/// statement. The argument may be any identifier (`exports`,
/// `__webpack_exports__`, etc.) since webpack picks the binding name from
/// the wrapping module function's parameter list.
fn is_webpack_make_namespace_marker(statement: &Statement<'_>) -> bool {
    let Statement::ExpressionStatement(statement) = statement else {
        return false;
    };
    let Expression::CallExpression(call) = &statement.expression else {
        return false;
    };
    let Expression::StaticMemberExpression(callee) = &call.callee else {
        return false;
    };
    let Expression::Identifier(callee_object) = &callee.object else {
        return false;
    };
    callee_object.name.as_str() == "__webpack_require__"
        && callee.property.name.as_str() == "r"
        && call.arguments.len() == 1
        && matches!(call.arguments[0], Argument::Identifier(_))
}

/// Recognise the canonical Babel CJS-to-ESM marker statement:
/// `Object.defineProperty(exports, "__esModule", { value: true });`. Stripping
/// it from emitted output is safe in an ES module context, where `exports` is
/// not bound and the assignment would otherwise be a runtime error.
fn is_babel_es_module_marker(statement: &Statement<'_>) -> bool {
    let Statement::ExpressionStatement(statement) = statement else {
        return false;
    };
    let Expression::CallExpression(call) = &statement.expression else {
        return false;
    };
    let Expression::StaticMemberExpression(callee) = &call.callee else {
        return false;
    };
    let Expression::Identifier(callee_object) = &callee.object else {
        return false;
    };
    if callee_object.name.as_str() != "Object" || callee.property.name.as_str() != "defineProperty"
    {
        return false;
    }
    if call.arguments.len() != 3 {
        return false;
    }
    let Argument::Identifier(target) = &call.arguments[0] else {
        return false;
    };
    if target.name.as_str() != "exports" {
        return false;
    }
    let Argument::StringLiteral(key) = &call.arguments[1] else {
        return false;
    };
    key.value.as_str() == "__esModule"
}

#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclarationCallability {
    Callable,
    NotCallable,
    Unknown,
}

#[must_use]
pub fn classify_top_level_bindings(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> BTreeMap<String, DeclarationCallability> {
    let mut classifications = BTreeMap::new();
    let allocator = Allocator::default();

    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            continue;
        }
        for statement in &parsed.program.body {
            classify_statement(statement, &mut classifications);
        }
        return classifications;
    }

    classifications
}

fn classify_statement(
    statement: &Statement<'_>,
    classifications: &mut BTreeMap<String, DeclarationCallability>,
) {
    match statement {
        Statement::FunctionDeclaration(function) => {
            if let Some(name) = function.id.as_ref().map(|id| id.name.as_str()) {
                classifications.insert(name.to_string(), DeclarationCallability::Callable);
            }
        }
        Statement::ClassDeclaration(class) => {
            if let Some(name) = class.id.as_ref().map(|id| id.name.as_str()) {
                classifications.insert(name.to_string(), DeclarationCallability::Callable);
            }
        }
        Statement::VariableDeclaration(declaration) => {
            for declarator in &declaration.declarations {
                classify_declarator(declarator, classifications);
            }
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(Declaration::FunctionDeclaration(function)) = &export.declaration
                && let Some(name) = function.id.as_ref().map(|id| id.name.as_str())
            {
                classifications.insert(name.to_string(), DeclarationCallability::Callable);
            }
            if let Some(Declaration::ClassDeclaration(class)) = &export.declaration
                && let Some(name) = class.id.as_ref().map(|id| id.name.as_str())
            {
                classifications.insert(name.to_string(), DeclarationCallability::Callable);
            }
            if let Some(Declaration::VariableDeclaration(declaration)) = &export.declaration {
                for declarator in &declaration.declarations {
                    classify_declarator(declarator, classifications);
                }
            }
        }
        Statement::ExportDefaultDeclaration(export) => match &export.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(function) => {
                if let Some(name) = function.id.as_ref().map(|id| id.name.as_str()) {
                    classifications.insert(name.to_string(), DeclarationCallability::Callable);
                }
            }
            ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                if let Some(name) = class.id.as_ref().map(|id| id.name.as_str()) {
                    classifications.insert(name.to_string(), DeclarationCallability::Callable);
                }
            }
            _ => {}
        },
        _ => {}
    }
}

fn classify_declarator(
    declarator: &VariableDeclarator<'_>,
    classifications: &mut BTreeMap<String, DeclarationCallability>,
) {
    let BindingPatternKind::BindingIdentifier(identifier) = &declarator.id.kind else {
        return;
    };
    let name = identifier.name.as_str().to_string();
    let callability = match declarator.init.as_ref() {
        Some(Expression::FunctionExpression(_)) | Some(Expression::ArrowFunctionExpression(_)) => {
            DeclarationCallability::Callable
        }
        Some(Expression::ClassExpression(_)) => DeclarationCallability::Callable,
        Some(
            Expression::NumericLiteral(_)
            | Expression::StringLiteral(_)
            | Expression::BooleanLiteral(_)
            | Expression::NullLiteral(_)
            | Expression::ObjectExpression(_)
            | Expression::ArrayExpression(_)
            | Expression::TemplateLiteral(_),
        ) => DeclarationCallability::NotCallable,
        Some(_) | None => DeclarationCallability::Unknown,
    };
    classifications.insert(name, callability);
}

/// Where in a module's source a binding is observed.
///
/// `TopLevel` — the reference participates in module evaluation order:
/// it appears in a statement that runs when the module is loaded
/// (top-level statement, expression inside a top-level statement,
/// class field initializer, class `static {}` block).
///
/// `NestedOnly` — the reference appears only inside function or arrow
/// bodies. It runs whenever the function is invoked, not when the module
/// is loaded; the import is effectively "lazy" from the consumer's
/// perspective and doesn't constrain the module-eval DAG.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportUsageScope {
    TopLevel,
    NestedOnly,
}

/// Classify every binding in `binding_names` by where it's referenced
/// in `source`. Returns one entry per requested binding (even if the
/// binding is never seen — those map to `NestedOnly` as a vacuously
/// safe default: zero top-level refs means zero constraints).
///
/// Use this to decide whether eliminating a lazy thunk that exports a
/// given binding is safe from a module-eval-order perspective: if every
/// consumer's usage is `NestedOnly`, eagerifying the binding cannot
/// reorder evaluation visibly.
#[must_use]
pub fn classify_import_usage_scope(
    source: &str,
    binding_names: &std::collections::BTreeSet<String>,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> BTreeMap<String, ImportUsageScope> {
    let mut out: BTreeMap<String, ImportUsageScope> = binding_names
        .iter()
        .map(|name| (name.clone(), ImportUsageScope::NestedOnly))
        .collect();
    if binding_names.is_empty() {
        return out;
    }
    let allocator = Allocator::default();
    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            continue;
        }
        let mut collector = ImportUsageScopeCollector {
            fn_depth: 0,
            targets: binding_names,
            out: &mut out,
        };
        collector.visit_program(&parsed.program);
        return out;
    }
    out
}

struct ImportUsageScopeCollector<'a> {
    /// Number of enclosing function / arrow bodies. Class bodies do
    /// NOT increment this — static-block code and class-field
    /// initializers execute at class declaration time, which is
    /// module-load time when the class itself is declared at the top
    /// level. Methods carry their own `visit_function` frame.
    fn_depth: u32,
    targets: &'a std::collections::BTreeSet<String>,
    out: &'a mut BTreeMap<String, ImportUsageScope>,
}

impl<'a> Visit<'a> for ImportUsageScopeCollector<'_> {
    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        self.fn_depth += 1;
        walk_function(self, it, flags);
        self.fn_depth -= 1;
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        self.fn_depth += 1;
        walk_arrow_function_expression(self, it);
        self.fn_depth -= 1;
    }

    fn visit_identifier_reference(&mut self, identifier: &IdentifierReference<'a>) {
        if self.fn_depth > 0 {
            return;
        }
        let name = identifier.name.as_str();
        if self.targets.contains(name) {
            self.out
                .insert(name.to_string(), ImportUsageScope::TopLevel);
        }
    }
}

/// Whether every reference to a named binding in `source` is a
/// zero-argument call (`X()`) — i.e. the call-site shape that can be
/// mechanically rewritten to a bare reference when the binding is
/// delazified into a direct value.
///
/// Cases that count as "rewritable":
///   * `X()` — counted as `call_count` AND `total_count`.
///
/// Cases that count as `total_count` but NOT `call_count`, making the
/// binding non-rewritable:
///   * `X` as a value (passed to a function, stored, returned, used in
///     `typeof X`, etc.).
///   * `X(args)` — the consumer expects to call the binding with
///     arguments; the binding is being used as a callable value
///     directly, not as a zero-arg thunk.
///   * `X()()`, `X().foo` — chained access on the call result still
///     counts the outer `X()` correctly (call_count++) but only when
///     the inner call has no args.
///
/// Property-key uses (`{ X: 1 }`, `obj.X`) and module-import specifier
/// uses (`import { X } from ...`) are not classified as IdentifierReferences
/// by the AST and so don't appear in either count.
///
/// Returns one entry per requested binding. A binding with zero
/// references is `true` (vacuously safe — no consumer to break).
#[must_use]
pub fn verify_only_immediate_call_references(
    source: &str,
    binding_names: &std::collections::BTreeSet<String>,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> BTreeMap<String, bool> {
    let mut out: BTreeMap<String, bool> = binding_names
        .iter()
        .map(|name| (name.clone(), true))
        .collect();
    if binding_names.is_empty() {
        return out;
    }
    let allocator = Allocator::default();
    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            continue;
        }
        let mut collector = CallFormCollector {
            targets: binding_names,
            call_count: BTreeMap::new(),
            total_count: BTreeMap::new(),
        };
        collector.visit_program(&parsed.program);
        for name in binding_names {
            let total = collector.total_count.get(name).copied().unwrap_or(0);
            let calls = collector.call_count.get(name).copied().unwrap_or(0);
            out.insert(name.clone(), total == 0 || total == calls);
        }
        return out;
    }
    out
}

struct CallFormCollector<'a> {
    targets: &'a std::collections::BTreeSet<String>,
    call_count: BTreeMap<String, u32>,
    total_count: BTreeMap<String, u32>,
}

impl<'a> Visit<'a> for CallFormCollector<'_> {
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if call.arguments.is_empty()
            && let Expression::Identifier(callee) = &call.callee
            && self.targets.contains(callee.name.as_str())
        {
            *self.call_count.entry(callee.name.to_string()).or_insert(0) += 1;
        }
        walk_call_expression(self, call);
    }

    fn visit_identifier_reference(&mut self, identifier: &IdentifierReference<'a>) {
        let name = identifier.name.as_str();
        if self.targets.contains(name) {
            *self.total_count.entry(name.to_string()).or_insert(0) += 1;
        }
    }
}

/// AST-level body classifier for `lazyModule((exports, module) => { BODY })`
/// and `lazyValue(() => { BODY })` wrappers. Returns the source text of an
/// eager-safe value when the body matches a recognized shape (possibly
/// nested through `(function(){...}).call(...)` / `(()=>{...})()` IIFE
/// wrappers, and tolerating harmless `var`/`let`/`const` declarations
/// alongside the actual exports write):
///   * `module.exports = PURE_EXPR`
///   * `module.exports = A = B = PURE_EXPR` (chain — rightmost pure wins)
///   * `exports.k = PURE_EXPR_k` series → collapsed to `{ k1: v1, ... }`
///   * `Object.defineProperty(exports, "k", { value: PURE_EXPR })`
///   * `return PURE_EXPR;` (for `lazyValue` bodies)
///
/// Returns `None` for any unrecognized statement OR when the body has
/// thunk-call dependencies that need inter-procedural fixpoint
/// resolution. The richer [`classify_lazy_module_body`] is the
/// recommended entry point — callers that need the deps for fixpoint
/// propagation use it; callers that want a value-or-nothing answer
/// use this wrapper.
#[must_use]
pub fn extract_lazy_module_eager_value(
    body: &str,
    exports_param: &str,
    module_param: Option<&str>,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Option<String> {
    match classify_lazy_module_body(body, exports_param, module_param, path_hint, goal) {
        LazyBodyClassification::Eager { value } => Some(value),
        _ => None,
    }
}

/// Outcome of analyzing a lazy thunk body. The eagerification pipeline
/// uses this to decide whether the body can be inlined at module load,
/// and — when it depends on other thunks — what those dependencies are
/// so an inter-procedural fixpoint can resolve them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LazyBodyClassification {
    /// Body is mechanically eagerifiable with no calls into other
    /// lazy thunks. The `value` is the source-text to use as the
    /// replacement RHS (already includes any setter-call prologue
    /// folded into a comma expression).
    Eager { value: String },
    /// Body would be eagerifiable IF every name in `call_deps` resolves
    /// to a thunk that is itself eager-safe. The producer's fixpoint
    /// validates this by recursive propagation. The `value` is the
    /// composed replacement RHS assuming all deps clear; thunk calls
    /// like `dep()` are intentionally NOT in the prologue, because
    /// eagerifying each dep makes its side effects run at the dep
    /// module's load time (earlier than the consumer's), so re-calling
    /// would be redundant or unsafe.
    EagerWithDeps {
        value: String,
        call_deps: BTreeSet<String>,
    },
    /// Body has unrecognized side effects. Cannot be eagerified
    /// regardless of caller's eagerness.
    Impure,
}

/// Same as [`extract_lazy_module_eager_value`] but also accepts bodies
/// whose dependencies are all in `eager_safe_call_targets`. Returns the
/// composed value (with the dep calls already DROPPED from the prologue
/// — the eagerified producers will have already run by module-load time).
#[must_use]
pub fn extract_lazy_module_eager_value_with_safe_deps(
    body: &str,
    exports_param: &str,
    module_param: Option<&str>,
    path_hint: Option<&Path>,
    goal: ParseGoal,
    eager_safe_call_targets: &BTreeSet<String>,
) -> Option<String> {
    match classify_lazy_module_body(body, exports_param, module_param, path_hint, goal) {
        LazyBodyClassification::Eager { value } => Some(value),
        LazyBodyClassification::EagerWithDeps { value, call_deps } => {
            if call_deps
                .iter()
                .all(|d| eager_safe_call_targets.contains(d))
            {
                Some(value)
            } else {
                None
            }
        }
        LazyBodyClassification::Impure => None,
    }
}

/// Inter-procedural-friendly body classifier. Same shape recognition as
/// [`extract_lazy_module_eager_value`] but also reports zero-arg calls
/// to bare identifiers as dependencies for a global fixpoint to resolve.
/// The fixpoint determines whether each dependency identifier maps to a
/// thunk that is itself eager-safe; if so, the body is eager-safe.
#[must_use]
pub fn classify_lazy_module_body(
    body: &str,
    exports_param: &str,
    module_param: Option<&str>,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> LazyBodyClassification {
    let allocator = Allocator::default();
    let wrapped = format!("function __lazy_body_classifier_wrapper() {{\n{body}\n}}");
    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, &wrapped, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            continue;
        }
        let Some(function_body) = parsed.program.body.first().and_then(|stmt| match stmt {
            Statement::FunctionDeclaration(function) => function.body.as_deref(),
            _ => None,
        }) else {
            continue;
        };
        let analysis = analyze_lazy_body_statements_v2(
            &function_body.statements,
            &wrapped,
            exports_param,
            module_param,
        );
        return analysis_to_classification(analysis, module_param);
    }
    LazyBodyClassification::Impure
}

/// Internal mutable state collected during AST traversal of a lazy
/// body. The v2 analyzer fills this in; `analysis_to_classification`
/// converts it to a `LazyBodyClassification` based on whether
/// dependencies were collected.
#[derive(Debug, Default)]
struct LazyBodyAnalysisState {
    captured_value: Option<String>,
    property_writes: BTreeMap<String, String>,
    prologue: Vec<String>,
    call_deps: BTreeSet<String>,
    impure: bool,
}

fn analyze_lazy_body_statements_v2(
    statements: &oxc_allocator::Vec<'_, Statement<'_>>,
    source: &str,
    exports_param: &str,
    module_param: Option<&str>,
) -> LazyBodyAnalysisState {
    let mut state = LazyBodyAnalysisState::default();
    for stmt in statements {
        if state.impure {
            break;
        }
        match stmt {
            Statement::VariableDeclaration(decl) => {
                if !is_harmless_variable_declaration(decl, source) {
                    state.impure = true;
                }
            }
            Statement::EmptyStatement(_) => {}
            Statement::ExpressionStatement(expr_stmt) => {
                let mut chain = &expr_stmt.expression;
                while let Expression::ParenthesizedExpression(inner) = chain {
                    chain = &inner.expression;
                }
                if let Some(inner_body) = iife_block_body(chain) {
                    let inner_state = analyze_lazy_body_statements_v2(
                        &inner_body.statements,
                        source,
                        exports_param,
                        module_param,
                    );
                    if inner_state.impure {
                        state.impure = true;
                        continue;
                    }
                    if state.captured_value.is_some() || !state.property_writes.is_empty() {
                        // Another exports write or value already captured —
                        // having two values from different statements is
                        // ambiguous. Refuse.
                        state.impure = true;
                        continue;
                    }
                    // Merge inner state into outer.
                    state.prologue.extend(inner_state.prologue);
                    state.call_deps.extend(inner_state.call_deps);
                    if let Some(value) = inner_state.captured_value {
                        state.captured_value = Some(value);
                    } else if !inner_state.property_writes.is_empty() {
                        state.property_writes.extend(inner_state.property_writes);
                    }
                    // If inner had only prologue (init-only), the IIFE
                    // contributes no value but its prologue's side
                    // effects bubble up.
                    continue;
                }
                if let Expression::AssignmentExpression(assign) = chain {
                    if let Some(module_name) = module_param
                        && is_module_exports_target(&assign.left, module_name)
                    {
                        let final_value = unwrap_assignment_chain(&assign.right);
                        if !is_pure_eager_expression(final_value, source) {
                            state.impure = true;
                            continue;
                        }
                        if state.captured_value.is_some() || !state.property_writes.is_empty() {
                            state.impure = true;
                            continue;
                        }
                        state.captured_value = Some(span_text(final_value, source).to_string());
                        continue;
                    }
                    if let Some((key, value)) =
                        match_exports_key_assignment(assign, exports_param, source)
                    {
                        if state.captured_value.is_some() {
                            state.impure = true;
                            continue;
                        }
                        if state.property_writes.insert(key, value).is_some() {
                            state.impure = true;
                        }
                        continue;
                    }
                    state.impure = true;
                    continue;
                }
                if let Expression::CallExpression(call) = chain {
                    if let Some((key, value)) =
                        match_object_define_property(call, exports_param, source)
                    {
                        if state.captured_value.is_some() {
                            state.impure = true;
                            continue;
                        }
                        if state.property_writes.insert(key, value).is_some() {
                            state.impure = true;
                        }
                        continue;
                    }
                    if is_reverts_setter_call_with_pure_args(call, source) {
                        let inner: &CallExpression<'_> = call;
                        state.prologue.push(span_text(inner, source).to_string());
                        continue;
                    }
                    // NEW: zero-arg call to a bare identifier — register
                    // as an inter-procedural dependency. The fixpoint
                    // determines whether the called binding is itself
                    // eager-safe; if all deps clear, the call's side
                    // effects are subsumed by the dep's eagerification
                    // (which runs at the dep's module-load time, before
                    // this module's). We DO NOT add the call to the
                    // prologue — re-invoking an eagerified binding
                    // would dereference a non-function.
                    if call.arguments.is_empty()
                        && let Expression::Identifier(callee) = &call.callee
                    {
                        state.call_deps.insert(callee.name.to_string());
                        continue;
                    }
                    state.impure = true;
                    continue;
                }
                if matches!(chain, Expression::Identifier(_)) {
                    continue;
                }
                if let Expression::SequenceExpression(seq) = chain {
                    let mut all_acceptable = true;
                    for sub in &seq.expressions {
                        match sub {
                            Expression::Identifier(_) => {}
                            Expression::CallExpression(c) => {
                                if is_reverts_setter_call_with_pure_args(c, source) {
                                    let inner: &CallExpression<'_> = c;
                                    state.prologue.push(span_text(inner, source).to_string());
                                } else if c.arguments.is_empty() {
                                    if let Expression::Identifier(callee) = &c.callee {
                                        state.call_deps.insert(callee.name.to_string());
                                    } else {
                                        all_acceptable = false;
                                        break;
                                    }
                                } else {
                                    all_acceptable = false;
                                    break;
                                }
                            }
                            _ => {
                                all_acceptable = false;
                                break;
                            }
                        }
                    }
                    if !all_acceptable {
                        state.impure = true;
                    }
                    continue;
                }
                state.impure = true;
            }
            Statement::ReturnStatement(ret) => {
                let Some(arg) = &ret.argument else {
                    state.impure = true;
                    continue;
                };
                let final_value = unwrap_assignment_chain(arg);
                if !is_pure_eager_expression(final_value, source) {
                    state.impure = true;
                    continue;
                }
                if state.captured_value.is_some() || !state.property_writes.is_empty() {
                    state.impure = true;
                    continue;
                }
                state.captured_value = Some(span_text(final_value, source).to_string());
            }
            _ => {
                state.impure = true;
            }
        }
    }
    state
}

fn analysis_to_classification(
    state: LazyBodyAnalysisState,
    module_param: Option<&str>,
) -> LazyBodyClassification {
    if state.impure {
        return LazyBodyClassification::Impure;
    }
    let base_value: Option<String> = if let Some(value) = state.captured_value {
        Some(value)
    } else if !state.property_writes.is_empty() {
        let formatted = state
            .property_writes
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!("{{ {formatted} }}"))
    } else if !state.prologue.is_empty() || !state.call_deps.is_empty() {
        Some(if module_param.is_some() {
            "{}".into()
        } else {
            "void 0".into()
        })
    } else {
        None
    };
    let Some(base) = base_value else {
        return LazyBodyClassification::Impure;
    };
    let value = if state.prologue.is_empty() {
        base
    } else {
        let mut combined = String::new();
        combined.push('(');
        for stmt in &state.prologue {
            combined.push_str(stmt);
            combined.push_str(", ");
        }
        combined.push_str(&base);
        combined.push(')');
        combined
    };
    if state.call_deps.is_empty() {
        LazyBodyClassification::Eager { value }
    } else {
        LazyBodyClassification::EagerWithDeps {
            value,
            call_deps: state.call_deps,
        }
    }
}

fn is_reverts_setter_call_with_pure_args(call: &CallExpression<'_>, source: &str) -> bool {
    let Expression::Identifier(callee) = &call.callee else {
        return false;
    };
    if !callee.name.as_str().starts_with("__reverts_set_") {
        return false;
    }
    if call.arguments.len() != 1 {
        return false;
    }
    is_pure_setter_argument(&call.arguments[0], source)
}

/// Same shape as `is_pure_eager_expression` but applied to the
/// `Argument` variants of OXC AST. The two enums share discriminants
/// (via OXC's `inherit_variants!` macro) but Rust treats them as
/// distinct types — there's no zero-cost `Argument -> Expression`
/// view in OXC 0.42, so we re-state the variant match here. Only the
/// shapes a reverts-emitted setter call ever receives (literal-like
/// values, function/class expressions, simple unary negations) are
/// accepted; anything else (call, member access, identifier reference)
/// keeps the lazy thunk to be safe.
fn is_pure_setter_argument(arg: &Argument<'_>, source: &str) -> bool {
    use oxc_ast::ast::Argument as A;
    match arg {
        A::NumericLiteral(_)
        | A::StringLiteral(_)
        | A::BooleanLiteral(_)
        | A::NullLiteral(_)
        | A::BigIntLiteral(_)
        | A::RegExpLiteral(_)
        | A::TemplateLiteral(_)
        | A::FunctionExpression(_)
        | A::ArrowFunctionExpression(_)
        | A::ClassExpression(_) => true,
        A::ObjectExpression(obj) => is_pure_object_expression(obj, source),
        A::ArrayExpression(arr) => arr
            .elements
            .iter()
            .all(|element| is_pure_array_element(element, source)),
        A::ParenthesizedExpression(inner) => is_pure_eager_expression(&inner.expression, source),
        A::UnaryExpression(unary) => {
            matches!(
                unary.operator,
                oxc_syntax::operator::UnaryOperator::LogicalNot
                    | oxc_syntax::operator::UnaryOperator::UnaryNegation
                    | oxc_syntax::operator::UnaryOperator::UnaryPlus
                    | oxc_syntax::operator::UnaryOperator::BitwiseNot
                    | oxc_syntax::operator::UnaryOperator::Void
            ) && is_pure_eager_expression(&unary.argument, source)
        }
        _ => false,
    }
}

fn is_harmless_variable_declaration(
    decl: &oxc_ast::ast::VariableDeclaration<'_>,
    source: &str,
) -> bool {
    decl.declarations
        .iter()
        .all(|declarator| match &declarator.init {
            None => true,
            Some(init) => is_pure_eager_expression(init, source),
        })
}

fn iife_block_body<'a>(expr: &'a Expression<'a>) -> Option<&'a oxc_ast::ast::FunctionBody<'a>> {
    let Expression::CallExpression(call) = expr else {
        return None;
    };
    if let Some(body) = function_body_of_invokable(&call.callee) {
        return Some(body);
    }
    if let Expression::StaticMemberExpression(member) = &call.callee {
        let prop = member.property.name.as_str();
        if (prop == "call" || prop == "apply")
            && let Some(body) = function_body_of_invokable(&member.object)
        {
            return Some(body);
        }
    }
    None
}

fn function_body_of_invokable<'a>(
    expr: &'a Expression<'a>,
) -> Option<&'a oxc_ast::ast::FunctionBody<'a>> {
    match expr {
        Expression::ParenthesizedExpression(inner) => function_body_of_invokable(&inner.expression),
        Expression::FunctionExpression(function) => function.body.as_deref(),
        Expression::ArrowFunctionExpression(arrow) if !arrow.expression => Some(&arrow.body),
        _ => None,
    }
}

fn is_module_exports_target(
    target: &oxc_ast::ast::AssignmentTarget<'_>,
    module_param: &str,
) -> bool {
    let oxc_ast::ast::AssignmentTarget::StaticMemberExpression(member) = target else {
        return false;
    };
    let Expression::Identifier(object) = &member.object else {
        return false;
    };
    object.name.as_str() == module_param && member.property.name.as_str() == "exports"
}

fn match_exports_key_assignment(
    assign: &oxc_ast::ast::AssignmentExpression<'_>,
    exports_param: &str,
    source: &str,
) -> Option<(String, String)> {
    let oxc_ast::ast::AssignmentTarget::StaticMemberExpression(member) = &assign.left else {
        return None;
    };
    let Expression::Identifier(object) = &member.object else {
        return None;
    };
    if object.name.as_str() != exports_param {
        return None;
    }
    let key = member.property.name.as_str().to_string();
    let final_value = unwrap_assignment_chain(&assign.right);
    if !is_pure_eager_expression(final_value, source) {
        return None;
    }
    Some((key, span_text(final_value, source).to_string()))
}

fn match_object_define_property(
    call: &oxc_ast::ast::CallExpression<'_>,
    exports_param: &str,
    source: &str,
) -> Option<(String, String)> {
    let Expression::StaticMemberExpression(callee) = &call.callee else {
        return None;
    };
    let Expression::Identifier(object_name) = &callee.object else {
        return None;
    };
    if object_name.name.as_str() != "Object" || callee.property.name.as_str() != "defineProperty" {
        return None;
    }
    if call.arguments.len() < 3 {
        return None;
    }
    let Argument::Identifier(target) = &call.arguments[0] else {
        return None;
    };
    if target.name.as_str() != exports_param {
        return None;
    }
    let key = match &call.arguments[1] {
        Argument::StringLiteral(s) => s.value.as_str().to_string(),
        _ => return None,
    };
    let Argument::ObjectExpression(descriptor) = &call.arguments[2] else {
        return None;
    };
    let mut value_text: Option<String> = None;
    for prop in &descriptor.properties {
        let oxc_ast::ast::ObjectPropertyKind::ObjectProperty(property) = prop else {
            return None;
        };
        let oxc_ast::ast::PropertyKey::StaticIdentifier(prop_name) = &property.key else {
            return None;
        };
        match prop_name.name.as_str() {
            "value" => {
                if !is_pure_eager_expression(&property.value, source) {
                    return None;
                }
                value_text = Some(span_text(&property.value, source).to_string());
            }
            "writable" | "configurable" | "enumerable" => {}
            _ => return None,
        }
    }
    Some((key, value_text?))
}

fn unwrap_assignment_chain<'a>(expr: &'a Expression<'a>) -> &'a Expression<'a> {
    match expr {
        Expression::AssignmentExpression(assign) => unwrap_assignment_chain(&assign.right),
        Expression::ParenthesizedExpression(inner) => unwrap_assignment_chain(&inner.expression),
        _ => expr,
    }
}

fn is_pure_eager_expression(expr: &Expression<'_>, source: &str) -> bool {
    match expr {
        Expression::NumericLiteral(_)
        | Expression::StringLiteral(_)
        | Expression::BooleanLiteral(_)
        | Expression::NullLiteral(_)
        | Expression::BigIntLiteral(_)
        | Expression::RegExpLiteral(_)
        | Expression::TemplateLiteral(_)
        | Expression::FunctionExpression(_)
        | Expression::ArrowFunctionExpression(_)
        | Expression::ClassExpression(_) => true,
        Expression::ObjectExpression(obj) => is_pure_object_expression(obj, source),
        Expression::ArrayExpression(arr) => arr
            .elements
            .iter()
            .all(|element| is_pure_array_element(element, source)),
        Expression::ParenthesizedExpression(inner) => {
            is_pure_eager_expression(&inner.expression, source)
        }
        Expression::UnaryExpression(unary) => {
            matches!(
                unary.operator,
                oxc_syntax::operator::UnaryOperator::LogicalNot
                    | oxc_syntax::operator::UnaryOperator::UnaryNegation
                    | oxc_syntax::operator::UnaryOperator::UnaryPlus
                    | oxc_syntax::operator::UnaryOperator::BitwiseNot
                    | oxc_syntax::operator::UnaryOperator::Void
            ) && is_pure_eager_expression(&unary.argument, source)
        }
        _ => false,
    }
}

fn is_pure_object_expression(obj: &oxc_ast::ast::ObjectExpression<'_>, source: &str) -> bool {
    for prop in &obj.properties {
        match prop {
            oxc_ast::ast::ObjectPropertyKind::ObjectProperty(p) => {
                if p.computed {
                    return false;
                }
                if !is_pure_eager_expression(&p.value, source) {
                    return false;
                }
            }
            oxc_ast::ast::ObjectPropertyKind::SpreadProperty(_) => return false,
        }
    }
    true
}

fn is_pure_array_element(elem: &oxc_ast::ast::ArrayExpressionElement<'_>, source: &str) -> bool {
    // `ArrayExpressionElement` shares discriminants with `Expression`
    // via OXC's `inherit_variants!` macro but the two enums are distinct
    // types in Rust — there's no zero-cost `&ArrayExpressionElement →
    // &Expression` view. We re-state the same recursive shape match here
    // so that arrays of nested objects, arrays, and other pure shapes
    // are accepted (matching the byte-level `pure_array_literal` scanner
    // in the planner). The two enums share `Elision` and `SpreadElement`,
    // which `Expression` doesn't have — spread keeps the lazy thunk to
    // be safe.
    use oxc_ast::ast::ArrayExpressionElement as A;
    match elem {
        A::Elision(_)
        | A::NumericLiteral(_)
        | A::StringLiteral(_)
        | A::BooleanLiteral(_)
        | A::NullLiteral(_)
        | A::BigIntLiteral(_)
        | A::RegExpLiteral(_)
        | A::TemplateLiteral(_)
        | A::FunctionExpression(_)
        | A::ArrowFunctionExpression(_)
        | A::ClassExpression(_) => true,
        A::ObjectExpression(obj) => is_pure_object_expression(obj, source),
        A::ArrayExpression(arr) => arr
            .elements
            .iter()
            .all(|element| is_pure_array_element(element, source)),
        A::ParenthesizedExpression(inner) => is_pure_eager_expression(&inner.expression, source),
        A::UnaryExpression(unary) => {
            matches!(
                unary.operator,
                oxc_syntax::operator::UnaryOperator::LogicalNot
                    | oxc_syntax::operator::UnaryOperator::UnaryNegation
                    | oxc_syntax::operator::UnaryOperator::UnaryPlus
                    | oxc_syntax::operator::UnaryOperator::BitwiseNot
                    | oxc_syntax::operator::UnaryOperator::Void
            ) && is_pure_eager_expression(&unary.argument, source)
        }
        _ => false,
    }
}

fn span_text<'a>(node: &impl oxc_span::GetSpan, source: &'a str) -> &'a str {
    let span = node.span();
    &source[span.start as usize..span.end as usize]
}

pub fn parse_error_message(error: &JsError, fallback: &str) -> String {
    match error {
        JsError::ParseFailed(errors) => errors.first().map_or_else(
            || fallback.to_string(),
            |error| {
                let diagnostic = error
                    .diagnostics
                    .first()
                    .map_or("no diagnostic", String::as_str);
                format!("{fallback} as {}: {diagnostic}", error.source_type)
            },
        ),
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
    Statement::ImportDeclaration(builder.alloc_import_declaration(
        SPAN,
        specifiers,
        source,
        None,
        NONE,
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SimpleNamedImportSpecifier {
    imported: String,
    local: String,
}

fn coalesce_simple_named_imports_in_program<'a>(
    program: &mut Program<'a>,
    builder: &AstBuilder<'a>,
) {
    let mut specifiers_by_source = BTreeMap::<String, BTreeSet<SimpleNamedImportSpecifier>>::new();
    let mut first_index_by_source = BTreeMap::<String, usize>::new();
    let mut duplicate_indices = Vec::<usize>::new();

    for (index, statement) in program.body.iter().enumerate() {
        let Statement::ImportDeclaration(declaration) = statement else {
            continue;
        };
        let Some(specifiers) = simple_named_import_specifiers(declaration) else {
            continue;
        };
        let source = declaration.source.value.as_str().to_string();
        specifiers_by_source
            .entry(source.clone())
            .or_default()
            .extend(specifiers);
        use std::collections::btree_map::Entry;
        match first_index_by_source.entry(source) {
            Entry::Vacant(entry) => {
                entry.insert(index);
            }
            Entry::Occupied(_) => {
                duplicate_indices.push(index);
            }
        }
    }

    if duplicate_indices.is_empty() {
        return;
    }

    for (source, index) in first_index_by_source {
        let Some(specifiers) = specifiers_by_source.get(source.as_str()) else {
            continue;
        };
        program.body[index] =
            simple_named_import_statement(builder, source.as_str(), specifiers.iter());
    }

    for index in duplicate_indices.iter().rev() {
        program.body.remove(*index);
    }
}

fn simple_named_import_specifiers(
    declaration: &ImportDeclaration<'_>,
) -> Option<BTreeSet<SimpleNamedImportSpecifier>> {
    if declaration.import_kind != ImportOrExportKind::Value
        || declaration.phase.is_some()
        || declaration.with_clause.is_some()
    {
        return None;
    }
    let specifiers = declaration.specifiers.as_ref()?;
    if specifiers.is_empty() {
        return None;
    }

    let mut out = BTreeSet::<SimpleNamedImportSpecifier>::new();
    for specifier in specifiers {
        let ImportDeclarationSpecifier::ImportSpecifier(specifier) = specifier else {
            return None;
        };
        if specifier.import_kind != ImportOrExportKind::Value {
            return None;
        }
        out.insert(SimpleNamedImportSpecifier {
            imported: module_export_name_text(&specifier.imported)?,
            local: specifier.local.name.as_str().to_string(),
        });
    }
    Some(out)
}

fn simple_named_import_statement<'a, 'b>(
    builder: &AstBuilder<'a>,
    source: &str,
    specifiers: impl Iterator<Item = &'b SimpleNamedImportSpecifier>,
) -> Statement<'a> {
    let mut generated_specifiers = builder.vec();
    for specifier in specifiers {
        let imported =
            builder.module_export_name_identifier_name(SPAN, specifier.imported.as_str());
        let local = builder.binding_identifier(SPAN, specifier.local.as_str());
        generated_specifiers.push(builder.import_declaration_specifier_import_specifier(
            SPAN,
            imported,
            local,
            ImportOrExportKind::Value,
        ));
    }
    let source = builder.string_literal(SPAN, source, None);
    Statement::ImportDeclaration(builder.alloc_import_declaration(
        SPAN,
        Some(generated_specifiers),
        source,
        None,
        NONE,
        ImportOrExportKind::Value,
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CompatibleImportDetails {
    Named(BTreeSet<SimpleNamedImportSpecifier>),
    Default {
        local: String,
    },
    DefaultNamed {
        local: String,
        named: BTreeSet<SimpleNamedImportSpecifier>,
    },
    Namespace {
        local: String,
    },
    DefaultNamespace {
        _default_local: String,
        _namespace_local: String,
    },
}

#[derive(Debug, Default, Clone)]
struct MixedImportState {
    named_indices: Vec<usize>,
    named_specifiers: BTreeSet<SimpleNamedImportSpecifier>,
    default_target: Option<(usize, String, BTreeSet<SimpleNamedImportSpecifier>)>,
    duplicate_default_indices: Vec<usize>,
    duplicate_default_specifiers: BTreeSet<SimpleNamedImportSpecifier>,
    namespace_target: Option<(usize, String)>,
}

fn coalesce_compatible_mixed_imports_in_program<'a>(
    program: &mut Program<'a>,
    builder: &AstBuilder<'a>,
) {
    let mut states = BTreeMap::<String, MixedImportState>::new();

    for (index, statement) in program.body.iter().enumerate() {
        let Statement::ImportDeclaration(declaration) = statement else {
            continue;
        };
        let Some(details) = compatible_import_details(declaration) else {
            continue;
        };
        let state = states
            .entry(declaration.source.value.as_str().to_string())
            .or_default();
        match details {
            CompatibleImportDetails::Named(specifiers) => {
                state.named_indices.push(index);
                state.named_specifiers.extend(specifiers);
            }
            CompatibleImportDetails::Default { local } => {
                if state.default_target.is_some() {
                    state.duplicate_default_indices.push(index);
                    state
                        .duplicate_default_specifiers
                        .insert(default_import_specifier(local.as_str()));
                } else {
                    state
                        .default_target
                        .get_or_insert_with(|| (index, local, BTreeSet::new()));
                }
            }
            CompatibleImportDetails::DefaultNamed { local, named } => {
                if state.default_target.is_some() {
                    state.duplicate_default_indices.push(index);
                    state
                        .duplicate_default_specifiers
                        .insert(default_import_specifier(local.as_str()));
                    state.named_specifiers.extend(named);
                } else {
                    state
                        .default_target
                        .get_or_insert_with(|| (index, local, named));
                }
            }
            CompatibleImportDetails::Namespace { local } => {
                state.namespace_target.get_or_insert_with(|| (index, local));
            }
            CompatibleImportDetails::DefaultNamespace { .. } => {}
        }
    }

    let mut replacements = BTreeMap::<usize, Statement<'a>>::new();
    let mut duplicate_indices = BTreeSet::<usize>::new();
    for (source, state) in states {
        if let Some((default_index, default_local, mut named_specifiers)) =
            state.default_target.clone()
            && (!state.named_indices.is_empty() || !state.duplicate_default_indices.is_empty())
        {
            named_specifiers.extend(state.named_specifiers);
            named_specifiers.extend(state.duplicate_default_specifiers);
            let replacement_index = state
                .named_indices
                .iter()
                .copied()
                .chain(state.duplicate_default_indices.iter().copied())
                .chain([default_index])
                .min()
                .unwrap_or(default_index);
            replacements.insert(
                replacement_index,
                default_named_import_statement(
                    builder,
                    source.as_str(),
                    default_local.as_str(),
                    named_specifiers.iter(),
                ),
            );
            for index in state
                .named_indices
                .iter()
                .copied()
                .chain(state.duplicate_default_indices.iter().copied())
                .chain([default_index])
            {
                if index != replacement_index {
                    duplicate_indices.insert(index);
                }
            }
            continue;
        }

        if state.named_indices.is_empty()
            && let (
                Some((default_index, default_local, _)),
                Some((namespace_index, namespace_local)),
            ) = (state.default_target, state.namespace_target)
        {
            let replacement_index = default_index.min(namespace_index);
            replacements.insert(
                replacement_index,
                default_namespace_import_statement(
                    builder,
                    source.as_str(),
                    default_local.as_str(),
                    namespace_local.as_str(),
                ),
            );
            if default_index != replacement_index {
                duplicate_indices.insert(default_index);
            }
            if namespace_index != replacement_index {
                duplicate_indices.insert(namespace_index);
            }
        }
    }

    if duplicate_indices.is_empty() {
        return;
    }
    let mut merged = builder.vec();
    for (index, statement) in program.body.drain(..).enumerate() {
        if duplicate_indices.contains(&index) {
            continue;
        }
        if let Some(replacement) = replacements.remove(&index) {
            merged.push(replacement);
        } else {
            merged.push(statement);
        }
    }
    program.body = merged;
}

fn compatible_import_details(
    declaration: &ImportDeclaration<'_>,
) -> Option<CompatibleImportDetails> {
    if declaration.import_kind != ImportOrExportKind::Value
        || declaration.phase.is_some()
        || declaration.with_clause.is_some()
    {
        return None;
    }
    let specifiers = declaration.specifiers.as_ref()?;
    if specifiers.is_empty() {
        return None;
    }

    let mut default_local = None::<String>;
    let mut namespace_local = None::<String>;
    let mut named = BTreeSet::<SimpleNamedImportSpecifier>::new();
    for specifier in specifiers {
        match specifier {
            ImportDeclarationSpecifier::ImportDefaultSpecifier(specifier) => {
                if default_local.is_some() {
                    return None;
                }
                default_local = Some(specifier.local.name.as_str().to_string());
            }
            ImportDeclarationSpecifier::ImportNamespaceSpecifier(specifier) => {
                if namespace_local.is_some() {
                    return None;
                }
                namespace_local = Some(specifier.local.name.as_str().to_string());
            }
            ImportDeclarationSpecifier::ImportSpecifier(specifier) => {
                if specifier.import_kind != ImportOrExportKind::Value {
                    return None;
                }
                named.insert(SimpleNamedImportSpecifier {
                    imported: module_export_name_text(&specifier.imported)?,
                    local: specifier.local.name.as_str().to_string(),
                });
            }
        }
    }

    match (default_local, namespace_local, named.is_empty()) {
        (Some(local), None, true) => Some(CompatibleImportDetails::Default { local }),
        (Some(local), None, false) => Some(CompatibleImportDetails::DefaultNamed { local, named }),
        (None, Some(local), true) => Some(CompatibleImportDetails::Namespace { local }),
        (Some(_default_local), Some(_namespace_local), true) => {
            Some(CompatibleImportDetails::DefaultNamespace {
                _default_local,
                _namespace_local,
            })
        }
        (None, None, false) => Some(CompatibleImportDetails::Named(named)),
        _ => None,
    }
}

fn default_named_import_statement<'a, 'b>(
    builder: &AstBuilder<'a>,
    source: &str,
    default_local: &str,
    named_specifiers: impl Iterator<Item = &'b SimpleNamedImportSpecifier>,
) -> Statement<'a> {
    let mut specifiers = builder.vec1(
        builder.import_declaration_specifier_import_default_specifier(
            SPAN,
            builder.binding_identifier(SPAN, default_local),
        ),
    );
    for specifier in named_specifiers {
        let imported =
            builder.module_export_name_identifier_name(SPAN, specifier.imported.as_str());
        let local = builder.binding_identifier(SPAN, specifier.local.as_str());
        specifiers.push(builder.import_declaration_specifier_import_specifier(
            SPAN,
            imported,
            local,
            ImportOrExportKind::Value,
        ));
    }
    let source = builder.string_literal(SPAN, source, None);
    Statement::ImportDeclaration(builder.alloc_import_declaration(
        SPAN,
        Some(specifiers),
        source,
        None,
        NONE,
        ImportOrExportKind::Value,
    ))
}

fn default_namespace_import_statement<'a>(
    builder: &AstBuilder<'a>,
    source: &str,
    default_local: &str,
    namespace_local: &str,
) -> Statement<'a> {
    let mut specifiers = builder.vec1(
        builder.import_declaration_specifier_import_default_specifier(
            SPAN,
            builder.binding_identifier(SPAN, default_local),
        ),
    );
    specifiers.push(
        builder.import_declaration_specifier_import_namespace_specifier(
            SPAN,
            builder.binding_identifier(SPAN, namespace_local),
        ),
    );
    let source = builder.string_literal(SPAN, source, None);
    Statement::ImportDeclaration(builder.alloc_import_declaration(
        SPAN,
        Some(specifiers),
        source,
        None,
        NONE,
        ImportOrExportKind::Value,
    ))
}

fn default_import_specifier(local: &str) -> SimpleNamedImportSpecifier {
    SimpleNamedImportSpecifier {
        imported: "default".to_string(),
        local: local.to_string(),
    }
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

fn module_export_name_text(name: &ModuleExportName<'_>) -> Option<String> {
    match name {
        ModuleExportName::IdentifierName(identifier) => Some(identifier.name.as_str().to_string()),
        ModuleExportName::IdentifierReference(identifier) => {
            Some(identifier.name.as_str().to_string())
        }
        ModuleExportName::StringLiteral(_) => None,
    }
}

fn parse_options_for(source_type: SourceType) -> ParseOptions {
    ParseOptions {
        allow_return_outside_function: source_type.is_script(),
        ..Default::default()
    }
}

pub fn normalize_source_for_pipeline(source: &str, path_hint: Option<&Path>) -> Result<String> {
    format_source_pretty(source, path_hint, ParseGoal::TypeScript)
}

#[must_use]
pub fn sanitize_identifier(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for (index, ch) in value.chars().enumerate() {
        let valid = if index == 0 {
            is_identifier_start(ch) || is_identifier_part(ch)
        } else {
            is_identifier_part(ch)
        };
        output.push(if valid { ch } else { '_' });
    }

    if output.is_empty() {
        return "_".to_string();
    }

    if output
        .chars()
        .next()
        .is_some_and(|first| !is_identifier_start(first))
    {
        output.insert(0, '_');
    }

    if is_reserved_word(&output) {
        output.insert(0, '_');
    }

    output
}

#[must_use]
pub fn is_identifier_start(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphabetic()
}

#[must_use]
pub fn is_identifier_part(ch: char) -> bool {
    is_identifier_start(ch) || ch.is_ascii_digit()
}

fn is_reserved_word(value: &str) -> bool {
    matches!(
        value,
        "await"
            | "arguments"
            | "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "debugger"
            | "default"
            | "delete"
            | "do"
            | "else"
            | "enum"
            | "eval"
            | "export"
            | "extends"
            | "false"
            | "finally"
            | "for"
            | "function"
            | "if"
            | "implements"
            | "import"
            | "in"
            | "instanceof"
            | "interface"
            | "let"
            | "new"
            | "null"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "return"
            | "static"
            | "super"
            | "switch"
            | "this"
            | "throw"
            | "true"
            | "try"
            | "typeof"
            | "var"
            | "void"
            | "while"
            | "with"
            | "yield"
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        CompilerLowering, GeneratedExport, GeneratedImport, GeneratedRename, ImportUsageScope,
        JsError, LazyBodyClassification, ParseGoal, classify_import_usage_scope,
        classify_lazy_module_body, collect_file_url_source_location_rewrites,
        collect_path_builder_calls, collect_static_resource_specifiers, collect_string_literals,
        extract_lazy_module_eager_value, format_source_pretty, format_source_with_module_items,
        format_source_with_module_items_and_renames,
        format_source_with_module_items_and_renames_with_report, normalize_source_for_pipeline,
        parse_error_message, parse_source, sanitize_identifier,
        verify_only_immediate_call_references,
    };
    use std::collections::BTreeSet;

    #[test]
    fn parses_typescript_without_external_tooling() {
        let source = "const answer: number = 42;";

        assert!(parse_source(source, Some(Path::new("fixture.ts")), ParseGoal::TypeScript).is_ok());
    }

    #[test]
    fn collects_string_literal_facts_from_ast_without_source_scanning_fallback() {
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
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import * as pkg from 'pkg';"));
        assert!(formatted.contains("const answer = pkg.answer;"));
        assert!(formatted.contains("export { answer };"));
    }

    #[test]
    fn module_item_formatting_coalesces_named_imports_by_source() {
        let formatted = format_source_with_module_items(
            "import { join as localJoin } from 'path';\nimport * as pathNS from 'path';\nimport { dirname as localDir, join as otherJoin } from 'path';\nconsole.log(pathNS, localJoin, localDir, otherJoin);",
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains(
            "import { dirname as localDir, join as localJoin, join as otherJoin } from 'path';"
        ));
        assert!(formatted.contains("import * as pathNS from 'path';"));
        assert_eq!(formatted.matches("from 'path'").count(), 2);
    }

    #[test]
    fn module_item_formatting_keeps_namespace_and_named_imports_separate() {
        let formatted = format_source_with_module_items(
            "import * as pkgNS from 'pkg';\nimport { alpha } from 'pkg';\nimport { beta } from 'pkg';\nconsole.log(pkgNS, alpha, beta);",
            &[],
            &[],
            Some(Path::new("src/index.ts")),
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
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import defaultPkg, { alpha, beta as localBeta } from 'pkg';"));
        assert_eq!(formatted.matches("from 'pkg'").count(), 1);
    }

    #[test]
    fn module_item_formatting_merges_default_and_namespace_imports() {
        let formatted = format_source_with_module_items(
            "import defaultPkg from 'pkg';\nimport * as pkgNS from 'pkg';\nconsole.log(defaultPkg, pkgNS);",
            &[],
            &[],
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import defaultPkg, * as pkgNS from 'pkg';"));
        assert_eq!(formatted.matches("from 'pkg'").count(), 1);
    }

    #[test]
    fn module_item_formatting_merges_multiple_default_aliases_as_named_default() {
        let formatted = format_source_with_module_items(
            "import first from 'pkg';\nimport second from 'pkg';\nimport { alpha } from 'pkg';\nconsole.log(first, second, alpha);",
            &[],
            &[],
            Some(Path::new("src/index.ts")),
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
            Some(Path::new("src/index.ts")),
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
            Some(Path::new("src/index.ts")),
            ParseGoal::TypeScript,
            CompilerLowering::None,
        )
        .expect("fixture should format");

        assert!(formatted.contains("import firstDefault, * as firstNS from 'pkg';"));
        assert!(formatted.contains("import secondDefault, * as secondNS from 'pkg';"));
        assert_eq!(formatted.matches("from 'pkg'").count(), 2);
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

        assert!(formatted.contains("export { alpha, beta };"));
        assert!(formatted.contains("export { beta as renamed };"));
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
}
