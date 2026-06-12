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

/// `ConditionalBooleanCoerced` rewrites a conditional expression whose
/// consequent and alternate are the boolean literals `true` / `false`
/// into the matching `!`/`!!` form:
///
/// * `c ? true : false`  → `!!c`
/// * `c ? false : true`  → `!c`
///
/// Per ECMA-262 §13.14.1 (`ConditionalExpression` evaluation), the
/// test is evaluated and then `ToBoolean` is applied to decide which
/// branch returns. `!c` evaluates `ToBoolean(c)` and negates it
/// (§13.5.7); `!!c` evaluates `ToBoolean(c)` and double-negates it,
/// recovering the same boolean. Both rewrites produce **exactly** the
/// same boolean result as the original conditional for every operand
/// `c`, including objects with custom `Symbol.toPrimitive`,
/// `Symbol`/`BigInt`, `NaN`, and exotic proxies. The transformation
/// is **strictly spec-equivalent** — no identifier dispatch, no
/// shadowing concerns.
///
/// The pass deliberately does NOT touch:
///
/// * `c ? 1 : 0` or other numeric/string consequent-alternate pairs —
///   those don't reduce to a boolean coercion (the result has a
///   different runtime type).
/// * `c ? x : y` where either arm is a non-literal — the arms may
///   have side effects or non-boolean values, and the rewrite would
///   change observable behaviour.
pub struct ConditionalBooleanCoerced;

impl NormalizationPass for ConditionalBooleanCoerced {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::ConditionalBooleanCoerced
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

#[derive(Copy, Clone)]
enum Shape {
    TrueFalse,
    FalseTrue,
}

impl<'a> VisitMut<'a> for Rewriter<'a> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        walk_expression(self, expr);
        let Some(shape) = matches_boolean_conditional(expr) else {
            return;
        };
        let placeholder = self.builder.expression_null_literal(SPAN);
        let owned = mem::replace(expr, placeholder);
        let Expression::ConditionalExpression(cond_box) = owned else {
            unreachable!()
        };
        let cond = cond_box.unbox();
        let test = strip_parens(cond.test);
        *expr = match shape {
            // `c ? true : false` → `!!c`
            Shape::TrueFalse => {
                let inner = self
                    .builder
                    .expression_unary(SPAN, UnaryOperator::LogicalNot, test);
                self.builder
                    .expression_unary(SPAN, UnaryOperator::LogicalNot, inner)
            }
            // `c ? false : true` → `!c`
            Shape::FalseTrue => {
                self.builder
                    .expression_unary(SPAN, UnaryOperator::LogicalNot, test)
            }
        };
    }
}

fn strip_parens<'a>(expr: Expression<'a>) -> Expression<'a> {
    match expr {
        Expression::ParenthesizedExpression(p) => strip_parens(p.unbox().expression),
        other => other,
    }
}

fn strip_parens_ref<'a, 'b>(expr: &'b Expression<'a>) -> &'b Expression<'a> {
    match expr {
        Expression::ParenthesizedExpression(p) => strip_parens_ref(&p.expression),
        other => other,
    }
}

fn matches_boolean_conditional(expr: &Expression<'_>) -> Option<Shape> {
    let Expression::ConditionalExpression(cond) = expr else {
        return None;
    };
    let consequent = strip_parens_ref(&cond.consequent);
    let alternate = strip_parens_ref(&cond.alternate);
    match (consequent, alternate) {
        (Expression::BooleanLiteral(c), Expression::BooleanLiteral(a)) => {
            match (c.value, a.value) {
                (true, false) => Some(Shape::TrueFalse),
                (false, true) => Some(Shape::FalseTrue),
                _ => None,
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn rewrites_true_false_to_double_not() {
        let out = apply_to_source(
            &ConditionalBooleanCoerced,
            "function f(c) { return c ? true : false; }",
        )
        .expect("parse");
        assert!(out.contains("!!c"), "got: {out}");
        assert!(!out.contains("? true"), "got: {out}");
    }

    #[test]
    fn rewrites_false_true_to_single_not() {
        let out = apply_to_source(
            &ConditionalBooleanCoerced,
            "function f(c) { return c ? false : true; }",
        )
        .expect("parse");
        assert!(out.contains("!c"), "got: {out}");
        assert!(!out.contains("? false"), "got: {out}");
    }

    #[test]
    fn rewrites_with_complex_test_expression() {
        let out = apply_to_source(
            &ConditionalBooleanCoerced,
            "function f(a, b) { return (a > b) ? true : false; }",
        )
        .expect("parse");
        // After the rewrite the test must be inside `!!(...)` — codegen
        // re-adds parens around `>` because `>` binds looser than `!`.
        assert!(out.contains("!!"), "got: {out}");
        assert!(!out.contains("? true"), "got: {out}");
    }

    #[test]
    fn leaves_non_boolean_consequent_alone() {
        let src = "function f(c) { return c ? 1 : 0; }";
        let out = apply_to_source(&ConditionalBooleanCoerced, src).expect("parse");
        assert!(out.contains("?"), "got: {out}");
        assert!(out.contains("1"), "got: {out}");
        assert!(out.contains("0"), "got: {out}");
    }

    #[test]
    fn leaves_non_literal_arms_alone() {
        let src = "function f(c, x, y) { return c ? x : y; }";
        let out = apply_to_source(&ConditionalBooleanCoerced, src).expect("parse");
        assert!(
            out.contains("c ? x : y") || out.contains("c?x:y"),
            "got: {out}"
        );
    }

    #[test]
    fn leaves_true_true_alone() {
        // Both arms identical; not a boolean coercion shape.
        let src = "function f(c) { return c ? true : true; }";
        let out = apply_to_source(&ConditionalBooleanCoerced, src).expect("parse");
        assert!(out.contains("?"), "got: {out}");
    }

    #[test]
    fn rewrites_inside_object_property() {
        let out = apply_to_source(
            &ConditionalBooleanCoerced,
            "function f(c) { return {b: c ? true : false}; }",
        )
        .expect("parse");
        assert!(out.contains("!!c"), "got: {out}");
    }

    #[test]
    fn idempotent_on_already_canonical() {
        let src = "function f(c) { return !!c; }";
        let first = apply_to_source(&ConditionalBooleanCoerced, src).expect("first");
        let second = apply_to_source(&ConditionalBooleanCoerced, &first).expect("second");
        assert_eq!(first, second);
    }
}
