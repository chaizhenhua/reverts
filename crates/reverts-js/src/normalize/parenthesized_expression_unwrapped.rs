use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Expression, Program};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_expression;
use oxc_span::SPAN;
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `ParenthesizedExpressionUnwrapped` strips every
/// `ParenthesizedExpression` AST node from the program, replacing
/// `(expr)` with `expr`.
///
/// Per ECMA-262 §13.2.4 (`ParenthesizedExpression`), the parentheses
/// are pure syntactic grouping: the production evaluates to exactly
/// the wrapped expression's value with no side effects of its own.
/// The OXC parser nevertheless preserves them as a separate AST node
/// so codegen can faithfully round-trip the source, which means
/// `(x + 1)` and `x + 1` hash differently at the `ast` axis (the
/// former falls into the `otherexpr` catch-all bucket, the latter
/// into the structured `bin(...)` form) even though they're
/// semantically identical.
///
/// Stripping the wrappers is **strictly spec-equivalent** — no
/// identifier dispatch, no shadowing concerns. Multiple existing
/// passes (`logical_short_circuit_expanded`,
/// `equality_negation_flattened`, `conditional_statement_expanded`,
/// etc.) carry local `strip_parens` helpers; running this pass once
/// removes them all up front so downstream passes see the bare
/// expression directly.
///
/// Codegen will re-add parens as needed for precedence, so the
/// printed output remains well-formed.
pub struct ParenthesizedExpressionUnwrapped;

impl NormalizationPass for ParenthesizedExpressionUnwrapped {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::ParenthesizedExpressionUnwrapped
    }
    fn version(&self) -> u32 {
        1
    }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut visitor = Unwrapper {
            builder: AstBuilder::new(alloc),
        };
        visitor.visit_program(program);
    }
}

struct Unwrapper<'a> {
    builder: AstBuilder<'a>,
}

impl<'a> VisitMut<'a> for Unwrapper<'a> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        // Strip an outer paren wrapper, then keep stripping if the
        // unwrapped expression is itself another paren — `((x))` →
        // `x` in a single visit.
        while matches!(expr, Expression::ParenthesizedExpression(_)) {
            let placeholder = self.builder.expression_null_literal(SPAN);
            let owned = mem::replace(expr, placeholder);
            let Expression::ParenthesizedExpression(p) = owned else {
                unreachable!()
            };
            *expr = p.unbox().expression;
        }
        walk_expression(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn strips_outer_parens_around_binary() {
        let out = apply_to_source(
            &ParenthesizedExpressionUnwrapped,
            "function f(a, b) { return (a + b); }",
        )
        .expect("parse");
        // Codegen may re-add parens around the binary if needed for
        // a surrounding precedence context, but a top-level return
        // expression doesn't need them.
        assert!(out.contains("return a + b"), "got: {out}");
    }

    #[test]
    fn strips_nested_parens() {
        let out = apply_to_source(
            &ParenthesizedExpressionUnwrapped,
            "function f(x) { return ((x)); }",
        )
        .expect("parse");
        assert!(out.contains("return x"), "got: {out}");
        assert!(!out.contains("((x))"), "got: {out}");
    }

    #[test]
    fn strips_parens_around_test_and_keeps_semantics() {
        let out = apply_to_source(
            &ParenthesizedExpressionUnwrapped,
            "function f(c, a, b) { return (c) ? (a) : (b); }",
        )
        .expect("parse");
        assert!(out.contains("c ? a : b"), "got: {out}");
    }

    #[test]
    fn preserves_grouping_for_precedence_via_codegen() {
        // `!(a + b)` parses as UnaryExpression(!, ParenthesizedExpression(BinaryExpression(a, +, b))).
        // After unwrap, the inner is BinaryExpression. Codegen needs to
        // re-add parens around the binary so `!` doesn't bind to `a`
        // only — verify the printed form keeps the binary intact.
        let out = apply_to_source(
            &ParenthesizedExpressionUnwrapped,
            "function f(a, b) { return !(a + b); }",
        )
        .expect("parse");
        assert!(out.contains("!(a + b)"), "got: {out}");
    }

    #[test]
    fn strips_parens_in_arrow_body() {
        let out = apply_to_source(
            &ParenthesizedExpressionUnwrapped,
            "const f = (x) => (x + 1);",
        )
        .expect("parse");
        // Arrow params keep their parens (those aren't `ParenthesizedExpression`
        // — they're FormalParameters syntax). The body wrapper paren is
        // stripped.
        assert!(out.contains("x + 1"), "got: {out}");
    }

    #[test]
    fn idempotent_when_no_parens() {
        let src = "function f(x) { return x + 1; }";
        let first = apply_to_source(&ParenthesizedExpressionUnwrapped, src).expect("first");
        let second = apply_to_source(&ParenthesizedExpressionUnwrapped, &first).expect("second");
        assert_eq!(first, second);
    }
}
