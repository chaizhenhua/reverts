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

/// `TypeofUndefinedCanonical` rewrites the safer-but-verbose `typeof
/// x === "undefined"` (and `!==`, both quote styles) form into the
/// shorter `x === undefined` form. Both are equivalent **when `x` is
/// a declared identifier in the surrounding scope** — which is always
/// the case for function bodies that we fingerprint.
///
/// Minifiers convert `x === undefined` → `x === void 0`, which
/// `BooleanUndefinedCanonicalised` already restores to `x ===
/// undefined`. Source code in libraries commonly spells the same
/// check as `typeof x === "undefined"`. Normalising both to the
/// `x === undefined` canonical form aligns the AST hash.
pub struct TypeofUndefinedCanonical;

impl NormalizationPass for TypeofUndefinedCanonical {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::TypeofUndefinedCanonical
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
        let Some(op) = matches_typeof_undefined(expr) else {
            return;
        };
        // Take ownership.
        let placeholder = self.builder.expression_null_literal(SPAN);
        let owned = mem::replace(expr, placeholder);
        let Expression::BinaryExpression(bin_box) = owned else {
            unreachable!()
        };
        let bin = bin_box.unbox();
        // The unary `typeof x` operand is on the typeof side; figure
        // out which side and extract.
        let operand = if let Expression::UnaryExpression(u) = &bin.left {
            if matches!(u.operator, UnaryOperator::Typeof) {
                let Expression::UnaryExpression(u_box) = bin.left else {
                    unreachable!()
                };
                u_box.unbox().argument
            } else {
                unreachable!("matches_typeof_undefined verified the shape")
            }
        } else {
            let Expression::UnaryExpression(u_box) = bin.right else {
                unreachable!()
            };
            u_box.unbox().argument
        };
        let undefined_ident = self
            .builder
            .expression_identifier_reference(SPAN, "undefined");
        *expr = self
            .builder
            .expression_binary(SPAN, operand, op, undefined_ident);
    }
}

/// If `expr` matches one of the four `typeof <X> {===,!==} "undefined"`
/// shapes (operands in either order), return the corresponding
/// strict-equality operator to use for the rewrite.
fn matches_typeof_undefined(expr: &Expression<'_>) -> Option<BinaryOperator> {
    let Expression::BinaryExpression(bin) = expr else {
        return None;
    };
    let op = match bin.operator {
        BinaryOperator::StrictEquality => BinaryOperator::StrictEquality,
        BinaryOperator::StrictInequality => BinaryOperator::StrictInequality,
        _ => return None,
    };
    let is_typeof_unary = |e: &Expression<'_>| -> bool {
        matches!(e, Expression::UnaryExpression(u) if matches!(u.operator, UnaryOperator::Typeof))
    };
    let is_undefined_string = |e: &Expression<'_>| -> bool {
        matches!(e, Expression::StringLiteral(s) if s.value == "undefined")
    };
    let typeof_left = is_typeof_unary(&bin.left) && is_undefined_string(&bin.right);
    let typeof_right = is_typeof_unary(&bin.right) && is_undefined_string(&bin.left);
    if typeof_left || typeof_right {
        Some(op)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn rewrites_typeof_strict_eq_undefined() {
        let out = apply_to_source(
            &TypeofUndefinedCanonical,
            r#"function f(x) { return typeof x === "undefined"; }"#,
        )
        .expect("parse");
        assert!(out.contains("x === undefined"), "got: {out}");
        assert!(!out.contains("typeof"), "got: {out}");
    }

    #[test]
    fn rewrites_typeof_strict_ineq_undefined() {
        let out = apply_to_source(
            &TypeofUndefinedCanonical,
            r#"function f(x) { return typeof x !== "undefined"; }"#,
        )
        .expect("parse");
        assert!(out.contains("x !== undefined"), "got: {out}");
    }

    #[test]
    fn rewrites_reverse_order_undefined_string_left() {
        // `"undefined" === typeof x` — same meaning, operands swapped.
        let out = apply_to_source(
            &TypeofUndefinedCanonical,
            r#"function f(x) { return "undefined" === typeof x; }"#,
        )
        .expect("parse");
        assert!(out.contains("x === undefined"), "got: {out}");
    }

    #[test]
    fn leaves_typeof_against_other_string_alone() {
        // `typeof x === "function"` is a different check.
        let src = r#"function f(x) { return typeof x === "function"; }"#;
        let out = apply_to_source(&TypeofUndefinedCanonical, src).expect("parse");
        assert!(out.contains("typeof x"), "got: {out}");
        assert!(out.contains("\"function\""), "got: {out}");
    }

    #[test]
    fn leaves_loose_equality_alone() {
        // `typeof x == "undefined"` (==, not ===) has different
        // coercion semantics, leave it.
        let src = r#"function f(x) { return typeof x == "undefined"; }"#;
        let out = apply_to_source(&TypeofUndefinedCanonical, src).expect("parse");
        assert!(out.contains("typeof"), "got: {out}");
    }

    #[test]
    fn idempotent_when_no_match() {
        let src = "function f(x) { return x === undefined; }";
        let first = apply_to_source(&TypeofUndefinedCanonical, src).expect("first");
        let second = apply_to_source(&TypeofUndefinedCanonical, &first).expect("second");
        assert_eq!(first, second);
    }
}
