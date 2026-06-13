use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Program, Statement};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::{walk_block_statement, walk_function_body};
use oxc_span::SPAN;
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `ForToWhileFiniteCanonical` rewrites a finite-test `for` whose
/// initializer and update slots are both absent into the matching
/// `while` form:
///
/// ```text
/// for (; cond; ) body   →   while (cond) body
/// ```
///
/// Per ECMA-262 §14.7.4 (`ForStatement` semantics), when both init
/// and update are absent the loop reduces to "evaluate `cond`, run
/// `body` if truthy, repeat". That is exactly the semantics of
/// `while (cond) body` (§14.7.3). The two forms produce **identical**
/// execution traces:
///
/// * `continue` jumps back to the test in both cases (no update to
///   skip past in the `for` form, since the update slot is empty).
/// * `break` exits both forms identically.
/// * Test evaluation order, body execution, and exit conditions all
///   match per spec.
///
/// The rewrite is **strictly spec-equivalent** — no identifier
/// dispatch, no shadowing concerns. The `structural_anchor`
/// fingerprint axis counts `for_count` and `while_count` separately,
/// so canonicalising shifts those counts and produces alternate
/// fingerprints that the alt-source cascade tiers can exercise.
///
/// Non-empty `for` (any init, any update — even a one-element comma
/// expression) is left alone: the init binds, the update has its
/// own evaluation slot at the end of each iteration that no `while`
/// form can express without restructuring.
pub struct ForToWhileFiniteCanonical;

impl NormalizationPass for ForToWhileFiniteCanonical {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::ForToWhileFiniteCanonical
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
            && for_box.update.is_none()
            && for_box.test.is_some()
        {
            let placeholder = builder.statement_empty(SPAN);
            let Statement::ForStatement(for_box) = mem::replace(stmt, placeholder) else {
                unreachable!()
            };
            let for_node = for_box.unbox();
            let test = for_node.test.expect("matched Some by guard");
            *stmt = builder.statement_while(SPAN, test, for_node.body);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn rewrites_finite_for_to_while() {
        let out = apply_to_source(
            &ForToWhileFiniteCanonical,
            "function f(n) { for (; n > 0; ) { handle(n); n--; } }",
        )
        .expect("parse");
        assert!(out.contains("while (n > 0)"), "got: {out}");
        assert!(!out.contains("for ("), "got: {out}");
    }

    #[test]
    fn rewrites_for_with_single_statement_body() {
        let out = apply_to_source(
            &ForToWhileFiniteCanonical,
            "function f(p) { for (; !done(); ) advance(p); }",
        )
        .expect("parse");
        assert!(out.contains("while (!done())"), "got: {out}");
    }

    #[test]
    fn leaves_for_with_init_alone() {
        let src = "function f() { for (let i = 0; i < 10; ) { handle(i); i++; } }";
        let out = apply_to_source(&ForToWhileFiniteCanonical, src).expect("parse");
        assert!(out.contains("for ("), "got: {out}");
        assert!(!out.contains("while ("), "got: {out}");
    }

    #[test]
    fn leaves_for_with_update_alone() {
        let src = "function f(n) { for (; n > 0; n--) handle(n); }";
        let out = apply_to_source(&ForToWhileFiniteCanonical, src).expect("parse");
        assert!(out.contains("for ("), "got: {out}");
    }

    #[test]
    fn leaves_infinite_for_alone() {
        // Empty test slot → handled by infinite_for_to_while; this
        // pass deliberately requires `test: Some(_)`.
        let src = "function f() { for (;;) { if (done()) break; } }";
        let out = apply_to_source(&ForToWhileFiniteCanonical, src).expect("parse");
        assert!(out.contains("for ("), "got: {out}");
    }

    #[test]
    fn rewrites_nested_inside_other_constructs() {
        let out = apply_to_source(
            &ForToWhileFiniteCanonical,
            r#"
            function f(xs) {
                for (let x of xs) {
                    for (; isActive(x); ) handle(x);
                }
            }
            "#,
        )
        .expect("parse");
        assert!(out.contains("for (let x of xs)"), "got: {out}");
        assert!(out.contains("while (isActive(x))"), "got: {out}");
    }

    #[test]
    fn idempotent_on_already_while() {
        let src = "function f(n) { while (n > 0) { handle(n); n--; } }";
        let first = apply_to_source(&ForToWhileFiniteCanonical, src).expect("first");
        let second = apply_to_source(&ForToWhileFiniteCanonical, &first).expect("second");
        assert_eq!(first, second);
    }
}
