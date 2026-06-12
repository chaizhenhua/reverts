use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Expression, Program};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_expression;
use oxc_span::SPAN;
use oxc_syntax::operator::{BinaryOperator, UnaryOperator};
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `EqualityNegationFlattened` rewrites a logical-not applied to an
/// (in)equality comparison into the negated equality operator:
///
/// * `!(a === b)` → `a !== b`
/// * `!(a !== b)` → `a === b`
/// * `!(a == b)`  → `a != b`
/// * `!(a != b)`  → `a == b`
///
/// This rewrite is **strictly spec-equivalent**: per ECMA-262 §7.2.13
/// (strict equality) and §7.2.14 (abstract equality), both equality
/// operators are total functions that always return a boolean; the
/// `!==` / `!=` forms are defined as the inverse. Applying `!` to the
/// equality and using the negated operator therefore produce the
/// same boolean for every operand pair, including `NaN`, `null`,
/// and exotic objects with custom `Symbol.toPrimitive`. No identifier
/// dispatch is involved, so no shadowing concerns.
///
/// Comparison operators (`<`, `>`, `<=`, `>=`) are deliberately
/// **not** rewritten: those operators return `false` when either
/// operand is `NaN`, so `!(a < b)` is `true` for `(NaN, 1)` while
/// `a >= b` is `false` for the same pair. Those forms are
/// fundamentally non-equivalent and the pass leaves them alone.
pub struct EqualityNegationFlattened;

impl NormalizationPass for EqualityNegationFlattened {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::EqualityNegationFlattened
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
        let Some(new_op) = matches_not_of_equality(expr) else {
            return;
        };
        let placeholder = self.builder.expression_null_literal(SPAN);
        let owned = mem::replace(expr, placeholder);
        let Expression::UnaryExpression(unary_box) = owned else {
            unreachable!()
        };
        let unary = unary_box.unbox();
        let inner = strip_parens(unary.argument);
        let Expression::BinaryExpression(bin_box) = inner else {
            unreachable!("matches_not_of_equality already verified the shape")
        };
        let bin = bin_box.unbox();
        *expr = self
            .builder
            .expression_binary(SPAN, bin.left, new_op, bin.right);
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

/// If `expr` is `!(a {===,!==,==,!=} b)` (with or without parens),
/// return the negated operator to use for the rewrite.
fn matches_not_of_equality(expr: &Expression<'_>) -> Option<BinaryOperator> {
    let Expression::UnaryExpression(u) = expr else {
        return None;
    };
    if !matches!(u.operator, UnaryOperator::LogicalNot) {
        return None;
    }
    let Expression::BinaryExpression(bin) = strip_parens_ref(&u.argument) else {
        return None;
    };
    match bin.operator {
        BinaryOperator::StrictEquality => Some(BinaryOperator::StrictInequality),
        BinaryOperator::StrictInequality => Some(BinaryOperator::StrictEquality),
        BinaryOperator::Equality => Some(BinaryOperator::Inequality),
        BinaryOperator::Inequality => Some(BinaryOperator::Equality),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn rewrites_not_strict_eq() {
        let out = apply_to_source(
            &EqualityNegationFlattened,
            "function f(a, b) { return !(a === b); }",
        )
        .expect("parse");
        assert!(out.contains("a !== b"), "got: {out}");
        assert!(!out.contains("!("), "got: {out}");
    }

    #[test]
    fn rewrites_not_strict_ineq() {
        let out = apply_to_source(
            &EqualityNegationFlattened,
            "function f(a, b) { return !(a !== b); }",
        )
        .expect("parse");
        assert!(out.contains("a === b"), "got: {out}");
    }

    #[test]
    fn rewrites_not_loose_eq() {
        let out = apply_to_source(
            &EqualityNegationFlattened,
            "function f(a, b) { return !(a == b); }",
        )
        .expect("parse");
        assert!(out.contains("a != b"), "got: {out}");
    }

    #[test]
    fn rewrites_not_loose_ineq() {
        let out = apply_to_source(
            &EqualityNegationFlattened,
            "function f(a, b) { return !(a != b); }",
        )
        .expect("parse");
        assert!(out.contains("a == b"), "got: {out}");
    }

    #[test]
    fn leaves_not_less_than_alone() {
        // `!(a < b)` is NOT equivalent to `a >= b` because of NaN:
        // `!(NaN < 1) === true` while `NaN >= 1 === false`. The
        // pass refuses to touch this case.
        let out = apply_to_source(
            &EqualityNegationFlattened,
            "function f(a, b) { return !(a < b); }",
        )
        .expect("parse");
        assert!(out.contains("!"), "got: {out}");
        assert!(out.contains("<"), "got: {out}");
    }

    #[test]
    fn leaves_not_greater_than_alone() {
        let out = apply_to_source(
            &EqualityNegationFlattened,
            "function f(a, b) { return !(a > b); }",
        )
        .expect("parse");
        assert!(out.contains("!"), "got: {out}");
    }

    #[test]
    fn leaves_not_of_non_equality_alone() {
        let out = apply_to_source(&EqualityNegationFlattened, "function f(x) { return !x; }")
            .expect("parse");
        assert!(out.contains("!x"), "got: {out}");
    }

    #[test]
    fn rewrites_inside_object_property() {
        // Verify recursion: nested in object literal.
        let out = apply_to_source(
            &EqualityNegationFlattened,
            "function f(a, b) { return {neq: !(a === b)}; }",
        )
        .expect("parse");
        assert!(out.contains("neq: a !== b"), "got: {out}");
    }

    #[test]
    fn idempotent_when_no_match() {
        let src = "function f(a, b) { return a !== b; }";
        let first = apply_to_source(&EqualityNegationFlattened, src).expect("first");
        let second = apply_to_source(&EqualityNegationFlattened, &first).expect("second");
        assert_eq!(first, second);
    }
}
