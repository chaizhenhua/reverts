use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Expression, Program, Statement};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_expression;
use oxc_span::SPAN;
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `ArrowBodyBlocked` rewrites the expression-form arrow body into the
/// explicit block-with-return form:
///
/// ```js
/// // expression-form (minifier-preferred for short bodies)
/// (a) => a + 1
///
/// // block-form, semantically identical
/// (a) => { return a + 1; }
/// ```
///
/// Both forms produce the same value but the AST shape differs: the
/// expression-form body is an `ExpressionStatement` while the block
/// form is a `ReturnStatement`. The cascade matcher's AST and CFG
/// axes consequently diverge for what is the same function body —
/// the cached source and the minified bundle, given typical author
/// style versus minifier output, often spell the same arrow each
/// of the two ways.
///
/// Normalising both forms to the block-with-return shape (the more
/// explicit, human-readable direction) closes the gap.
pub struct ArrowBodyBlocked;

impl NormalizationPass for ArrowBodyBlocked {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::ArrowBodyBlocked
    }
    fn version(&self) -> u32 {
        1
    }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut visitor = Blocker {
            builder: AstBuilder::new(alloc),
        };
        visitor.visit_program(program);
    }
}

struct Blocker<'a> {
    builder: AstBuilder<'a>,
}

impl<'a> VisitMut<'a> for Blocker<'a> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        walk_expression(self, expr);
        let Expression::ArrowFunctionExpression(arrow_box) = expr else {
            return;
        };
        if !arrow_box.expression {
            return;
        }
        // The expression-body arrow's FunctionBody has exactly one
        // ExpressionStatement wrapping the value. Replace it with a
        // ReturnStatement and flip the `expression` flag.
        if arrow_box.body.statements.len() != 1 {
            return;
        }
        // Take ownership: swap out the body's statements with a fresh
        // empty vec, work on the old, then push back.
        let placeholder = oxc_allocator::Vec::new_in(self.builder.allocator);
        let old_statements = mem::replace(&mut arrow_box.body.statements, placeholder);
        let mut iter = old_statements.into_iter();
        let stmt = iter.next().expect("len==1 checked above");
        let Statement::ExpressionStatement(es_box) = stmt else {
            // Some other statement kind — leave body empty would be
            // wrong, so put it back unchanged.
            let mut put_back = oxc_allocator::Vec::new_in(self.builder.allocator);
            put_back.push(stmt);
            arrow_box.body.statements = put_back;
            return;
        };
        let es = es_box.unbox();
        let return_stmt = Statement::ReturnStatement(
            self.builder
                .alloc(self.builder.return_statement(SPAN, Some(es.expression))),
        );
        let mut new_stmts = oxc_allocator::Vec::new_in(self.builder.allocator);
        new_stmts.push(return_stmt);
        arrow_box.body.statements = new_stmts;
        arrow_box.expression = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn rewrites_expression_arrow_to_block_return() {
        let out = apply_to_source(&ArrowBodyBlocked, "var f = (a) => a + 1;").expect("parse");
        assert!(out.contains("return a + 1"), "got: {out}");
    }

    #[test]
    fn rewrites_zero_arg_arrow() {
        let out = apply_to_source(&ArrowBodyBlocked, "var g = () => 42;").expect("parse");
        assert!(out.contains("return 42"), "got: {out}");
    }

    #[test]
    fn rewrites_nested_arrow_in_higher_order() {
        let out = apply_to_source(
            &ArrowBodyBlocked,
            "function h(xs) { return xs.map(x => x * 2); }",
        )
        .expect("parse");
        assert!(out.contains("return x * 2"), "got: {out}");
    }

    #[test]
    fn leaves_block_form_arrow_alone() {
        let src = "var f = (a) => { return a + 1; };";
        let out = apply_to_source(&ArrowBodyBlocked, src).expect("parse");
        assert!(out.contains("return a + 1"), "got: {out}");
        // Should already have been block-form; idempotent.
        let again = apply_to_source(&ArrowBodyBlocked, &out).expect("parse");
        assert_eq!(out, again);
    }

    #[test]
    fn idempotent_on_plain_function() {
        let src = "function add(a, b) { return a + b; }";
        let first = apply_to_source(&ArrowBodyBlocked, src).expect("first");
        let second = apply_to_source(&ArrowBodyBlocked, &first).expect("second");
        assert_eq!(first, second);
    }
}
