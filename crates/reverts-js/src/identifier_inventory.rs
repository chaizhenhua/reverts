//! AST-backed identifier inventory for generated JavaScript/TypeScript output.
//!
//! This is intentionally an inventory, not a rename pass: it tells the
//! decompile session how many AST identifier sites exist beyond the
//! module-scope symbol denominator without mutating source.

use std::{collections::BTreeMap, path::Path};

use oxc_allocator::Allocator;
use oxc_ast::{
    Visit,
    ast::{
        BindingIdentifier, ExportAllDeclaration, ExportNamedDeclaration, IdentifierReference,
        ImportDeclaration, ObjectProperty, Program, StaticMemberExpression,
    },
    visit::walk::{
        walk_export_all_declaration, walk_export_named_declaration, walk_import_declaration,
        walk_object_property, walk_static_member_expression,
    },
};
use oxc_parser::Parser;

use crate::commonjs_exports::static_property_key_name_ref;
use crate::errors::{JsError, ParseError, ParseGoal, Result};
use crate::parse::{parse_options_for, source_type_candidates};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdentifierInventoryStats {
    pub binding_identifiers: usize,
    pub identifier_references: usize,
    pub static_member_properties: usize,
    pub object_property_keys: usize,
    pub import_specifiers: usize,
    pub export_specifiers: usize,
    pub semantic_named_bindings: usize,
    pub semantic_pending_bindings: usize,
    pub semantic_pending_import_bindings: usize,
    pub semantic_pending_binding_names: BTreeMap<String, usize>,
}

impl IdentifierInventoryStats {
    #[must_use]
    pub fn total(&self) -> usize {
        self.binding_identifiers
            + self.identifier_references
            + self.static_member_properties
            + self.object_property_keys
            + self.import_specifiers
            + self.export_specifiers
    }
}

pub fn collect_identifier_inventory(
    source: &str,
    path_hint: Option<&Path>,
    goal: ParseGoal,
) -> Result<IdentifierInventoryStats> {
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

        let mut collector = IdentifierInventoryCollector::default();
        collector.visit_program(&parsed.program);
        return Ok(collector.stats);
    }

    Err(JsError::ParseFailed(errors))
}

#[derive(Default)]
struct IdentifierInventoryCollector {
    stats: IdentifierInventoryStats,
}

impl<'a> Visit<'a> for IdentifierInventoryCollector {
    fn visit_program(&mut self, program: &Program<'a>) {
        oxc_ast::visit::walk::walk_program(self, program);
    }

    fn visit_binding_identifier(&mut self, identifier: &BindingIdentifier<'a>) {
        let name = identifier.name.as_str();
        self.stats.binding_identifiers += 1;
        self.stats.semantic_pending_bindings += 1;
        *self
            .stats
            .semantic_pending_binding_names
            .entry(name.to_string())
            .or_default() += 1;
    }

    fn visit_identifier_reference(&mut self, _identifier: &IdentifierReference<'a>) {
        self.stats.identifier_references += 1;
    }

    fn visit_static_member_expression(&mut self, expression: &StaticMemberExpression<'a>) {
        self.stats.static_member_properties += 1;
        walk_static_member_expression(self, expression);
    }

    fn visit_object_property(&mut self, property: &ObjectProperty<'a>) {
        if !property.computed && static_property_key_name_ref(&property.key).is_some() {
            self.stats.object_property_keys += 1;
        }
        walk_object_property(self, property);
    }

    fn visit_import_declaration(&mut self, declaration: &ImportDeclaration<'a>) {
        self.stats.import_specifiers += declaration
            .specifiers
            .as_ref()
            .map_or(0, |specifiers| specifiers.len());
        if let Some(specifiers) = &declaration.specifiers {
            for _specifier in specifiers {
                self.stats.semantic_pending_import_bindings += 1;
            }
        }
        walk_import_declaration(self, declaration);
    }

    fn visit_export_named_declaration(&mut self, declaration: &ExportNamedDeclaration<'a>) {
        self.stats.export_specifiers += declaration.specifiers.len();
        walk_export_named_declaration(self, declaration);
    }

    fn visit_export_all_declaration(&mut self, declaration: &ExportAllDeclaration<'a>) {
        self.stats.export_specifiers += 1;
        walk_export_all_declaration(self, declaration);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::{ParseGoal, collect_identifier_inventory};

    #[test]
    fn counts_bindings_references_members_and_import_exports() {
        let source = r#"
            import value, { named as alias } from "pkg";
            const obj = { answer: 42, alias };
            export const out = obj.answer + alias;
        "#;

        let stats = collect_identifier_inventory(
            source,
            Some(Path::new("fixture.ts")),
            ParseGoal::TypeScript,
        )
        .expect("fixture should parse");

        assert_eq!(stats.import_specifiers, 2);
        assert_eq!(stats.export_specifiers, 0);
        assert!(stats.binding_identifiers >= 4);
        assert!(stats.identifier_references >= 3);
        assert_eq!(stats.static_member_properties, 1);
        assert_eq!(stats.object_property_keys, 2);
        assert_eq!(stats.semantic_pending_bindings, stats.binding_identifiers);
        assert_eq!(stats.semantic_named_bindings, 0);
        assert!(stats.total() >= 12);
    }

    #[test]
    fn marks_minified_bindings_as_semantic_pending() {
        let stats = collect_identifier_inventory(
            "const a = 1; function b(c) { return c + a; }",
            Some(Path::new("fixture.ts")),
            ParseGoal::TypeScript,
        )
        .expect("fixture should parse");

        assert_eq!(stats.binding_identifiers, 3);
        assert_eq!(stats.semantic_named_bindings, 0);
        assert_eq!(stats.semantic_pending_bindings, 3);
    }

    #[test]
    fn marks_placeholder_names_as_semantic_pending() {
        let stats = collect_identifier_inventory(
            "const semanticValue25 = 1; function semanticFunction2() { return semanticValue25; }",
            Some(Path::new("fixture.ts")),
            ParseGoal::TypeScript,
        )
        .expect("fixture should parse");

        assert_eq!(stats.binding_identifiers, 2);
        assert_eq!(stats.semantic_named_bindings, 0);
        assert_eq!(stats.semantic_pending_bindings, 2);
    }
}
