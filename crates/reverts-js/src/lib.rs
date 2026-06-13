pub mod normalize;

use std::collections::BTreeMap;
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::{
    AstBuilder, NONE, Visit,
    ast::{
        Argument, ArrowFunctionExpression, BindingPatternKind, CallExpression, Declaration,
        ExportAllDeclaration, ExportDefaultDeclarationKind, ExportNamedDeclaration, Expression,
        Function, IdentifierReference, ImportDeclaration, ImportExpression, ImportOrExportKind,
        NewExpression, Program, Statement, StringLiteral, VariableDeclarator,
    },
    visit::walk::{
        walk_arrow_function_expression, walk_call_expression, walk_export_all_declaration,
        walk_export_named_declaration, walk_function, walk_import_declaration,
        walk_import_expression, walk_new_expression, walk_string_literal,
    },
};
use oxc_codegen::{CodeGenerator, CodegenOptions};
use oxc_parser::{ParseOptions, Parser};
use oxc_span::{GetSpan, SPAN, SourceType, Span};
use oxc_syntax::scope::ScopeFlags;

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
    // Source-level pre-rewrites: applied before the main parse/codegen path so
    // that subsequent steps (audit, codegen) see the lowered form. The
    // rewriter parses once, collects span-aware edits, and returns the
    // unchanged source if it cannot parse — in which case the regular parse
    // below will surface a faithful diagnostic.
    let lowered = apply_source_level_lowerings(body_source, path_hint, goal, lowering);
    let body_source = lowered.as_str();

    let mut errors = Vec::new();

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
        if parsed.program.body.is_empty() {
            parsed.program.body.push(empty_export_statement(&builder));
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
/// Returns `None` for any unrecognized statement — wrong rewrites break
/// the runtime, missed rewrites are harmless (the existing lazy stays).
/// The body is wrapped in `function __lazy() { BODY }` for parsing so it
/// can contain `return` statements.
#[must_use]
pub fn extract_lazy_module_eager_value(
    body: &str,
    exports_param: &str,
    module_param: Option<&str>,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Option<String> {
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
        return analyze_lazy_body_statements(
            &function_body.statements,
            &wrapped,
            exports_param,
            module_param,
        );
    }
    None
}

fn analyze_lazy_body_statements(
    statements: &oxc_allocator::Vec<'_, Statement<'_>>,
    source: &str,
    exports_param: &str,
    module_param: Option<&str>,
) -> Option<String> {
    let mut captured_value: Option<String> = None;
    let mut property_writes: BTreeMap<String, String> = BTreeMap::new();
    // Side-effect statements collected before the value-producing
    // statement. They get folded into a comma expression: the lazy
    // thunk's original "run on first call" semantics map to "run at
    // module load" — same observable state under the SCC-singleton
    // gate that compute_eager_safe_analysis enforces. Recognized
    // prologue forms:
    //   * `__reverts_set_<X>(PURE)` — reverts-emitted setter; pure-
    //     mutates a module-local binding.
    //   * Bare identifier reference (`x;`) — esbuild emits these as
    //     anti-tree-shake markers; they have no runtime effect.
    let mut prologue: Vec<String> = Vec::new();
    for stmt in statements {
        match stmt {
            Statement::VariableDeclaration(decl) => {
                if !is_harmless_variable_declaration(decl, source) {
                    return None;
                }
            }
            Statement::EmptyStatement(_) => {}
            Statement::ExpressionStatement(expr_stmt) => {
                let mut chain = &expr_stmt.expression;
                while let Expression::ParenthesizedExpression(inner) = chain {
                    chain = &inner.expression;
                }
                if let Some(inner_body) = iife_block_body(chain) {
                    let recurse = analyze_lazy_body_statements(
                        &inner_body.statements,
                        source,
                        exports_param,
                        module_param,
                    )?;
                    if captured_value.is_some() || !property_writes.is_empty() {
                        return None;
                    }
                    captured_value = Some(recurse);
                    continue;
                }
                if let Expression::AssignmentExpression(assign) = chain {
                    if let Some(module_name) = module_param
                        && is_module_exports_target(&assign.left, module_name)
                    {
                        let final_value = unwrap_assignment_chain(&assign.right);
                        if !is_pure_eager_expression(final_value, source) {
                            return None;
                        }
                        if captured_value.is_some() || !property_writes.is_empty() {
                            return None;
                        }
                        captured_value = Some(span_text(final_value, source).to_string());
                        continue;
                    }
                    if let Some((key, value)) =
                        match_exports_key_assignment(assign, exports_param, source)
                    {
                        if captured_value.is_some() {
                            return None;
                        }
                        if property_writes.insert(key, value).is_some() {
                            return None;
                        }
                        continue;
                    }
                    return None;
                }
                if let Expression::CallExpression(call) = chain {
                    if let Some((key, value)) =
                        match_object_define_property(call, exports_param, source)
                    {
                        if captured_value.is_some() {
                            return None;
                        }
                        if property_writes.insert(key, value).is_some() {
                            return None;
                        }
                        continue;
                    }
                    if is_reverts_setter_call_with_pure_args(call, source) {
                        let inner: &CallExpression<'_> = call;
                        prologue.push(span_text(inner, source).to_string());
                        continue;
                    }
                    return None;
                }
                if matches!(chain, Expression::Identifier(_)) {
                    // Bare identifier reference statement — esbuild
                    // emits these as anti-tree-shake markers. No
                    // runtime effect. Drop on collapse.
                    continue;
                }
                if let Expression::SequenceExpression(seq) = chain {
                    // Commonly seen: `setter1(...), setter2(...), setter3(...);`
                    // — every comma-separated expression must be one
                    // of our accepted side-effect forms.
                    let mut all_acceptable = true;
                    for sub in &seq.expressions {
                        match sub {
                            Expression::Identifier(_) => {}
                            Expression::CallExpression(c)
                                if is_reverts_setter_call_with_pure_args(c, source) =>
                            {
                                let inner: &CallExpression<'_> = c;
                                prologue.push(span_text(inner, source).to_string());
                            }
                            _ => {
                                all_acceptable = false;
                                break;
                            }
                        }
                    }
                    if all_acceptable {
                        continue;
                    }
                    return None;
                }
                return None;
            }
            Statement::ReturnStatement(ret) => {
                let Some(arg) = &ret.argument else {
                    return None;
                };
                let final_value = unwrap_assignment_chain(arg);
                if !is_pure_eager_expression(final_value, source) {
                    return None;
                }
                if captured_value.is_some() || !property_writes.is_empty() {
                    return None;
                }
                captured_value = Some(span_text(final_value, source).to_string());
            }
            _ => return None,
        }
    }
    let base_value: Option<String> = if let Some(value) = captured_value {
        Some(value)
    } else if !property_writes.is_empty() {
        let formatted = property_writes
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!("{{ {formatted} }}"))
    } else if !prologue.is_empty() {
        // Init-only body: no value produced, just side effects.
        // Use the lazy thunk's natural "empty result":
        //   * lazyModule → empty exports object `{}` (what the
        //     wrapper would have created on first call)
        //   * lazyValue  → `void 0` (thunk with no return)
        Some(if module_param.is_some() {
            "{}".into()
        } else {
            "void 0".into()
        })
    } else {
        None
    };
    let base = base_value?;
    if prologue.is_empty() {
        return Some(base);
    }
    // Compose `(setter1, setter2, base)` — side effects run in order,
    // expression evaluates to `base`. Direction-of-evaluation matches
    // the original lazy body, just shifted to module-load time.
    let mut combined = String::new();
    combined.push('(');
    for stmt in &prologue {
        combined.push_str(stmt);
        combined.push_str(", ");
    }
    combined.push_str(&base);
    combined.push(')');
    Some(combined)
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
        A::ArrayExpression(arr) => arr.elements.iter().all(is_pure_array_element),
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
        Expression::ArrayExpression(arr) => arr.elements.iter().all(is_pure_array_element),
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

fn is_pure_array_element(elem: &oxc_ast::ast::ArrayExpressionElement<'_>) -> bool {
    // We deliberately keep this list narrow: only literal-shape elements
    // are accepted. Nested object/array/function inside an array is
    // technically pure too, but the recursive case here would require
    // converting an `ArrayExpressionElement` to a borrowed `Expression`
    // (which oxc doesn't expose) — and array-of-non-literal exports is
    // a rare pattern in practice. Eagerifying is opt-in: missing a case
    // just leaves the lazy thunk alone, which is safe.
    use oxc_ast::ast::ArrayExpressionElement as A;
    matches!(
        elem,
        A::Elision(_)
            | A::NumericLiteral(_)
            | A::StringLiteral(_)
            | A::BooleanLiteral(_)
            | A::NullLiteral(_)
            | A::BigIntLiteral(_)
            | A::RegExpLiteral(_)
            | A::TemplateLiteral(_)
    )
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
            | "export"
            | "extends"
            | "false"
            | "finally"
            | "for"
            | "function"
            | "if"
            | "import"
            | "in"
            | "instanceof"
            | "new"
            | "null"
            | "return"
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
        CompilerLowering, GeneratedExport, GeneratedImport, ImportUsageScope, JsError, ParseGoal,
        classify_import_usage_scope, collect_file_url_source_location_rewrites,
        collect_path_builder_calls, collect_static_resource_specifiers, collect_string_literals,
        extract_lazy_module_eager_value, format_source_pretty, format_source_with_module_items,
        normalize_source_for_pipeline, parse_error_message, parse_source, sanitize_identifier,
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

        assert!(formatted.contains("import * as __pkg from 'pkg';"));
        assert!(formatted.contains("const answer = __pkg.answer;"));
        assert!(formatted.contains("export { answer };"));
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
