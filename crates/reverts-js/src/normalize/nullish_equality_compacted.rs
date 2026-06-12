use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Expression, Program};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_expression;
use oxc_span::SPAN;
use oxc_syntax::operator::{BinaryOperator, LogicalOperator};
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `NullishEqualityCompacted` rewrites the explicit null/undefined
/// check into the abstract `== null` form (and the negated form into
/// `!= null`):
///
/// * `a === null || a === undefined`  → `a == null`
/// * `a === undefined || a === null`  → `a == null` (commutative)
/// * `a !== null && a !== undefined`  → `a != null`
/// * `a !== undefined && a !== null`  → `a != null` (commutative)
///
/// Per ECMA-262 §7.2.14 (`IsLooselyEqual`), `null == undefined`,
/// `null == null`, and `undefined == undefined` all evaluate to
/// `true`, while every other operand combination evaluates to `false`.
/// Therefore `a == null` is exactly the predicate "`a` is null or
/// undefined", which is the same predicate as the explicit chain.
///
/// The chain reads `a` twice while the compact form reads it once, so
/// the rewrite is restricted to **plain identifier `a`**: identifier
/// reads are pure (no getters), so the read count doesn't matter.
/// Member-expression or call-expression `a` (`obj.x`, `f()`) is left
/// alone — the long form may invoke a getter twice or call a
/// function twice, and the compact form would change that.
///
/// The rewrite is **guarded** on `undefined` not being shadowed
/// anywhere in the program (via [`super::shadow_check`]). When
/// `undefined` resolves to anything other than the actual undefined
/// value, the long-form chain checks something else entirely (e.g.
/// `a === 5`), and the rewrite would silently change semantics.
pub struct NullishEqualityCompacted;

impl NormalizationPass for NullishEqualityCompacted {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::NullishEqualityCompacted
    }
    fn version(&self) -> u32 {
        1
    }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        if super::shadow_check::program_can_shadow(program, "undefined") {
            return;
        }
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
    /// `a === null || a === undefined`  → `a == null`
    EqualityOr,
    /// `a !== null && a !== undefined`  → `a != null`
    InequalityAnd,
}

impl<'a> VisitMut<'a> for Rewriter<'a> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        walk_expression(self, expr);
        let Some((shape, ident_name)) = matches_nullish_pair(expr) else {
            return;
        };
        let ident = self
            .builder
            .expression_identifier_reference(SPAN, ident_name.as_str());
        let null = self.builder.expression_null_literal(SPAN);
        let new_op = match shape {
            Shape::EqualityOr => BinaryOperator::Equality,
            Shape::InequalityAnd => BinaryOperator::Inequality,
        };
        let placeholder = self.builder.expression_null_literal(SPAN);
        let _ = mem::replace(expr, placeholder);
        *expr = self.builder.expression_binary(SPAN, ident, new_op, null);
    }
}

/// Match the `(a === null || a === undefined)` family. Returns the
/// canonical shape and the identifier name when found.
fn matches_nullish_pair(expr: &Expression<'_>) -> Option<(Shape, String)> {
    let Expression::LogicalExpression(log) = expr else {
        return None;
    };

    // Determine which shape we're matching based on logical operator.
    let (logical_op, binary_op_for_arm) = match log.operator {
        LogicalOperator::Or => (Shape::EqualityOr, BinaryOperator::StrictEquality),
        LogicalOperator::And => (Shape::InequalityAnd, BinaryOperator::StrictInequality),
        LogicalOperator::Coalesce => return None,
    };

    let left = decompose_arm(&log.left, binary_op_for_arm)?;
    let right = decompose_arm(&log.right, binary_op_for_arm)?;
    if left.name != right.name {
        return None;
    }
    // One arm must check null, the other must check undefined.
    if matches!((left.rhs, right.rhs), (Rhs::Null, Rhs::Undefined))
        || matches!((left.rhs, right.rhs), (Rhs::Undefined, Rhs::Null))
    {
        Some((logical_op, left.name))
    } else {
        None
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Rhs {
    Null,
    Undefined,
}

struct Arm {
    name: String,
    rhs: Rhs,
}

fn decompose_arm(expr: &Expression<'_>, expected_op: BinaryOperator) -> Option<Arm> {
    let Expression::BinaryExpression(bin) = expr else {
        return None;
    };
    if bin.operator != expected_op {
        return None;
    }
    // Classify each side: is it `null`, the `undefined` identifier, or a
    // plain identifier we'd use as the variable name? `undefined` is
    // an Identifier too, so we have to disambiguate by name before
    // treating either side as "the variable".
    let (var_name, rhs) = match (classify_side(&bin.left), classify_side(&bin.right)) {
        (Some(Side::Variable(n)), Some(Side::NullOrUndefined(r))) => (n, r),
        (Some(Side::NullOrUndefined(r)), Some(Side::Variable(n))) => (n, r),
        _ => return None,
    };
    Some(Arm {
        name: var_name,
        rhs,
    })
}

enum Side {
    Variable(String),
    NullOrUndefined(Rhs),
}

fn classify_side(e: &Expression<'_>) -> Option<Side> {
    match e {
        Expression::NullLiteral(_) => Some(Side::NullOrUndefined(Rhs::Null)),
        Expression::Identifier(i) => {
            if i.name.as_str() == "undefined" {
                Some(Side::NullOrUndefined(Rhs::Undefined))
            } else {
                Some(Side::Variable(i.name.as_str().to_owned()))
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
    fn compacts_or_form_with_undefined_second() {
        let out = apply_to_source(
            &NullishEqualityCompacted,
            "function f(a) { return a === null || a === undefined; }",
        )
        .expect("parse");
        assert!(out.contains("a == null"), "got: {out}");
        assert!(!out.contains("|| a"), "got: {out}");
    }

    #[test]
    fn compacts_or_form_with_undefined_first() {
        let out = apply_to_source(
            &NullishEqualityCompacted,
            "function f(a) { return a === undefined || a === null; }",
        )
        .expect("parse");
        assert!(out.contains("a == null"), "got: {out}");
    }

    #[test]
    fn compacts_and_form_into_loose_inequality() {
        let out = apply_to_source(
            &NullishEqualityCompacted,
            "function f(a) { return a !== null && a !== undefined; }",
        )
        .expect("parse");
        assert!(out.contains("a != null"), "got: {out}");
    }

    #[test]
    fn compacts_with_swapped_operand_order() {
        // `null === a` should match same as `a === null`.
        let out = apply_to_source(
            &NullishEqualityCompacted,
            "function f(a) { return null === a || undefined === a; }",
        )
        .expect("parse");
        assert!(out.contains("a == null"), "got: {out}");
    }

    #[test]
    fn bails_out_when_undefined_shadowed() {
        let src = r#"
            function f(a) {
                let undefined = 0;
                return a === null || a === undefined;
            }
        "#;
        let out = apply_to_source(&NullishEqualityCompacted, src).expect("parse");
        assert!(out.contains("a === null || a === undefined"), "got: {out}");
    }

    #[test]
    fn leaves_different_identifiers_alone() {
        // `a === null || b === undefined` is NOT the same as `a == null` — different operands.
        let src = "function f(a, b) { return a === null || b === undefined; }";
        let out = apply_to_source(&NullishEqualityCompacted, src).expect("parse");
        assert!(out.contains("a === null || b === undefined"), "got: {out}");
    }

    #[test]
    fn leaves_member_expression_target_alone() {
        // `obj.x` could be a getter; reading it twice may have side effects.
        let src = "function f(obj) { return obj.x === null || obj.x === undefined; }";
        let out = apply_to_source(&NullishEqualityCompacted, src).expect("parse");
        assert!(out.contains("obj.x === null"), "got: {out}");
    }

    #[test]
    fn leaves_call_expression_target_alone() {
        let src = "function f(g) { return g() === null || g() === undefined; }";
        let out = apply_to_source(&NullishEqualityCompacted, src).expect("parse");
        assert!(out.contains("g() === null"), "got: {out}");
    }

    #[test]
    fn leaves_chain_with_extra_condition_alone() {
        // `a === null || a === undefined || a === 0` — only the first two arms match;
        // the outer `|| === 0` puts this into a different shape.
        let src = "function f(a) { return a === null || a === undefined || a === 0; }";
        let out = apply_to_source(&NullishEqualityCompacted, src).expect("parse");
        // Inner pair becomes `a == null`, then outer `|| a === 0` stays.
        // Outer parse: ((a===null || a===undef) || a===0). Inner is rewritten.
        assert!(out.contains("a == null"), "got: {out}");
        assert!(out.contains("a === 0"), "got: {out}");
    }

    #[test]
    fn idempotent_when_already_compacted() {
        let src = "function f(a) { return a == null; }";
        let first = apply_to_source(&NullishEqualityCompacted, src).expect("first");
        let second = apply_to_source(&NullishEqualityCompacted, &first).expect("second");
        assert_eq!(first, second);
    }
}
