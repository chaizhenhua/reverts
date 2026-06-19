use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::Visit;
use oxc_ast::VisitMut;
use oxc_ast::ast::{
    ArrayExpressionElement, ArrowFunctionExpression, AssignmentExpression, AssignmentTarget,
    BindingPattern, BindingPatternKind, Declaration, ExportNamedDeclaration, Expression,
    FormalParameters, Function, FunctionBody, ImportDeclarationSpecifier, ObjectPropertyKind,
    Program, PropertyKey, ReturnStatement, SimpleAssignmentTarget, Statement, TSType, TSTypeName,
    TSTypeParameterInstantiation, TSTypeQueryExprName, UpdateExpression, VariableDeclaration,
    VariableDeclarationKind,
};
use oxc_ast::visit::walk::{walk_assignment_expression, walk_update_expression};
use oxc_ast::visit::walk_mut::{
    walk_arrow_function_expression, walk_function, walk_variable_declaration,
};
use oxc_parser::Parser;
use oxc_span::SPAN;
use oxc_syntax::scope::ScopeFlags;

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

    let mut function_annotator = FunctionSignatureAnnotator { builder: &builder };
    function_annotator.visit_program(program);
}

pub fn apply_import_member_type_queries_to_program<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
) {
    let import_namespaces = import_namespace_bindings(program);
    if import_namespaces.is_empty() {
        return;
    }
    let written_bindings = written_bindings_in_program(program);
    let builder = AstBuilder::new(allocator);
    let mut annotator = ImportMemberTypeQueryAnnotator {
        builder: &builder,
        written_bindings: &written_bindings,
        import_namespaces: &import_namespaces,
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
        let Some(type_annotation) = inferred_type_annotation_for_expression(builder, init, 0)
        else {
            continue;
        };
        declarator.id.type_annotation =
            Some(builder.alloc_ts_type_annotation(SPAN, type_annotation));
    }
}

struct FunctionSignatureAnnotator<'b, 'a> {
    builder: &'b AstBuilder<'a>,
}

impl<'a> VisitMut<'a> for FunctionSignatureAnnotator<'_, 'a> {
    fn visit_function(&mut self, function: &mut Function<'a>, flags: ScopeFlags) {
        annotate_function_parameters(self.builder, &mut function.params);
        annotate_function_return(self.builder, function);
        walk_function(self, function, flags);
    }

    fn visit_arrow_function_expression(&mut self, arrow: &mut ArrowFunctionExpression<'a>) {
        annotate_function_parameters(self.builder, &mut arrow.params);
        annotate_arrow_return(self.builder, arrow);
        walk_arrow_function_expression(self, arrow);
    }
}

fn annotate_function_parameters<'a>(
    builder: &AstBuilder<'a>,
    parameters: &mut FormalParameters<'a>,
) {
    for parameter in parameters.items.iter_mut() {
        annotate_defaulted_binding_pattern(builder, &mut parameter.pattern);
    }
}

fn annotate_defaulted_binding_pattern<'a>(
    builder: &AstBuilder<'a>,
    pattern: &mut BindingPattern<'a>,
) {
    let BindingPatternKind::AssignmentPattern(assignment) = &mut pattern.kind else {
        return;
    };
    if assignment.left.type_annotation.is_some() {
        return;
    }
    if binding_pattern_identifier(&assignment.left).is_none() {
        return;
    }
    let Some(type_annotation) =
        inferred_type_annotation_for_expression(builder, &assignment.right, 0)
    else {
        return;
    };
    assignment.left.type_annotation = Some(builder.alloc_ts_type_annotation(SPAN, type_annotation));
}

fn annotate_function_return<'a>(builder: &AstBuilder<'a>, function: &mut Function<'a>) {
    if function.return_type.is_some() || function.r#async || function.generator {
        return;
    }
    let Some(body) = &function.body else {
        return;
    };
    let Some(kind) = body_return_type(body) else {
        return;
    };
    let Some(type_annotation) = type_annotation_for_kind(builder, kind) else {
        return;
    };
    function.return_type = Some(builder.alloc_ts_type_annotation(SPAN, type_annotation));
}

fn annotate_arrow_return<'a>(builder: &AstBuilder<'a>, arrow: &mut ArrowFunctionExpression<'a>) {
    if arrow.return_type.is_some() || arrow.r#async {
        return;
    }
    let Some(kind) = body_return_type(&arrow.body) else {
        return;
    };
    let Some(type_annotation) = type_annotation_for_kind(builder, kind) else {
        return;
    };
    arrow.return_type = Some(builder.alloc_ts_type_annotation(SPAN, type_annotation));
}

fn body_return_type(body: &FunctionBody<'_>) -> Option<GeneratedTypeKind> {
    let mut collector = ReturnTypeCollector::default();
    collector.visit_function_body(body);
    if collector.conflict {
        return None;
    }
    collector.inferred
}

#[derive(Default)]
struct ReturnTypeCollector {
    inferred: Option<GeneratedTypeKind>,
    conflict: bool,
}

impl<'a> Visit<'a> for ReturnTypeCollector {
    fn visit_return_statement(&mut self, statement: &ReturnStatement<'a>) {
        let Some(argument) = &statement.argument else {
            self.conflict = true;
            return;
        };
        let Some(kind) = literal_type_kind(argument) else {
            self.conflict = true;
            return;
        };
        if let Some(existing) = self.inferred {
            if existing != kind {
                self.conflict = true;
            }
        } else {
            self.inferred = Some(kind);
        }
    }

    fn visit_function(&mut self, _function: &Function<'a>, _flags: ScopeFlags) {}

    fn visit_arrow_function_expression(&mut self, _arrow: &ArrowFunctionExpression<'a>) {}
}

struct ImportMemberTypeQueryAnnotator<'b, 'a> {
    builder: &'b AstBuilder<'a>,
    written_bindings: &'b BTreeSet<String>,
    import_namespaces: &'b BTreeSet<String>,
}

impl<'a> VisitMut<'a> for ImportMemberTypeQueryAnnotator<'_, 'a> {
    fn visit_variable_declaration(&mut self, declaration: &mut VariableDeclaration<'a>) {
        annotate_import_member_variable_declaration(
            self.builder,
            self.written_bindings,
            self.import_namespaces,
            declaration,
        );
        walk_variable_declaration(self, declaration);
    }
}

fn annotate_import_member_variable_declaration<'a>(
    builder: &AstBuilder<'a>,
    written_bindings: &BTreeSet<String>,
    import_namespaces: &BTreeSet<String>,
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
        let Some(type_annotation) = import_member_type_query(builder, import_namespaces, init)
        else {
            continue;
        };
        declarator.id.type_annotation =
            Some(builder.alloc_ts_type_annotation(SPAN, type_annotation));
    }
}

fn import_namespace_bindings(program: &Program<'_>) -> BTreeSet<String> {
    let mut namespaces = BTreeSet::new();
    for statement in &program.body {
        let Statement::ImportDeclaration(import) = statement else {
            continue;
        };
        let Some(specifiers) = &import.specifiers else {
            continue;
        };
        for specifier in specifiers {
            if let ImportDeclarationSpecifier::ImportNamespaceSpecifier(namespace) = specifier {
                namespaces.insert(namespace.local.name.as_str().to_string());
            }
        }
    }
    namespaces
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

fn inferred_type_annotation_for_expression<'a>(
    builder: &AstBuilder<'a>,
    expression: &Expression<'_>,
    depth: usize,
) -> Option<TSType<'a>> {
    if depth > 2 {
        return None;
    }
    if let Some(kind) = literal_type_kind(expression) {
        return type_annotation_for_kind(builder, kind);
    }
    match expression {
        Expression::ArrayExpression(array) => array_type_annotation(builder, array, depth),
        Expression::ObjectExpression(object) => {
            if object.properties.is_empty() || object.properties.len() > 12 {
                return None;
            }
            let mut members = builder.vec();
            for property in &object.properties {
                let ObjectPropertyKind::ObjectProperty(property) = property else {
                    return None;
                };
                if property.method || property.computed {
                    return None;
                }
                let PropertyKey::StaticIdentifier(identifier) = &property.key else {
                    return None;
                };
                let value_type =
                    inferred_type_annotation_for_expression(builder, &property.value, depth + 1)?;
                members.push(builder.ts_signature_property_signature(
                    SPAN,
                    false,
                    false,
                    false,
                    builder.property_key_identifier_name(SPAN, identifier.name.as_str()),
                    Some(builder.alloc_ts_type_annotation(SPAN, value_type)),
                ));
            }
            Some(builder.ts_type_type_literal(SPAN, members))
        }
        _ => None,
    }
}

fn array_type_annotation<'a>(
    builder: &AstBuilder<'a>,
    array: &oxc_ast::ast::ArrayExpression<'_>,
    depth: usize,
) -> Option<TSType<'a>> {
    if array.elements.is_empty() {
        return None;
    }
    let mut kind = None;
    for element in &array.elements {
        let element_expression = match element {
            ArrayExpressionElement::SpreadElement(_) | ArrayExpressionElement::Elision(_) => {
                return None;
            }
            other => other.to_expression(),
        };
        let element_kind = literal_type_kind(element_expression)?;
        if let Some(existing) = kind {
            if existing != element_kind {
                return None;
            }
        } else {
            kind = Some(element_kind);
        }
    }
    let element_type = type_annotation_for_kind(builder, kind?)?;
    if depth > 1 {
        return None;
    }
    Some(builder.ts_type_array_type(SPAN, element_type))
}

fn import_member_type_query<'a>(
    builder: &AstBuilder<'a>,
    import_namespaces: &BTreeSet<String>,
    expression: &Expression<'_>,
) -> Option<TSType<'a>> {
    let path = static_member_path(expression)?;
    if path.len() < 2 || !import_namespaces.contains(path[0]) {
        return None;
    }
    let mut type_name = builder.ts_type_name_identifier_reference(SPAN, path[0]);
    for segment in path.iter().skip(1) {
        let right = builder.identifier_name(SPAN, *segment);
        type_name = builder.ts_type_name_qualified_name(SPAN, type_name, right);
    }
    let query_name = match type_name {
        TSTypeName::IdentifierReference(identifier) => {
            TSTypeQueryExprName::IdentifierReference(identifier)
        }
        TSTypeName::QualifiedName(qualified) => TSTypeQueryExprName::QualifiedName(qualified),
    };
    Some(builder.ts_type_type_query(
        SPAN,
        query_name,
        None::<oxc_allocator::Box<'a, TSTypeParameterInstantiation<'a>>>,
    ))
}

fn static_member_path<'a>(expression: &'a Expression<'a>) -> Option<Vec<&'a str>> {
    match expression {
        Expression::Identifier(identifier) => Some(vec![identifier.name.as_str()]),
        Expression::StaticMemberExpression(member) if !member.optional => {
            let mut path = static_member_path(&member.object)?;
            path.push(member.property.name.as_str());
            Some(path)
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
