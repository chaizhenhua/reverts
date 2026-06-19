use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::Visit;
use oxc_ast::VisitMut;
use oxc_ast::ast::{
    Argument, ArrayExpressionElement, ArrowFunctionExpression, AssignmentExpression,
    AssignmentTarget, BindingPattern, BindingPatternKind, CallExpression, Declaration,
    ExportNamedDeclaration, Expression, FormalParameters, Function, FunctionBody,
    ImportDeclarationSpecifier, ObjectPropertyKind, Program, PropertyKey, ReturnStatement,
    SimpleAssignmentTarget, Statement, TSType, TSTypeName, TSTypeParameterInstantiation,
    TSTypeQueryExprName, UpdateExpression, VariableDeclaration, VariableDeclarationKind,
};
use oxc_ast::visit::walk::{
    walk_arrow_function_expression as walk_arrow_function_expression_read,
    walk_assignment_expression, walk_call_expression, walk_function as walk_function_read,
    walk_update_expression, walk_variable_declaration as walk_variable_declaration_read,
};
use oxc_ast::visit::walk_mut::{
    walk_arrow_function_expression, walk_function, walk_variable_declaration,
};
use oxc_parser::Parser;
use oxc_semantic::{SemanticBuilder, SymbolTable};
use oxc_span::SPAN;
use oxc_syntax::scope::ScopeFlags;
use oxc_syntax::symbol::SymbolId;

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

#[derive(Debug, Clone, PartialEq, Eq)]
enum InferredExpressionType {
    Primitive(GeneratedTypeKind),
    Object(Vec<(String, InferredExpressionType)>),
    Array(Box<InferredExpressionType>),
    Union(Vec<InferredExpressionType>),
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

    let call_site_parameter_types = call_site_parameter_types(program);
    let mut function_annotator = FunctionSignatureAnnotator {
        builder: &builder,
        call_site_parameter_types: &call_site_parameter_types,
    };
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
    call_site_parameter_types: &'b BTreeMap<SymbolId, Vec<Option<InferredExpressionType>>>,
}

impl<'a> VisitMut<'a> for FunctionSignatureAnnotator<'_, 'a> {
    fn visit_variable_declaration(&mut self, declaration: &mut VariableDeclaration<'a>) {
        for declarator in declaration.declarations.iter_mut() {
            let Some(symbol_id) = binding_pattern_symbol_id(&declarator.id) else {
                continue;
            };
            let Some(init) = &mut declarator.init else {
                continue;
            };
            match init {
                Expression::FunctionExpression(function) => {
                    annotate_function_parameters(self.builder, &mut function.params);
                    annotate_call_site_parameters(
                        self.builder,
                        self.call_site_parameter_types,
                        symbol_id,
                        &mut function.params,
                    );
                    annotate_function_return(self.builder, function);
                }
                Expression::ArrowFunctionExpression(arrow) => {
                    annotate_function_parameters(self.builder, &mut arrow.params);
                    annotate_call_site_parameters(
                        self.builder,
                        self.call_site_parameter_types,
                        symbol_id,
                        &mut arrow.params,
                    );
                    annotate_arrow_return(self.builder, arrow);
                }
                _ => {}
            }
        }
        walk_variable_declaration(self, declaration);
    }

    fn visit_function(&mut self, function: &mut Function<'a>, flags: ScopeFlags) {
        annotate_function_parameters(self.builder, &mut function.params);
        if let Some(id) = &function.id
            && let Some(symbol_id) = id.symbol_id.get()
        {
            annotate_call_site_parameters(
                self.builder,
                self.call_site_parameter_types,
                symbol_id,
                &mut function.params,
            );
        }
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

fn annotate_call_site_parameters<'a>(
    builder: &AstBuilder<'a>,
    call_site_parameter_types: &BTreeMap<SymbolId, Vec<Option<InferredExpressionType>>>,
    symbol_id: SymbolId,
    parameters: &mut FormalParameters<'a>,
) {
    let Some(parameter_types) = call_site_parameter_types.get(&symbol_id) else {
        return;
    };
    for (index, parameter) in parameters.items.iter_mut().enumerate() {
        let Some(Some(inferred)) = parameter_types.get(index) else {
            continue;
        };
        annotate_call_site_binding_pattern(builder, &mut parameter.pattern, inferred);
    }
}

fn annotate_call_site_binding_pattern<'a>(
    builder: &AstBuilder<'a>,
    pattern: &mut BindingPattern<'a>,
    inferred: &InferredExpressionType,
) {
    match &mut pattern.kind {
        BindingPatternKind::BindingIdentifier(_) if pattern.type_annotation.is_none() => {
            let Some(type_annotation) =
                type_annotation_for_inferred_expression_type(builder, inferred)
            else {
                return;
            };
            pattern.type_annotation = Some(builder.alloc_ts_type_annotation(SPAN, type_annotation));
        }
        BindingPatternKind::AssignmentPattern(assignment)
            if assignment.left.type_annotation.is_none()
                && binding_pattern_identifier(&assignment.left).is_some() =>
        {
            let Some(type_annotation) =
                type_annotation_for_inferred_expression_type(builder, inferred)
            else {
                return;
            };
            assignment.left.type_annotation =
                Some(builder.alloc_ts_type_annotation(SPAN, type_annotation));
        }
        _ => {}
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
    let Some(inferred) = body_return_type(body) else {
        return;
    };
    let Some(type_annotation) = type_annotation_for_inferred_expression_type(builder, &inferred)
    else {
        return;
    };
    function.return_type = Some(builder.alloc_ts_type_annotation(SPAN, type_annotation));
}

fn annotate_arrow_return<'a>(builder: &AstBuilder<'a>, arrow: &mut ArrowFunctionExpression<'a>) {
    if arrow.return_type.is_some() || arrow.r#async {
        return;
    }
    let Some(inferred) = body_return_type(&arrow.body) else {
        return;
    };
    let Some(type_annotation) = type_annotation_for_inferred_expression_type(builder, &inferred)
    else {
        return;
    };
    arrow.return_type = Some(builder.alloc_ts_type_annotation(SPAN, type_annotation));
}

fn body_return_type(body: &FunctionBody<'_>) -> Option<InferredExpressionType> {
    let mut collector = ReturnTypeCollector::default();
    collector.visit_function_body(body);
    if collector.conflict {
        return None;
    }
    merged_return_type(collector.inferred)
}

fn merged_return_type(mut inferred: Vec<InferredExpressionType>) -> Option<InferredExpressionType> {
    merged_expression_type(&mut inferred)
}

fn merged_expression_type(
    inferred: &mut Vec<InferredExpressionType>,
) -> Option<InferredExpressionType> {
    if inferred.is_empty() {
        return None;
    }
    let mut unique = Vec::new();
    for ty in inferred.drain(..) {
        if !unique.contains(&ty) {
            unique.push(ty);
        }
    }
    if unique.len() == 1 {
        return Some(unique.remove(0));
    }
    if unique.len() <= 3
        && unique
            .iter()
            .all(|ty| matches!(ty, InferredExpressionType::Primitive(_)))
    {
        return Some(InferredExpressionType::Union(unique));
    }
    None
}

#[derive(Default)]
struct ReturnTypeCollector {
    inferred: Vec<InferredExpressionType>,
    conflict: bool,
}

impl<'a> Visit<'a> for ReturnTypeCollector {
    fn visit_return_statement(&mut self, statement: &ReturnStatement<'a>) {
        let Some(argument) = &statement.argument else {
            self.conflict = true;
            return;
        };
        let Some(inferred) = inferred_expression_type(argument, 0) else {
            self.conflict = true;
            return;
        };
        self.inferred.push(inferred);
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

fn binding_pattern_symbol_id(pattern: &BindingPattern<'_>) -> Option<SymbolId> {
    match &pattern.kind {
        BindingPatternKind::BindingIdentifier(identifier) => identifier.symbol_id.get(),
        BindingPatternKind::ObjectPattern(_)
        | BindingPatternKind::ArrayPattern(_)
        | BindingPatternKind::AssignmentPattern(_) => None,
    }
}

fn call_site_parameter_types(
    program: &Program<'_>,
) -> BTreeMap<SymbolId, Vec<Option<InferredExpressionType>>> {
    let semantic = SemanticBuilder::new().build(program).semantic;
    let callable_parameter_counts = callable_parameter_counts(program);
    if callable_parameter_counts.is_empty() {
        return BTreeMap::new();
    }

    let mut collector = CallSiteArgumentCollector {
        symbols: semantic.symbols(),
        callable_parameter_counts: &callable_parameter_counts,
        evidence: BTreeMap::new(),
    };
    collector.visit_program(program);

    collector
        .evidence
        .into_iter()
        .filter_map(|(symbol_id, parameter_evidence)| {
            let mut parameter_types = Vec::new();
            for mut evidence in parameter_evidence {
                if evidence.conflict {
                    parameter_types.push(None);
                    continue;
                }
                parameter_types.push(merged_expression_type(&mut evidence.types));
            }
            if parameter_types.iter().any(Option::is_some) {
                Some((symbol_id, parameter_types))
            } else {
                None
            }
        })
        .collect()
}

fn callable_parameter_counts(program: &Program<'_>) -> BTreeMap<SymbolId, usize> {
    let mut collector = CallableParameterCollector::default();
    collector.visit_program(program);
    collector
        .counts
        .into_iter()
        .filter(|(symbol_id, _)| !collector.conflicts.contains(symbol_id))
        .collect()
}

#[derive(Default)]
struct CallableParameterCollector {
    counts: BTreeMap<SymbolId, usize>,
    conflicts: BTreeSet<SymbolId>,
}

impl CallableParameterCollector {
    fn record(&mut self, symbol_id: SymbolId, parameters: &FormalParameters<'_>) {
        if parameters.rest.is_some() || parameters.items.is_empty() {
            return;
        }
        let count = parameters.items.len();
        if let Some(existing) = self.counts.get(&symbol_id)
            && *existing != count
        {
            self.conflicts.insert(symbol_id);
            return;
        }
        self.counts.insert(symbol_id, count);
    }
}

impl<'a> Visit<'a> for CallableParameterCollector {
    fn visit_variable_declaration(&mut self, declaration: &VariableDeclaration<'a>) {
        for declarator in &declaration.declarations {
            let Some(symbol_id) = binding_pattern_symbol_id(&declarator.id) else {
                continue;
            };
            let Some(init) = &declarator.init else {
                continue;
            };
            match init {
                Expression::FunctionExpression(function) => {
                    self.record(symbol_id, &function.params)
                }
                Expression::ArrowFunctionExpression(arrow) => self.record(symbol_id, &arrow.params),
                _ => {}
            }
        }
        walk_variable_declaration_read(self, declaration);
    }

    fn visit_function(&mut self, function: &Function<'a>, flags: ScopeFlags) {
        if let Some(id) = &function.id
            && let Some(symbol_id) = id.symbol_id.get()
        {
            self.record(symbol_id, &function.params);
        }
        walk_function_read(self, function, flags);
    }

    fn visit_arrow_function_expression(&mut self, arrow: &ArrowFunctionExpression<'a>) {
        walk_arrow_function_expression_read(self, arrow);
    }
}

#[derive(Default)]
struct ParameterTypeEvidence {
    types: Vec<InferredExpressionType>,
    conflict: bool,
}

struct CallSiteArgumentCollector<'b> {
    symbols: &'b SymbolTable,
    callable_parameter_counts: &'b BTreeMap<SymbolId, usize>,
    evidence: BTreeMap<SymbolId, Vec<ParameterTypeEvidence>>,
}

impl<'a> Visit<'a> for CallSiteArgumentCollector<'_> {
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        self.record_call(call);
        walk_call_expression(self, call);
    }
}

impl CallSiteArgumentCollector<'_> {
    fn record_call(&mut self, call: &CallExpression<'_>) {
        if call.optional
            || call
                .arguments
                .iter()
                .any(|arg| matches!(arg, Argument::SpreadElement(_)))
        {
            return;
        }
        let Expression::Identifier(callee) = &call.callee else {
            return;
        };
        let Some(reference_id) = callee.reference_id.get() else {
            return;
        };
        let Some(symbol_id) = self.symbols.get_reference(reference_id).symbol_id() else {
            return;
        };
        let Some(parameter_count) = self.callable_parameter_counts.get(&symbol_id).copied() else {
            return;
        };
        let evidence = self.evidence.entry(symbol_id).or_insert_with(|| {
            (0..parameter_count)
                .map(|_| ParameterTypeEvidence::default())
                .collect()
        });
        for (index, argument) in call.arguments.iter().take(parameter_count).enumerate() {
            let Some(expression) = argument_expression(argument) else {
                evidence[index].conflict = true;
                continue;
            };
            match inferred_expression_type(expression, 0) {
                Some(ty) => evidence[index].types.push(ty),
                None => evidence[index].conflict = true,
            }
        }
    }
}

fn argument_expression<'a>(argument: &'a Argument<'a>) -> Option<&'a Expression<'a>> {
    match argument {
        Argument::SpreadElement(_) => None,
        other => Some(other.to_expression()),
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
        Expression::UnaryExpression(unary)
            if matches!(
                unary.operator,
                oxc_ast::ast::UnaryOperator::UnaryNegation | oxc_ast::ast::UnaryOperator::UnaryPlus
            ) && matches!(&unary.argument, Expression::NumericLiteral(_)) =>
        {
            Some(GeneratedTypeKind::Number)
        }
        Expression::UnaryExpression(unary)
            if unary.operator == oxc_ast::ast::UnaryOperator::LogicalNot
                && matches!(
                    &unary.argument,
                    Expression::NumericLiteral(_) | Expression::BooleanLiteral(_)
                ) =>
        {
            Some(GeneratedTypeKind::Boolean)
        }
        Expression::UnaryExpression(unary)
            if unary.operator == oxc_ast::ast::UnaryOperator::Typeof =>
        {
            Some(GeneratedTypeKind::String)
        }
        Expression::UnaryExpression(unary)
            if unary.operator == oxc_ast::ast::UnaryOperator::Delete =>
        {
            Some(GeneratedTypeKind::Boolean)
        }
        Expression::BinaryExpression(binary)
            if binary.operator.is_equality()
                || binary.operator.is_compare()
                || matches!(
                    binary.operator,
                    oxc_ast::ast::BinaryOperator::In | oxc_ast::ast::BinaryOperator::Instanceof
                ) =>
        {
            Some(GeneratedTypeKind::Boolean)
        }
        Expression::BinaryExpression(binary)
            if numeric_binary_operator_returns_number(binary.operator)
                && expression_has_scalar_type(&binary.left, GeneratedTypeKind::Number)
                && expression_has_scalar_type(&binary.right, GeneratedTypeKind::Number) =>
        {
            Some(GeneratedTypeKind::Number)
        }
        Expression::CallExpression(call) => builtin_call_type(call).and_then(|ty| {
            if let InferredExpressionType::Primitive(kind) = ty {
                Some(kind)
            } else {
                None
            }
        }),
        _ => None,
    }
}

fn expression_has_scalar_type(expression: &Expression<'_>, kind: GeneratedTypeKind) -> bool {
    literal_type_kind(expression) == Some(kind)
}

fn numeric_binary_operator_returns_number(operator: oxc_ast::ast::BinaryOperator) -> bool {
    matches!(
        operator,
        oxc_ast::ast::BinaryOperator::Addition
            | oxc_ast::ast::BinaryOperator::Subtraction
            | oxc_ast::ast::BinaryOperator::Multiplication
            | oxc_ast::ast::BinaryOperator::Division
            | oxc_ast::ast::BinaryOperator::Remainder
            | oxc_ast::ast::BinaryOperator::Exponential
            | oxc_ast::ast::BinaryOperator::ShiftLeft
            | oxc_ast::ast::BinaryOperator::ShiftRight
            | oxc_ast::ast::BinaryOperator::ShiftRightZeroFill
            | oxc_ast::ast::BinaryOperator::BitwiseOR
            | oxc_ast::ast::BinaryOperator::BitwiseXOR
            | oxc_ast::ast::BinaryOperator::BitwiseAnd
    )
}

fn inferred_type_annotation_for_expression<'a>(
    builder: &AstBuilder<'a>,
    expression: &Expression<'_>,
    depth: usize,
) -> Option<TSType<'a>> {
    let inferred = inferred_expression_type(expression, depth)?;
    type_annotation_for_inferred_expression_type(builder, &inferred)
}

fn inferred_expression_type(
    expression: &Expression<'_>,
    depth: usize,
) -> Option<InferredExpressionType> {
    if depth > 2 {
        return None;
    }
    if let Some(kind) = literal_type_kind(expression) {
        return Some(InferredExpressionType::Primitive(kind));
    }
    match expression {
        Expression::ArrayExpression(array) => array_expression_type(array, depth),
        Expression::ObjectExpression(object) => object_expression_type(object, depth),
        Expression::CallExpression(call) => builtin_call_type(call),
        Expression::ConditionalExpression(conditional) => {
            let mut types = vec![
                inferred_expression_type(&conditional.consequent, depth + 1)?,
                inferred_expression_type(&conditional.alternate, depth + 1)?,
            ];
            merged_expression_type(&mut types)
        }
        Expression::LogicalExpression(logical)
            if matches!(
                logical.operator,
                oxc_ast::ast::LogicalOperator::Or | oxc_ast::ast::LogicalOperator::Coalesce
            ) =>
        {
            let mut types = vec![
                inferred_expression_type(&logical.left, depth + 1)?,
                inferred_expression_type(&logical.right, depth + 1)?,
            ];
            merged_expression_type(&mut types)
        }
        Expression::StaticMemberExpression(_) => global_static_member_type(expression),
        _ => None,
    }
}

fn builtin_call_type(call: &oxc_ast::ast::CallExpression<'_>) -> Option<InferredExpressionType> {
    if call.optional || call.type_parameters.is_some() {
        return None;
    }
    let path = static_member_path(&call.callee)?;
    let kind = match path.as_slice() {
        ["String"] | ["JSON", "stringify"] => GeneratedTypeKind::String,
        ["Number"]
        | ["Date", "now"]
        | ["Date", "parse"]
        | ["Math", "abs"]
        | ["Math", "acos"]
        | ["Math", "acosh"]
        | ["Math", "asin"]
        | ["Math", "asinh"]
        | ["Math", "atan"]
        | ["Math", "atan2"]
        | ["Math", "atanh"]
        | ["Math", "cbrt"]
        | ["Math", "ceil"]
        | ["Math", "clz32"]
        | ["Math", "cos"]
        | ["Math", "cosh"]
        | ["Math", "exp"]
        | ["Math", "expm1"]
        | ["Math", "floor"]
        | ["Math", "fround"]
        | ["Math", "hypot"]
        | ["Math", "imul"]
        | ["Math", "log"]
        | ["Math", "log10"]
        | ["Math", "log1p"]
        | ["Math", "log2"]
        | ["Math", "max"]
        | ["Math", "min"]
        | ["Math", "pow"]
        | ["Math", "random"]
        | ["Math", "round"]
        | ["Math", "sign"]
        | ["Math", "sin"]
        | ["Math", "sinh"]
        | ["Math", "sqrt"]
        | ["Math", "tan"]
        | ["Math", "tanh"]
        | ["Math", "trunc"] => GeneratedTypeKind::Number,
        ["Boolean"]
        | ["Array", "isArray"]
        | ["Number", "isFinite"]
        | ["Number", "isInteger"]
        | ["Number", "isNaN"]
        | ["Number", "isSafeInteger"] => GeneratedTypeKind::Boolean,
        ["Object", "keys"] => {
            return Some(InferredExpressionType::Array(Box::new(
                InferredExpressionType::Primitive(GeneratedTypeKind::String),
            )));
        }
        _ => return None,
    };
    Some(InferredExpressionType::Primitive(kind))
}

fn global_static_member_type(expression: &Expression<'_>) -> Option<InferredExpressionType> {
    let path = static_member_path(expression)?;
    let kind = match path.as_slice() {
        ["Number", "EPSILON"]
        | ["Number", "MAX_SAFE_INTEGER"]
        | ["Number", "MAX_VALUE"]
        | ["Number", "MIN_SAFE_INTEGER"]
        | ["Number", "MIN_VALUE"]
        | ["Number", "NaN"]
        | ["Number", "NEGATIVE_INFINITY"]
        | ["Number", "POSITIVE_INFINITY"]
        | ["Math", "E"]
        | ["Math", "LN10"]
        | ["Math", "LN2"]
        | ["Math", "LOG10E"]
        | ["Math", "LOG2E"]
        | ["Math", "PI"]
        | ["Math", "SQRT1_2"]
        | ["Math", "SQRT2"] => GeneratedTypeKind::Number,
        ["process", "env", _] => {
            return Some(InferredExpressionType::Union(vec![
                InferredExpressionType::Primitive(GeneratedTypeKind::String),
                InferredExpressionType::Primitive(GeneratedTypeKind::Undefined),
            ]));
        }
        _ => return None,
    };
    Some(InferredExpressionType::Primitive(kind))
}

fn object_expression_type(
    object: &oxc_ast::ast::ObjectExpression<'_>,
    depth: usize,
) -> Option<InferredExpressionType> {
    if object.properties.is_empty() || object.properties.len() > 12 {
        return None;
    }
    let mut members = Vec::new();
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
        let value_type = inferred_expression_type(&property.value, depth + 1)?;
        members.push((identifier.name.as_str().to_string(), value_type));
    }
    Some(InferredExpressionType::Object(members))
}

fn array_expression_type(
    array: &oxc_ast::ast::ArrayExpression<'_>,
    depth: usize,
) -> Option<InferredExpressionType> {
    if array.elements.is_empty() {
        return None;
    }
    if depth > 1 {
        return None;
    }
    let mut element_types = Vec::<InferredExpressionType>::new();
    for element in &array.elements {
        let element_expression = match element {
            ArrayExpressionElement::SpreadElement(_) | ArrayExpressionElement::Elision(_) => {
                return None;
            }
            other => other.to_expression(),
        };
        let element_type = inferred_expression_type(element_expression, depth + 1)?;
        if !element_types.contains(&element_type) {
            element_types.push(element_type);
        }
    }
    if element_types.len() == 1 {
        return Some(InferredExpressionType::Array(Box::new(
            element_types.remove(0),
        )));
    }
    if element_types.len() > 3
        || !element_types
            .iter()
            .all(|ty| matches!(ty, InferredExpressionType::Primitive(_)))
    {
        return None;
    }
    Some(InferredExpressionType::Array(Box::new(
        InferredExpressionType::Union(element_types),
    )))
}

fn type_annotation_for_inferred_expression_type<'a>(
    builder: &AstBuilder<'a>,
    inferred: &InferredExpressionType,
) -> Option<TSType<'a>> {
    match inferred {
        InferredExpressionType::Primitive(kind) => type_annotation_for_kind(builder, *kind),
        InferredExpressionType::Object(properties) => {
            let mut members = builder.vec();
            for (name, value_type) in properties {
                let value_type = type_annotation_for_inferred_expression_type(builder, value_type)?;
                members.push(builder.ts_signature_property_signature(
                    SPAN,
                    false,
                    false,
                    false,
                    builder.property_key_identifier_name(SPAN, name.as_str()),
                    Some(builder.alloc_ts_type_annotation(SPAN, value_type)),
                ));
            }
            Some(builder.ts_type_type_literal(SPAN, members))
        }
        InferredExpressionType::Array(element) => {
            let mut element_type = type_annotation_for_inferred_expression_type(builder, element)?;
            if matches!(element.as_ref(), InferredExpressionType::Union(_)) {
                element_type = builder.ts_type_parenthesized_type(SPAN, element_type);
            }
            Some(builder.ts_type_array_type(SPAN, element_type))
        }
        InferredExpressionType::Union(types) => {
            let mut members = builder.vec();
            for ty in types {
                members.push(type_annotation_for_inferred_expression_type(builder, ty)?);
            }
            Some(builder.ts_type_union_type(SPAN, members))
        }
    }
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
