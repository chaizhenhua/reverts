//! Source-side import-specifier collector.
//!
//! Walks an OXC-parsed program for every dependency edge a JS/TS module
//! declares — static `import`/`export ... from`, dynamic `import("...")`,
//! and CommonJS `require("...")` / `require.resolve("...")` — in source
//! order. Returns the bare specifier strings only. Used by the cross-
//! project module matcher to hydrate the ref side's dependency graph
//! when the project was ingested without `module_dependencies` rows.

use oxc_allocator::Allocator;
use oxc_ast::Visit;
use oxc_ast::ast::{
    Argument, CallExpression, ExportAllDeclaration, ExportNamedDeclaration, Expression,
    ImportDeclaration, ImportExpression,
};
use oxc_parser::Parser;
use oxc_span::SourceType;

use crate::parse_options_for;

#[must_use]
pub fn extract_import_specifiers(source: &str) -> Vec<String> {
    let alloc = Allocator::default();
    let source_type = SourceType::default().with_typescript(true).with_jsx(true);
    let parsed = Parser::new(&alloc, source, source_type)
        .with_options(parse_options_for(source_type))
        .parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return Vec::new();
    }
    let mut collector = Collector { out: Vec::new() };
    collector.visit_program(&parsed.program);
    collector.out
}

struct Collector {
    out: Vec<String>,
}

impl<'a> Visit<'a> for Collector {
    fn visit_import_declaration(&mut self, decl: &ImportDeclaration<'a>) {
        self.out.push(decl.source.value.as_str().to_string());
    }

    fn visit_export_all_declaration(&mut self, decl: &ExportAllDeclaration<'a>) {
        self.out.push(decl.source.value.as_str().to_string());
    }

    fn visit_export_named_declaration(&mut self, decl: &ExportNamedDeclaration<'a>) {
        if let Some(source) = &decl.source {
            self.out.push(source.value.as_str().to_string());
        }
    }

    fn visit_import_expression(&mut self, expr: &ImportExpression<'a>) {
        if let Expression::StringLiteral(s) = &expr.source {
            self.out.push(s.value.as_str().to_string());
        }
        oxc_ast::visit::walk::walk_import_expression(self, expr);
    }

    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        let is_require = match &call.callee {
            Expression::Identifier(id) => id.name.as_str() == "require",
            Expression::StaticMemberExpression(member) => {
                matches!(&member.object, Expression::Identifier(id) if id.name.as_str() == "require")
                    && member.property.name.as_str() == "resolve"
            }
            _ => false,
        };
        if is_require && let Some(Argument::StringLiteral(s)) = call.arguments.first() {
            self.out.push(s.value.as_str().to_string());
        }
        oxc_ast::visit::walk::walk_call_expression(self, call);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_up_static_import_and_export_from() {
        let src = r#"
            import a from "lodash";
            import { b } from "./b";
            export { c } from "../c";
            export * from "d";
        "#;
        let specs = extract_import_specifiers(src);
        assert_eq!(specs, vec!["lodash", "./b", "../c", "d"]);
    }

    #[test]
    fn picks_up_dynamic_import_and_require() {
        let src = r#"
            const a = require("foo");
            const b = require.resolve("bar");
            import("./baz").then(m => m.x);
        "#;
        let specs = extract_import_specifiers(src);
        assert_eq!(specs, vec!["foo", "bar", "./baz"]);
    }

    #[test]
    fn ignores_non_string_imports() {
        let src = r#"
            const x = "dyn";
            import(x).then(m => m);
        "#;
        let specs = extract_import_specifiers(src);
        assert!(specs.is_empty());
    }
}
