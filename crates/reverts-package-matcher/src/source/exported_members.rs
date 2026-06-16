//! Public-export-member discovery for a package source body. Combines an
//! OXC AST visitor over export declarations with the
//! [`commonjs_export_members_from_text`] text-based scanner as a fallback,
//! plus the small classification predicates that decide whether a discovered
//! identifier is strong enough to count as a usable public export.

use std::collections::BTreeSet;
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::{
    Visit,
    ast::{
        CallExpression, ExportAllDeclaration, ExportDefaultDeclaration,
        ExportDefaultDeclarationKind, ExportNamedDeclaration, Expression,
    },
    visit::walk::{
        walk_assignment_expression, walk_call_expression, walk_export_all_declaration,
        walk_export_default_declaration, walk_export_named_declaration,
    },
};
use oxc_parser::Parser;
use reverts_js::{ParseGoal, parse_options_for, source_type_candidates};

use super::ast_export_helpers::{
    commonjs_create_binding_export_member, commonjs_export_property_name,
    commonjs_module_exports_target, declaration_binding_names, module_export_name,
    object_define_property_export_member, object_expression_static_keys,
};
use super::commonjs_exports::commonjs_export_members_from_text;
use crate::package_helpers::normalize_hint_text;

#[must_use]
pub(crate) fn exported_members_from_source(path: &str, source: &str) -> BTreeSet<String> {
    let allocator = Allocator::default();
    for source_type in source_type_candidates(Some(Path::new(path)), ParseGoal::TypeScript) {
        let parsed = Parser::new(&allocator, source, source_type)
            .with_options(parse_options_for(source_type))
            .parse();
        if parsed.errors.is_empty() && !parsed.panicked {
            let mut visitor = ExportMemberCollector::default();
            visitor.visit_program(&parsed.program);
            visitor
                .members
                .extend(commonjs_export_members_from_text(source));
            return visitor
                .members
                .into_iter()
                .filter(|member| is_usable_export_member(member))
                .collect();
        }
    }
    commonjs_export_members_from_text(source)
        .into_iter()
        .filter(|member| is_usable_export_member(member))
        .collect()
}

#[derive(Debug, Default)]
struct ExportMemberCollector {
    members: BTreeSet<String>,
}

impl ExportMemberCollector {
    fn insert(&mut self, member: impl Into<String>) {
        let member = member.into();
        if is_usable_export_member(member.as_str()) {
            self.members.insert(member);
        }
    }
}

impl<'a> Visit<'a> for ExportMemberCollector {
    fn visit_export_named_declaration(&mut self, declaration: &ExportNamedDeclaration<'a>) {
        if let Some(declaration) = &declaration.declaration {
            for binding in declaration_binding_names(declaration) {
                self.insert(binding);
            }
        }
        for specifier in &declaration.specifiers {
            if let Some(exported) = module_export_name(&specifier.exported) {
                self.insert(exported);
            }
        }
        walk_export_named_declaration(self, declaration);
    }

    fn visit_export_default_declaration(&mut self, declaration: &ExportDefaultDeclaration<'a>) {
        match &declaration.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(function) => {
                if let Some(id) = &function.id {
                    self.insert(id.name.as_str());
                }
            }
            ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                if let Some(id) = &class.id {
                    self.insert(id.name.as_str());
                }
            }
            _ => {}
        }
        walk_export_default_declaration(self, declaration);
    }

    fn visit_export_all_declaration(&mut self, declaration: &ExportAllDeclaration<'a>) {
        if let Some(exported) = &declaration.exported
            && let Some(binding) = module_export_name(exported)
        {
            self.insert(binding);
        }
        walk_export_all_declaration(self, declaration);
    }

    fn visit_assignment_expression(&mut self, expression: &oxc_ast::ast::AssignmentExpression<'a>) {
        if expression.operator.is_assign() {
            if let Some(exported) = commonjs_export_property_name(&expression.left) {
                self.insert(exported);
            }
            if commonjs_module_exports_target(&expression.left) {
                match &expression.right {
                    Expression::ObjectExpression(object) => {
                        for member in object_expression_static_keys(object) {
                            self.insert(member);
                        }
                    }
                    Expression::Identifier(identifier) => {
                        self.insert(identifier.name.as_str());
                    }
                    Expression::FunctionExpression(function) => {
                        if let Some(id) = &function.id {
                            self.insert(id.name.as_str());
                        }
                    }
                    Expression::ClassExpression(class) => {
                        if let Some(id) = &class.id {
                            self.insert(id.name.as_str());
                        }
                    }
                    _ => {}
                }
            }
        }
        walk_assignment_expression(self, expression);
    }

    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if let Some(exported) = object_define_property_export_member(call) {
            self.insert(exported);
        }
        if let Some(exported) = commonjs_create_binding_export_member(call) {
            self.insert(exported);
        }
        walk_call_expression(self, call);
    }
}

#[must_use]
pub(crate) fn export_member_set_is_strong<'a>(members: impl Iterator<Item = &'a String>) -> bool {
    let members = members.collect::<Vec<_>>();
    !members.is_empty()
        && members
            .iter()
            .any(|member| is_specific_export_member(member.as_str()))
}

#[must_use]
pub(crate) fn is_usable_export_member(member: &str) -> bool {
    !matches!(member, "default" | "__esModule")
        && is_identifier_name(member)
        && is_specific_export_member(member)
}

#[must_use]
pub(crate) fn is_specific_export_member(member: &str) -> bool {
    let normalized = normalize_hint_text(member);
    normalized.len() >= 3
        && !matches!(
            normalized.as_str(),
            "get"
                | "set"
                | "has"
                | "map"
                | "key"
                | "keys"
                | "add"
                | "run"
                | "main"
                | "init"
                | "name"
                | "type"
                | "types"
                | "value"
                | "values"
                | "index"
        )
}

#[must_use]
pub(crate) fn is_identifier_name(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first == '$' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch == '$' || ch.is_ascii_alphanumeric())
}
