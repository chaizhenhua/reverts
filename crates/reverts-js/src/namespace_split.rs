use std::collections::{BTreeMap, BTreeSet};

use oxc_allocator::Allocator;
use oxc_ast::{
    AstBuilder, Visit, VisitMut,
    ast::{
        AssignmentExpression, Expression, ImportDeclaration, ImportDeclarationSpecifier,
        ImportOrExportKind, ModuleExportName, Program, SimpleAssignmentTarget, Statement,
        StaticMemberExpression, UnaryExpression,
    },
    visit::walk::{
        walk_assignment_expression, walk_static_member_expression, walk_unary_expression,
        walk_update_expression,
    },
    visit::walk_mut::walk_expression as walk_expression_mut,
};
use oxc_semantic::SemanticBuilder;
use oxc_span::SPAN;
use oxc_syntax::{operator::UnaryOperator, reference::ReferenceId, symbol::SymbolId};

use crate::ReadabilityReport;
use crate::identifier::sanitize_identifier;

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

pub(crate) fn split_safe_namespace_imports<'a>(
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

pub(crate) fn merge_and_sort_named_imports<'a>(
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
                if let Some(first_index) = first_by_source.get(&source).copied()
                    && let Statement::ImportDeclaration(first) = &next[first_index]
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
