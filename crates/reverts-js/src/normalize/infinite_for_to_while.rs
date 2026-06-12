use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Program, Statement};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::{walk_block_statement, walk_function_body};
use oxc_span::SPAN;
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `InfiniteForToWhile` rewrites the byte-saving minifier shape
/// `for (;;) body` into the canonical `while (true) body`.
///
/// Per ECMA-262 §14.7.4 (`ForStatement` semantics), when the
/// initializer, test, and update slots are all absent the loop reduces
/// to "repeat body forever, exit only via break/return/throw". That is
/// exactly the semantics of `while (true) body` (§14.7.3). The two
/// forms are **strictly spec-equivalent**: no identifier dispatch is
/// involved, no observable test of any value happens (the absent
/// `for` test is treated as "true" by the spec, never reading anything
/// from scope).
///
/// The pass restores the verbose form because:
///
/// * Minifiers consistently emit `for(;;)` (3 chars shorter than
///   `while(!0)` or `while(true)`) — un-minified source uses
///   `while (true)` or `for (;;)` mixed.
/// * The `structural_anchor` axis counts `for_count` and `while_count`
///   separately. Canonicalising to one form pulls minifier output and
///   un-minified source into the same bucket.
///
/// Non-infinite `for` loops (any non-empty init, test, or update) are
/// left alone — those forms have side effects and binding-scope
/// behaviour that the `while` form cannot express without a wrapping
/// block, and the rewrite is not always equivalent.
pub struct InfiniteForToWhile;

impl NormalizationPass for InfiniteForToWhile {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::InfiniteForToWhile
    }
    fn version(&self) -> u32 {
        1
    }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut visitor = Rewriter {
            builder: AstBuilder::new(alloc),
        };
        visitor.visit_program(program);
        rewrite_statements(&visitor.builder, &mut program.body);
    }
}

struct Rewriter<'a> {
    builder: AstBuilder<'a>,
}

impl<'a> VisitMut<'a> for Rewriter<'a> {
    fn visit_function_body(&mut self, body: &mut oxc_ast::ast::FunctionBody<'a>) {
        walk_function_body(self, body);
        rewrite_statements(&self.builder, &mut body.statements);
    }

    fn visit_block_statement(&mut self, block: &mut oxc_ast::ast::BlockStatement<'a>) {
        walk_block_statement(self, block);
        rewrite_statements(&self.builder, &mut block.body);
    }
}

fn rewrite_statements<'a>(
    builder: &AstBuilder<'a>,
    statements: &mut oxc_allocator::Vec<'a, Statement<'a>>,
) {
    for stmt in statements.iter_mut() {
        if let Statement::ForStatement(for_box) = stmt
            && for_box.init.is_none()
            && for_box.test.is_none()
            && for_box.update.is_none()
        {
            let placeholder = builder.statement_empty(SPAN);
            let Statement::ForStatement(for_box) = mem::replace(stmt, placeholder) else {
                unreachable!()
            };
            let for_node = for_box.unbox();
            let test = builder.expression_boolean_literal(SPAN, true);
            *stmt = builder.statement_while(SPAN, test, for_node.body);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn rewrites_empty_for_to_while_true() {
        let out = apply_to_source(
            &InfiniteForToWhile,
            "function f() { for (;;) { if (done()) break; } }",
        )
        .expect("parse");
        assert!(out.contains("while (true)"), "got: {out}");
        assert!(!out.contains("for (;"), "got: {out}");
    }

    #[test]
    fn rewrites_for_with_single_statement_body() {
        let out = apply_to_source(&InfiniteForToWhile, "function f() { for (;;) doWork(); }")
            .expect("parse");
        assert!(out.contains("while (true)"), "got: {out}");
    }

    #[test]
    fn leaves_for_with_init_alone() {
        let src = "function f() { for (let i = 0;; i++) { if (i > 9) break; } }";
        let out = apply_to_source(&InfiniteForToWhile, src).expect("parse");
        assert!(out.contains("for ("), "got: {out}");
        assert!(!out.contains("while (true)"), "got: {out}");
    }

    #[test]
    fn leaves_for_with_test_alone() {
        let src = "function f(n) { for (; n > 0; n--) {} }";
        let out = apply_to_source(&InfiniteForToWhile, src).expect("parse");
        assert!(out.contains("for ("), "got: {out}");
        assert!(!out.contains("while (true)"), "got: {out}");
    }

    #[test]
    fn leaves_for_with_update_alone() {
        let src = "function f(n) { for (;; n--) { if (!n) break; } }";
        let out = apply_to_source(&InfiniteForToWhile, src).expect("parse");
        assert!(out.contains("for ("), "got: {out}");
        assert!(!out.contains("while (true)"), "got: {out}");
    }

    #[test]
    fn rewrites_nested_inside_other_loops() {
        let out = apply_to_source(
            &InfiniteForToWhile,
            "function f(xs) { for (let x of xs) { for (;;) { if (done(x)) break; } } }",
        )
        .expect("parse");
        // The outer for-of must remain (it's not an infinite for).
        assert!(out.contains("for (let x of xs)"), "got: {out}");
        // The inner for(;;) must become while (true).
        assert!(out.contains("while (true)"), "got: {out}");
    }

    #[test]
    fn idempotent_on_already_while_true() {
        let src = "function f() { while (true) { if (done()) break; } }";
        let first = apply_to_source(&InfiniteForToWhile, src).expect("first");
        let second = apply_to_source(&InfiniteForToWhile, &first).expect("second");
        assert_eq!(first, second);
    }
}
