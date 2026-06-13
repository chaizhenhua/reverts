use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Expression, Program};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_expression;
use oxc_span::SPAN;
use oxc_syntax::operator::UnaryOperator;
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `ConditionalNegationFlipped` rewrites a conditional expression
/// whose test is a single negation by stripping the `!` and swapping
/// the consequent/alternate arms:
///
/// ```text
/// !c ? a : b   →   c ? b : a
/// ```
///
/// Per ECMA-262 §13.14.1 (`ConditionalExpression` evaluation), the
/// test is first evaluated and then `ToBoolean` is applied to decide
/// which branch returns. Per §13.5.7 (`UnaryExpression: !`), `!c` is
/// `!ToBoolean(c)`. So:
///
/// * `!c ? a : b` returns `a` when `ToBoolean(c)` is false and `b`
///   when it's true.
/// * `c ? b : a` returns `b` when `ToBoolean(c)` is true and `a`
///   when it's false.
///
/// The two produce **exactly** the same value for every operand,
/// including objects with custom `Symbol.toPrimitive`, NaN, Symbol,
/// document.all, and exotic proxies. The rewrite is **strictly
/// spec-equivalent** — no identifier dispatch, no shadowing.
///
/// The pass restricts to a literal `!<expr>` test. `!!<expr>` is a
/// boolean-coercion idiom; leaving it for the `conditional_boolean_coerced`
/// pass keeps the simpler `c ? true : false → !!c` shape easy to
/// recognise. Tests that are not single-`!` unary expressions are
/// left alone.
pub struct ConditionalNegationFlipped;

impl NormalizationPass for ConditionalNegationFlipped {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::ConditionalNegationFlipped
    }
    fn version(&self) -> u32 {
        1
    }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut visitor = Rewriter {
            builder: AstBuilder::new(alloc),
        };
        visitor.visit_program(program);
    }
}

struct Rewriter<'a> {
    builder: AstBuilder<'a>,
}

impl<'a> VisitMut<'a> for Rewriter<'a> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        walk_expression(self, expr);
        if !matches_negated_conditional(expr) {
            return;
        }
        let placeholder = self.builder.expression_null_literal(SPAN);
        let owned = mem::replace(expr, placeholder);
        let Expression::ConditionalExpression(cond_box) = owned else {
            unreachable!()
        };
        let cond = cond_box.unbox();
        let Expression::UnaryExpression(neg_box) = cond.test else {
            unreachable!()
        };
        let neg = neg_box.unbox();
        // `!!c ? a : b` would land here too if we matched any `!`
        // outer — but `matches_negated_conditional` also rejects a
        // `!!` (double-not) shape since that's a ToBoolean coercion
        // handled by `conditional_boolean_coerced`. So `neg.argument`
        // is a non-`!` expression.
        *expr = self.builder.expression_conditional(
            SPAN,
            neg.argument,
            cond.alternate,
            cond.consequent,
        );
    }
}

fn matches_negated_conditional(expr: &Expression<'_>) -> bool {
    let Expression::ConditionalExpression(cond) = expr else {
        return false;
    };
    let Expression::UnaryExpression(u) = &cond.test else {
        return false;
    };
    if !matches!(u.operator, UnaryOperator::LogicalNot) {
        return false;
    }
    // Reject `!!<expr>` (test is double-not) — that's the ToBoolean
    // idiom handled by `conditional_boolean_coerced`.
    if let Expression::UnaryExpression(inner) = &u.argument
        && matches!(inner.operator, UnaryOperator::LogicalNot)
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn flips_simple_negated_conditional() {
        let out = apply_to_source(
            &ConditionalNegationFlipped,
            "function f(c, a, b) { return !c ? a : b; }",
        )
        .expect("parse");
        assert!(out.contains("c ? b : a"), "got: {out}");
        assert!(!out.contains("!c ?"), "got: {out}");
    }

    #[test]
    fn flips_with_complex_test_inside_not() {
        let out = apply_to_source(
            &ConditionalNegationFlipped,
            "function f(a, b) { return !(a > b) ? handleLow() : handleHigh(); }",
        )
        .expect("parse");
        // After flipping: `(a > b) ? handleHigh() : handleLow()`.
        assert!(out.contains("a > b"), "got: {out}");
        assert!(out.contains("? handleHigh() : handleLow()"), "got: {out}");
    }

    #[test]
    fn leaves_double_not_test_alone() {
        // `!!c ? true : false` is the boolean-coercion shape;
        // `conditional_boolean_coerced` handles it. This pass
        // must NOT touch it (otherwise we'd thrash).
        let src = "function f(c) { return !!c ? true : false; }";
        let out = apply_to_source(&ConditionalNegationFlipped, src).expect("parse");
        assert!(out.contains("!!c ?"), "got: {out}");
    }

    #[test]
    fn leaves_unnegated_conditional_alone() {
        let src = "function f(c, a, b) { return c ? a : b; }";
        let out = apply_to_source(&ConditionalNegationFlipped, src).expect("parse");
        assert!(out.contains("c ? a : b"), "got: {out}");
    }

    #[test]
    fn leaves_typeof_test_alone() {
        // `typeof x` is a UnaryExpression but not `!`. Don't match.
        let src = r#"function f(x, a, b) { return typeof x === "undefined" ? a : b; }"#;
        let out = apply_to_source(&ConditionalNegationFlipped, src).expect("parse");
        assert!(out.contains("typeof x"), "got: {out}");
    }

    #[test]
    fn flips_inside_nested_position() {
        let out = apply_to_source(
            &ConditionalNegationFlipped,
            "function f(c, a, b) { return [!c ? a : b]; }",
        )
        .expect("parse");
        assert!(out.contains("c ? b : a"), "got: {out}");
    }

    #[test]
    fn flips_in_return_chain() {
        let out = apply_to_source(
            &ConditionalNegationFlipped,
            "function f(c, a, b) { let x = !c ? a : b; return x; }",
        )
        .expect("parse");
        assert!(out.contains("c ? b : a"), "got: {out}");
    }

    #[test]
    fn idempotent_when_no_match() {
        let src = "function f(c, a, b) { return c ? a : b; }";
        let first = apply_to_source(&ConditionalNegationFlipped, src).expect("first");
        let second = apply_to_source(&ConditionalNegationFlipped, &first).expect("second");
        assert_eq!(first, second);
    }
}
