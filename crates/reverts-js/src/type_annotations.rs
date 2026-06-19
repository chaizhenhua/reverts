use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::Visit;
use oxc_ast::VisitMut;
use oxc_ast::ast::{
    AssignmentExpression, AssignmentTarget, BindingPattern, BindingPatternKind, Declaration,
    ExportNamedDeclaration, Expression, Program, SimpleAssignmentTarget, Statement, TSType,
    UpdateExpression, VariableDeclaration, VariableDeclarationKind,
};
use oxc_ast::visit::walk::{walk_assignment_expression, walk_update_expression};
use oxc_ast::visit::walk_mut::walk_variable_declaration;
use oxc_parser::Parser;
use oxc_span::SPAN;

use crate::errors::{JsError, ParseError, ParseGoal, Result};
use crate::parse::{parse_options_for, source_type_candidates};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GeneratedTypeKind {
    Unknown,
    Never,
    String,
    Number,
    Boolean,
    BigInt,
    Null,
    Undefined,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedTypeAnnotation {
    pub binding: String,
    pub kind: GeneratedTypeKind,
}

impl GeneratedTypeAnnotation {
    #[must_use]
    pub fn new(binding: impl Into<String>, kind: GeneratedTypeKind) -> Self {
        Self {
            binding: binding.into(),
            kind,
        }
    }
}

pub fn collect_top_level_literal_type_annotations(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<Vec<GeneratedTypeAnnotation>> {
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

        let mut annotations = Vec::new();
        for statement in &parsed.program.body {
            collect_statement_literal_annotations(statement, &mut annotations);
        }
        return Ok(annotations);
    }

    Err(JsError::ParseFailed(errors))
}

pub fn apply_type_annotations_to_program<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    annotations: &[GeneratedTypeAnnotation],
    infer_literal_types: bool,
) {
    let annotation_map = annotations
        .iter()
        .filter(|annotation| !matches!(annotation.kind, GeneratedTypeKind::Unknown))
        .map(|annotation| (annotation.binding.as_str(), annotation.kind))
        .collect::<BTreeMap<_, _>>();

    let builder = AstBuilder::new(allocator);
    if !annotation_map.is_empty() {
        for statement in program.body.iter_mut() {
            apply_statement_type_annotations(&builder, &annotation_map, statement);
        }
    }
    if !infer_literal_types {
        return;
    }

    let written_bindings = written_bindings_in_program(program);
    let mut annotator = SafeLiteralInitializerAnnotator {
        builder: &builder,
        written_bindings: &written_bindings,
    };
    annotator.visit_program(program);
}

fn collect_statement_literal_annotations(
    statement: &Statement<'_>,
    annotations: &mut Vec<GeneratedTypeAnnotation>,
) {
    match statement {
        Statement::VariableDeclaration(declaration) => {
            collect_variable_declaration_literal_annotations(declaration, annotations);
        }
        Statement::ExportNamedDeclaration(declaration) => {
            collect_export_named_literal_annotations(declaration, annotations);
        }
        _ => {}
    }
}

fn collect_export_named_literal_annotations(
    declaration: &ExportNamedDeclaration<'_>,
    annotations: &mut Vec<GeneratedTypeAnnotation>,
) {
    if let Some(Declaration::VariableDeclaration(variable)) = &declaration.declaration {
        collect_variable_declaration_literal_annotations(variable, annotations);
    }
}

fn collect_variable_declaration_literal_annotations(
    declaration: &VariableDeclaration<'_>,
    annotations: &mut Vec<GeneratedTypeAnnotation>,
) {
    for declarator in &declaration.declarations {
        let Some(init) = &declarator.init else {
            continue;
        };
        let Some(kind) = literal_type_kind(init) else {
            continue;
        };
        let Some(binding) = binding_pattern_identifier(&declarator.id) else {
            continue;
        };
        annotations.push(GeneratedTypeAnnotation::new(binding, kind));
    }
}

fn apply_statement_type_annotations<'a>(
    builder: &AstBuilder<'a>,
    annotations: &BTreeMap<&str, GeneratedTypeKind>,
    statement: &mut Statement<'a>,
) {
    match statement {
        Statement::VariableDeclaration(declaration) => {
            apply_variable_declaration_type_annotations(builder, annotations, declaration);
        }
        Statement::ExportNamedDeclaration(declaration) => {
            if let Some(Declaration::VariableDeclaration(variable)) = &mut declaration.declaration {
                apply_variable_declaration_type_annotations(builder, annotations, variable);
            }
        }
        _ => {}
    }
}

fn apply_variable_declaration_type_annotations<'a>(
    builder: &AstBuilder<'a>,
    annotations: &BTreeMap<&str, GeneratedTypeKind>,
    declaration: &mut VariableDeclaration<'a>,
) {
    for declarator in declaration.declarations.iter_mut() {
        if declarator.id.type_annotation.is_some() {
            continue;
        }
        let Some(binding) = binding_pattern_identifier(&declarator.id) else {
            continue;
        };
        let Some(kind) = annotations.get(binding) else {
            continue;
        };
        let Some(type_annotation) = type_annotation_for_kind(builder, *kind) else {
            continue;
        };
        declarator.id.type_annotation =
            Some(builder.alloc_ts_type_annotation(SPAN, type_annotation));
    }
}

struct SafeLiteralInitializerAnnotator<'b, 'a> {
    builder: &'b AstBuilder<'a>,
    written_bindings: &'b BTreeSet<String>,
}

impl<'a> VisitMut<'a> for SafeLiteralInitializerAnnotator<'_, 'a> {
    fn visit_variable_declaration(&mut self, declaration: &mut VariableDeclaration<'a>) {
        annotate_safe_variable_declaration(self.builder, self.written_bindings, declaration);
        walk_variable_declaration(self, declaration);
    }
}

fn annotate_safe_variable_declaration<'a>(
    builder: &AstBuilder<'a>,
    written_bindings: &BTreeSet<String>,
    declaration: &mut VariableDeclaration<'a>,
) {
    for declarator in declaration.declarations.iter_mut() {
        if declarator.id.type_annotation.is_some() {
            continue;
        }
        let Some(binding) = binding_pattern_identifier(&declarator.id) else {
            continue;
        };
        if declaration.kind != VariableDeclarationKind::Const && written_bindings.contains(binding)
        {
            continue;
        }
        let Some(init) = &declarator.init else {
            continue;
        };
        let Some(kind) = literal_type_kind(init) else {
            continue;
        };
        let Some(type_annotation) = type_annotation_for_kind(builder, kind) else {
            continue;
        };
        declarator.id.type_annotation =
            Some(builder.alloc_ts_type_annotation(SPAN, type_annotation));
    }
}

fn written_bindings_in_program(program: &Program<'_>) -> BTreeSet<String> {
    let mut collector = WrittenBindingCollector::default();
    collector.visit_program(program);
    collector.bindings
}

#[derive(Default)]
struct WrittenBindingCollector {
    bindings: BTreeSet<String>,
}

impl<'a> Visit<'a> for WrittenBindingCollector {
    fn visit_assignment_expression(&mut self, expression: &AssignmentExpression<'a>) {
        if let Some(binding) = assignment_target_identifier(&expression.left) {
            self.bindings.insert(binding.to_string());
        }
        walk_assignment_expression(self, expression);
    }

    fn visit_update_expression(&mut self, expression: &UpdateExpression<'a>) {
        if let SimpleAssignmentTarget::AssignmentTargetIdentifier(identifier) = &expression.argument
        {
            self.bindings.insert(identifier.name.as_str().to_string());
        }
        walk_update_expression(self, expression);
    }
}

fn assignment_target_identifier<'a>(target: &'a AssignmentTarget<'a>) -> Option<&'a str> {
    match target {
        AssignmentTarget::AssignmentTargetIdentifier(identifier) => Some(identifier.name.as_str()),
        AssignmentTarget::ComputedMemberExpression(_)
        | AssignmentTarget::StaticMemberExpression(_)
        | AssignmentTarget::PrivateFieldExpression(_)
        | AssignmentTarget::TSAsExpression(_)
        | AssignmentTarget::TSSatisfiesExpression(_)
        | AssignmentTarget::TSNonNullExpression(_)
        | AssignmentTarget::TSTypeAssertion(_)
        | AssignmentTarget::TSInstantiationExpression(_)
        | AssignmentTarget::ArrayAssignmentTarget(_)
        | AssignmentTarget::ObjectAssignmentTarget(_) => None,
    }
}

fn binding_pattern_identifier<'a>(pattern: &'a BindingPattern<'_>) -> Option<&'a str> {
    match &pattern.kind {
        BindingPatternKind::BindingIdentifier(identifier) => Some(identifier.name.as_str()),
        BindingPatternKind::ObjectPattern(_)
        | BindingPatternKind::ArrayPattern(_)
        | BindingPatternKind::AssignmentPattern(_) => None,
    }
}

fn literal_type_kind(expression: &Expression<'_>) -> Option<GeneratedTypeKind> {
    match expression {
        Expression::StringLiteral(_) => Some(GeneratedTypeKind::String),
        Expression::NumericLiteral(_) => Some(GeneratedTypeKind::Number),
        Expression::BooleanLiteral(_) => Some(GeneratedTypeKind::Boolean),
        Expression::BigIntLiteral(_) => Some(GeneratedTypeKind::BigInt),
        Expression::NullLiteral(_) => Some(GeneratedTypeKind::Null),
        Expression::Identifier(identifier) if identifier.name.as_str() == "undefined" => {
            Some(GeneratedTypeKind::Undefined)
        }
        Expression::UnaryExpression(unary)
            if unary.operator == oxc_ast::ast::UnaryOperator::Void =>
        {
            Some(GeneratedTypeKind::Undefined)
        }
        _ => None,
    }
}

fn type_annotation_for_kind<'a>(
    builder: &AstBuilder<'a>,
    kind: GeneratedTypeKind,
) -> Option<TSType<'a>> {
    match kind {
        GeneratedTypeKind::Unknown => Some(builder.ts_type_unknown_keyword(SPAN)),
        GeneratedTypeKind::Never => Some(builder.ts_type_never_keyword(SPAN)),
        GeneratedTypeKind::String => Some(builder.ts_type_string_keyword(SPAN)),
        GeneratedTypeKind::Number => Some(builder.ts_type_number_keyword(SPAN)),
        GeneratedTypeKind::Boolean => Some(builder.ts_type_boolean_keyword(SPAN)),
        GeneratedTypeKind::BigInt => Some(builder.ts_type_big_int_keyword(SPAN)),
        GeneratedTypeKind::Null => Some(builder.ts_type_null_keyword(SPAN)),
        GeneratedTypeKind::Undefined => Some(builder.ts_type_undefined_keyword(SPAN)),
    }
}
