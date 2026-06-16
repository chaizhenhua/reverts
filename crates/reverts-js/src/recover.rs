use std::collections::{BTreeMap, BTreeSet};

use oxc_allocator::Allocator;
use oxc_ast::{
    AstBuilder, NONE, Visit, VisitMut,
    ast::{
        AssignmentExpression, BindingPatternKind, CallExpression, Class, ClassType, Expression,
        Function, FunctionType, IdentifierReference, ObjectProperty, ObjectPropertyKind, Program,
        PropertyKind, Statement, VariableDeclarationKind,
    },
    visit::walk::{walk_assignment_expression, walk_call_expression},
    visit::walk_mut::walk_object_property as walk_object_property_mut,
};
use oxc_semantic::SemanticBuilder;
use oxc_span::{GetSpan, SPAN};
use oxc_syntax::{reference::ReferenceId, symbol::SymbolId};

use crate::ReadabilityReport;
use crate::identifier::sanitize_identifier;
use crate::rename_hints::{
    commonjs_export_property_name, commonjs_module_exports_target, module_export_identifier_name,
    object_define_property_export_getter, property_key_readability_name,
};

#[derive(Debug, Clone)]
struct AliasCandidate {
    statement_index: usize,
    declaration_start: u32,
    alias_name: String,
    alias_symbol: SymbolId,
    source_symbol: SymbolId,
    source_name: String,
}

pub(crate) fn inline_simple_root_aliases<'a>(
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

pub(crate) fn recover_function_declarations<'a>(
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

pub(crate) fn recover_class_declarations<'a>(
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

pub(crate) fn apply_object_property_readability(
    program: &mut Program<'_>,
    report: &mut ReadabilityReport,
) {
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

pub(crate) fn recover_object_destructuring<'a>(
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
