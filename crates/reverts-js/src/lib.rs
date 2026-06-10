use std::collections::BTreeMap;
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::{
    AstBuilder, NONE, Visit,
    ast::{
        Argument, BindingPatternKind, CallExpression, Declaration, ExportAllDeclaration,
        ExportDefaultDeclarationKind, ExportNamedDeclaration, Expression, IdentifierReference,
        ImportDeclaration, ImportExpression, ImportOrExportKind, NewExpression, Program, Statement,
        StringLiteral, VariableDeclarator,
    },
    visit::walk::{
        walk_call_expression, walk_export_all_declaration, walk_export_named_declaration,
        walk_import_declaration, walk_import_expression, walk_new_expression, walk_string_literal,
    },
};
use oxc_codegen::{CodeGenerator, CodegenOptions};
use oxc_parser::{ParseOptions, Parser};
use oxc_span::{GetSpan, SPAN, SourceType, Span};

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
}

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
            if !program_references_interop_require_default(&parsed.program) {
                parsed
                    .program
                    .body
                    .retain(|statement| !is_babel_interop_require_default_helper(statement));
            }
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

/// Apply compiler-specific source-level rewrites before the main format pass.
/// Currently handles Babel's `_interopRequireDefault(require("X"))` pattern,
/// reducing it to `{ default: require("X") }` so the helper call disappears
/// while `.default` access on the wrapped binding stays valid.
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
                let Some(arg_span) = babel_interop_require_default_arg_span(init) else {
                    continue;
                };
                let init_span = init.span();
                let arg_text = &source[arg_span.start as usize..arg_span.end as usize];
                replacements.push((
                    init_span.start,
                    init_span.end,
                    format!("{{ default: require({arg_text}) }}"),
                ));
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

/// Recognise the canonical Babel interop wrapper `_interopRequireDefault(require("X"))`.
/// Returns the span of the inner string-literal specifier so the caller can
/// re-use the original quoting and escaping when constructing the rewrite.
fn babel_interop_require_default_arg_span(expression: &Expression<'_>) -> Option<Span> {
    let Expression::CallExpression(outer) = expression else {
        return None;
    };
    let Expression::Identifier(callee) = &outer.callee else {
        return None;
    };
    if callee.name.as_str() != "_interopRequireDefault" || outer.arguments.len() != 1 {
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

/// Returns true if any expression in the program refers to
/// `_interopRequireDefault` as an identifier (i.e. uses it as a value).
/// The function's own `BindingIdentifier` does NOT show up as an
/// `IdentifierReference`, so when the count is zero the helper declaration
/// is genuinely dead and can be safely stripped.
fn program_references_interop_require_default(program: &Program<'_>) -> bool {
    let mut counter = InteropRequireDefaultReferenceCounter::default();
    counter.visit_program(program);
    counter.count > 0
}

#[derive(Debug, Default)]
struct InteropRequireDefaultReferenceCounter {
    count: usize,
}

impl<'a> Visit<'a> for InteropRequireDefaultReferenceCounter {
    fn visit_identifier_reference(&mut self, identifier: &IdentifierReference<'a>) {
        if identifier.name.as_str() == "_interopRequireDefault" {
            self.count += 1;
        }
    }
}

/// Recognise the canonical Babel CJS interop helper declaration:
/// `function _interopRequireDefault(<param>) { return <param> && <param>.__esModule ? <param> : { default: <param> }; }`.
/// The strict-name + single-param + single-return-statement check stays
/// narrow enough that a user-written function with the same name but a
/// different signature is left alone.
fn is_babel_interop_require_default_helper(statement: &Statement<'_>) -> bool {
    let Statement::FunctionDeclaration(function) = statement else {
        return false;
    };
    let Some(name) = function.id.as_ref() else {
        return false;
    };
    if name.name.as_str() != "_interopRequireDefault" {
        return false;
    }
    if function.params.items.len() != 1 {
        return false;
    }
    let Some(body) = function.body.as_ref() else {
        return false;
    };
    body.statements.len() == 1 && matches!(body.statements[0], Statement::ReturnStatement(_))
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
        CompilerLowering, GeneratedExport, GeneratedImport, JsError, ParseGoal,
        collect_file_url_source_location_rewrites, collect_path_builder_calls,
        collect_static_resource_specifiers, collect_string_literals, format_source_pretty,
        format_source_with_module_items, normalize_source_for_pipeline, parse_error_message,
        parse_source, sanitize_identifier,
    };

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
}
