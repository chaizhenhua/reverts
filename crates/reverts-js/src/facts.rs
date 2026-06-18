use std::collections::BTreeMap;
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::{
    Visit,
    ast::{
        Argument, BindingPatternKind, CallExpression, Declaration, ExportAllDeclaration,
        ExportDefaultDeclarationKind, ExportNamedDeclaration, Expression, IdentifierReference,
        ImportDeclaration, ImportExpression, NewExpression, Statement, StringLiteral,
    },
    visit::walk::{
        walk_call_expression, walk_export_all_declaration, walk_export_named_declaration,
        walk_expression, walk_import_declaration, walk_import_expression, walk_new_expression,
        walk_statement, walk_string_literal,
    },
};
use oxc_parser::Parser;
use oxc_span::GetSpan;
use oxc_syntax::operator::UnaryOperator;

use crate::errors::{JsError, ParseError, ParseGoal, Result};
use crate::parse::{parse_options_for, source_type_candidates};
use crate::{expression_identifier, module_export_name_text};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringLiteralFact {
    pub value: String,
    pub byte_start: u32,
    pub byte_end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticTemplateLiteralFact {
    pub value: String,
    pub byte_start: u32,
    pub byte_end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementSpanFact {
    pub byte_start: u32,
    pub byte_end: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TopLevelStatementKind {
    Import,
    Export,
    Setter,
    Function,
    Class,
    LazyValue,
    LazyModule,
    Variable,
    Other,
}

impl TopLevelStatementKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Import => "import",
            Self::Export => "export",
            Self::Setter => "setter",
            Self::Function => "function",
            Self::Class => "class",
            Self::LazyValue => "lazy_value",
            Self::LazyModule => "lazy_module",
            Self::Variable => "variable",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopLevelStatementFact {
    pub kind: TopLevelStatementKind,
    pub bindings: Vec<String>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentifierReadFact {
    pub name: String,
    pub byte_start: u32,
    pub byte_end: u32,
    pub is_call_callee: bool,
    pub call_arg_count: Option<u32>,
}

/// A statement-level slice of a `lazyValue(() => { ... })` body. Each
/// slice carries its source text and offsets relative to the surrounding
/// snippet, so the planner can evaluate migration of individual
/// statements (e.g. a single `class X = ...` inside the lazy body)
/// independently from the rest of the lazy block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LazyValueSubSnippet {
    pub source: String,
    /// Offset into the *enclosing* snippet's source text (not the global
    /// helper file).
    pub byte_start: u32,
    pub byte_end: u32,
    /// Top-level statement kind inside the lazy body.
    pub kind: TopLevelStatementKind,
    pub bindings: Vec<String>,
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

pub fn collect_static_template_literals(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<Vec<StaticTemplateLiteralFact>> {
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

        let mut collector = StaticTemplateLiteralCollector::default();
        collector.visit_program(&parsed.program);
        return Ok(collector.literals);
    }

    Err(JsError::ParseFailed(errors))
}

pub fn collect_void_zero_expression_statements(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<Vec<StatementSpanFact>> {
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

        let mut collector = VoidZeroExpressionStatementCollector::default();
        collector.visit_program(&parsed.program);
        return Ok(collector.statements);
    }

    Err(JsError::ParseFailed(errors))
}

/// Try to slice `source` (the body of one `RuntimePreludeSnippet`) into
/// per-statement sub-snippets, when the snippet matches the
/// `var X = lazyValue(() => { ... });` (or `var X = lazyModule(...)`)
/// shape. Returns `None` for any other shape so callers fall back to the
/// whole snippet as one unit.
pub fn lazy_value_sub_snippets(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Option<Vec<LazyValueSubSnippet>> {
    let allocator = Allocator::default();
    for source_type in source_type_candidates(path_hint, goal) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if !parsed.errors.is_empty() || parsed.panicked {
            continue;
        }
        // Only one top-level statement expected (a single `var X = lazyValue(...);`).
        let [Statement::VariableDeclaration(declaration)] = parsed.program.body.as_slice() else {
            return None;
        };
        let [declarator] = declaration.declarations.as_slice() else {
            return None;
        };
        let Some(Expression::CallExpression(call)) = declarator.init.as_ref() else {
            return None;
        };
        let callee = expression_identifier(&call.callee)?;
        if callee != "lazyValue" && callee != "lazyModule" {
            return None;
        }
        // The single argument must be an arrow function (`() => { BODY }`).
        let [argument] = call.arguments.as_slice() else {
            return None;
        };
        let Argument::ArrowFunctionExpression(arrow) = argument else {
            return None;
        };
        // Arrow body must be a function-body block; expression-only
        // arrows (`() => 1`) are single-statement lazies that aren't
        // worth slicing.
        if arrow.expression || arrow.body.statements.is_empty() {
            return None;
        }
        let slices = arrow
            .body
            .statements
            .iter()
            .map(|statement| {
                let span = statement.span();
                let (kind, bindings) = top_level_statement_kind_and_bindings(statement);
                LazyValueSubSnippet {
                    source: source
                        .get(span.start as usize..span.end as usize)
                        .unwrap_or("")
                        .to_string(),
                    byte_start: span.start,
                    byte_end: span.end,
                    kind,
                    bindings,
                }
            })
            .collect();
        return Some(slices);
    }
    None
}

pub fn collect_top_level_statement_facts(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<Vec<TopLevelStatementFact>> {
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

        return Ok(parsed
            .program
            .body
            .iter()
            .map(top_level_statement_fact)
            .collect());
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

pub fn collect_identifier_read_facts(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<Vec<IdentifierReadFact>> {
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

        let mut collector = IdentifierReadCollector::default();
        collector.visit_program(&parsed.program);
        return Ok(collector.into_facts());
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
struct StaticTemplateLiteralCollector {
    literals: Vec<StaticTemplateLiteralFact>,
}

impl<'a> Visit<'a> for StaticTemplateLiteralCollector {
    fn visit_expression(&mut self, expression: &Expression<'a>) {
        if let Expression::TemplateLiteral(template) = expression
            && template.expressions.is_empty()
            && let Some(quasi) = template.quasis.first()
            && let Some(value) = quasi.value.cooked.as_deref()
        {
            let span = expression.span();
            self.literals.push(StaticTemplateLiteralFact {
                value: value.to_string(),
                byte_start: span.start,
                byte_end: span.end,
            });
        }
        walk_expression(self, expression);
    }
}

#[derive(Debug, Default)]
struct VoidZeroExpressionStatementCollector {
    statements: Vec<StatementSpanFact>,
}

impl<'a> Visit<'a> for VoidZeroExpressionStatementCollector {
    fn visit_statement(&mut self, statement: &Statement<'a>) {
        if let Statement::ExpressionStatement(expression_statement) = statement
            && matches_void_numeric_expression(&expression_statement.expression)
        {
            let span = statement.span();
            self.statements.push(StatementSpanFact {
                byte_start: span.start,
                byte_end: span.end,
            });
        }
        walk_statement(self, statement);
    }
}

fn matches_void_numeric_expression(expression: &Expression<'_>) -> bool {
    let Expression::UnaryExpression(unary) = expression else {
        return false;
    };
    matches!(unary.operator, UnaryOperator::Void)
        && matches!(&unary.argument, Expression::NumericLiteral(_))
}

pub(crate) fn top_level_statement_fact(statement: &Statement<'_>) -> TopLevelStatementFact {
    let span = statement.span();
    let (kind, bindings) = top_level_statement_kind_and_bindings(statement);
    TopLevelStatementFact {
        kind,
        bindings,
        byte_start: span.start,
        byte_end: span.end,
    }
}

pub(crate) fn top_level_statement_kind_and_bindings(
    statement: &Statement<'_>,
) -> (TopLevelStatementKind, Vec<String>) {
    match statement {
        Statement::ImportDeclaration(_) => (TopLevelStatementKind::Import, Vec::new()),
        Statement::FunctionDeclaration(function) => {
            let bindings = function
                .id
                .as_ref()
                .map(|id| id.name.as_str().to_string())
                .into_iter()
                .collect::<Vec<_>>();
            let kind = if bindings
                .first()
                .is_some_and(|binding| binding.starts_with("__reverts_set_"))
            {
                TopLevelStatementKind::Setter
            } else {
                TopLevelStatementKind::Function
            };
            (kind, bindings)
        }
        Statement::ClassDeclaration(class) => (
            TopLevelStatementKind::Class,
            class
                .id
                .as_ref()
                .map(|id| id.name.as_str().to_string())
                .into_iter()
                .collect(),
        ),
        Statement::VariableDeclaration(declaration) => {
            let bindings = declaration_binding_names(declaration);
            let kind = declaration
                .declarations
                .iter()
                .find_map(|declarator| {
                    let init = declarator.init.as_ref()?;
                    let Expression::CallExpression(call) = init else {
                        return None;
                    };
                    match expression_identifier(&call.callee) {
                        Some("lazyValue") => Some(TopLevelStatementKind::LazyValue),
                        Some("lazyModule") => Some(TopLevelStatementKind::LazyModule),
                        _ => None,
                    }
                })
                .unwrap_or(TopLevelStatementKind::Variable);
            (kind, bindings)
        }
        Statement::ExportNamedDeclaration(export) => {
            let bindings = export
                .declaration
                .as_ref()
                .map(export_declaration_binding_names)
                .unwrap_or_default();
            (TopLevelStatementKind::Export, bindings)
        }
        Statement::ExportDefaultDeclaration(export) => {
            let bindings = match &export.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(function) => function
                    .id
                    .as_ref()
                    .map(|id| id.name.as_str().to_string())
                    .into_iter()
                    .collect(),
                ExportDefaultDeclarationKind::ClassDeclaration(class) => class
                    .id
                    .as_ref()
                    .map(|id| id.name.as_str().to_string())
                    .into_iter()
                    .collect(),
                _ => Vec::new(),
            };
            (TopLevelStatementKind::Export, bindings)
        }
        Statement::ExportAllDeclaration(_) => (TopLevelStatementKind::Export, Vec::new()),
        _ => (TopLevelStatementKind::Other, Vec::new()),
    }
}

fn export_declaration_binding_names(declaration: &Declaration<'_>) -> Vec<String> {
    match declaration {
        Declaration::FunctionDeclaration(function) => function
            .id
            .as_ref()
            .map(|id| id.name.as_str().to_string())
            .into_iter()
            .collect(),
        Declaration::ClassDeclaration(class) => class
            .id
            .as_ref()
            .map(|id| id.name.as_str().to_string())
            .into_iter()
            .collect(),
        Declaration::VariableDeclaration(declaration) => declaration_binding_names(declaration),
        _ => Vec::new(),
    }
}

fn declaration_binding_names(declaration: &oxc_ast::ast::VariableDeclaration<'_>) -> Vec<String> {
    declaration
        .declarations
        .iter()
        .filter_map(|declarator| match &declarator.id.kind {
            BindingPatternKind::BindingIdentifier(binding) => {
                Some(binding.name.as_str().to_string())
            }
            _ => None,
        })
        .collect()
}

#[derive(Debug, Default)]
struct IdentifierReadCollector {
    facts: BTreeMap<(u32, u32, String), IdentifierReadAccumulator>,
}

#[derive(Debug, Default)]
struct IdentifierReadAccumulator {
    is_call_callee: bool,
    call_arg_count: Option<u32>,
}

impl IdentifierReadCollector {
    fn push_identifier(
        &mut self,
        identifier: &IdentifierReference<'_>,
        is_call_callee: bool,
        call_arg_count: Option<u32>,
    ) {
        let span = identifier.span();
        let entry = self
            .facts
            .entry((span.start, span.end, identifier.name.as_str().to_string()))
            .or_default();
        entry.is_call_callee |= is_call_callee;
        if call_arg_count.is_some() {
            entry.call_arg_count = call_arg_count;
        }
    }

    fn into_facts(self) -> Vec<IdentifierReadFact> {
        self.facts
            .into_iter()
            .map(
                |((byte_start, byte_end, name), accumulator)| IdentifierReadFact {
                    name,
                    byte_start,
                    byte_end,
                    is_call_callee: accumulator.is_call_callee,
                    call_arg_count: accumulator.call_arg_count,
                },
            )
            .collect()
    }
}

impl<'a> Visit<'a> for IdentifierReadCollector {
    fn visit_export_named_declaration(&mut self, declaration: &ExportNamedDeclaration<'a>) {
        if declaration.source.is_none() {
            for specifier in &declaration.specifiers {
                if let Some(local) = module_export_name_text(&specifier.local) {
                    let span = specifier.local.span();
                    self.facts.entry((span.start, span.end, local)).or_default();
                }
            }
        }
        walk_export_named_declaration(self, declaration);
    }

    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if let Expression::Identifier(callee) = &call.callee {
            self.push_identifier(callee, true, Some(call.arguments.len() as u32));
        }
        walk_call_expression(self, call);
    }

    fn visit_new_expression(&mut self, expression: &NewExpression<'a>) {
        if let Expression::Identifier(callee) = &expression.callee {
            self.push_identifier(callee, true, None);
        }
        walk_new_expression(self, expression);
    }

    fn visit_identifier_reference(&mut self, identifier: &IdentifierReference<'a>) {
        self.push_identifier(identifier, false, None);
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
