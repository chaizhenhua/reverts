use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{AssignmentTarget, Expression, Program, SimpleAssignmentTarget};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_expression;
use oxc_span::SPAN;
use oxc_syntax::operator::{AssignmentOperator, BinaryOperator};
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `CompoundAssignmentCanonical` rewrites `x = x op y` into the
/// compound form `x op= y` for every binary operator that has a
/// compound counterpart.
///
/// The two forms are semantically identical **when the LHS is an
/// `Identifier`**:
///
/// * `x = x + 1` ↔ `x += 1`
/// * `x = x - n` ↔ `x -= n`
/// * `x = x * f()` ↔ `x *= f()`
///
/// Reading `x` once vs twice doesn't matter because plain identifier
/// reads are pure (no getter side effects). Member-expression LHS
/// (`obj.x = obj.x + 1`, `arr[i] = arr[i] + 1`) is **not** rewritten
/// because the long form may evaluate the receiver — or the
/// computed index — twice while the compound form evaluates it once.
/// When the receiver is a function call (`f().x = f().x + 1`), the
/// long form invokes `f()` twice; rewriting would silently drop one
/// call.
///
/// Restricting to `Identifier` LHS keeps the pass observably
/// equivalent in all positions; the cascade fingerprint converges
/// because both forms produce the same AST shape after normalization.
pub struct CompoundAssignmentCanonical;

impl NormalizationPass for CompoundAssignmentCanonical {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::CompoundAssignmentCanonical
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
        let Some(_op) = matches_self_op_assignment(expr) else {
            return;
        };
        let placeholder = self.builder.expression_null_literal(SPAN);
        let owned = mem::replace(expr, placeholder);
        let Expression::AssignmentExpression(assign_box) = owned else {
            unreachable!()
        };
        let assign = assign_box.unbox();
        // LHS must be a plain Identifier (verified by
        // matches_self_op_assignment). Extract its name.
        let lhs_name = match &assign.left {
            AssignmentTarget::AssignmentTargetIdentifier(id) => id.name.clone(),
            _ => unreachable!("matches_self_op_assignment verified identifier LHS"),
        };
        let Expression::BinaryExpression(bin_box) = assign.right else {
            unreachable!()
        };
        let bin = bin_box.unbox();
        let compound_op = compound_op_for(bin.operator)
            .expect("matches_self_op_assignment already verified compound exists");
        let new_lhs = AssignmentTarget::AssignmentTargetIdentifier(
            self.builder
                .alloc(self.builder.identifier_reference(SPAN, lhs_name.as_str())),
        );
        // RHS becomes the binary expression's right operand.
        *expr = Expression::AssignmentExpression(
            self.builder.alloc(self.builder.assignment_expression(
                SPAN,
                compound_op,
                new_lhs,
                bin.right,
            )),
        );
    }
}

/// Match the shape `Ident(name) = Ident(name) <op> rhs` where `op`
/// has a compound counterpart. Returns the binary operator if match.
fn matches_self_op_assignment(expr: &Expression<'_>) -> Option<BinaryOperator> {
    let Expression::AssignmentExpression(assign) = expr else {
        return None;
    };
    if !matches!(assign.operator, AssignmentOperator::Assign) {
        return None;
    }
    let AssignmentTarget::AssignmentTargetIdentifier(lhs) = &assign.left else {
        return None;
    };
    let Expression::BinaryExpression(bin) = &assign.right else {
        return None;
    };
    let _ = compound_op_for(bin.operator)?;
    // Inner LHS-of-binary must be the same identifier name.
    let Expression::Identifier(inner) = &bin.left else {
        return None;
    };
    if inner.name != lhs.name {
        return None;
    }
    Some(bin.operator)
}

/// Map a binary operator to its compound-assignment counterpart, or
/// `None` for operators that don't have one (`==`, `<`, `instanceof`, …).
fn compound_op_for(op: BinaryOperator) -> Option<AssignmentOperator> {
    Some(match op {
        BinaryOperator::Addition => AssignmentOperator::Addition,
        BinaryOperator::Subtraction => AssignmentOperator::Subtraction,
        BinaryOperator::Multiplication => AssignmentOperator::Multiplication,
        BinaryOperator::Division => AssignmentOperator::Division,
        BinaryOperator::Remainder => AssignmentOperator::Remainder,
        BinaryOperator::Exponential => AssignmentOperator::Exponential,
        BinaryOperator::ShiftLeft => AssignmentOperator::ShiftLeft,
        BinaryOperator::ShiftRight => AssignmentOperator::ShiftRight,
        BinaryOperator::ShiftRightZeroFill => AssignmentOperator::ShiftRightZeroFill,
        BinaryOperator::BitwiseAnd => AssignmentOperator::BitwiseAnd,
        BinaryOperator::BitwiseOR => AssignmentOperator::BitwiseOR,
        BinaryOperator::BitwiseXOR => AssignmentOperator::BitwiseXOR,
        _ => return None,
    })
}

// Suppress the unused-import warning for SimpleAssignmentTarget; it
// shows up via macro-expanded patterns in the OXC AST.
#[allow(dead_code)]
fn _force_use_simple_assignment_target<'a>(_: SimpleAssignmentTarget<'a>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn rewrites_plus_self_assignment() {
        let out = apply_to_source(
            &CompoundAssignmentCanonical,
            "function f(x) { x = x + 1; return x; }",
        )
        .expect("parse");
        assert!(out.contains("x += 1"), "got: {out}");
        assert!(!out.contains("x = x +"), "got: {out}");
    }

    #[test]
    fn rewrites_minus_self_assignment() {
        let out = apply_to_source(
            &CompoundAssignmentCanonical,
            "function f(x) { x = x - 1; return x; }",
        )
        .expect("parse");
        assert!(out.contains("x -= 1"), "got: {out}");
    }

    #[test]
    fn rewrites_times_with_call_rhs() {
        let out = apply_to_source(
            &CompoundAssignmentCanonical,
            "function f(x) { x = x * factor(); return x; }",
        )
        .expect("parse");
        assert!(out.contains("x *= factor()"), "got: {out}");
    }

    #[test]
    fn rewrites_bitwise_or() {
        let out = apply_to_source(
            &CompoundAssignmentCanonical,
            "function f(flags) { flags = flags | 0x10; return flags; }",
        )
        .expect("parse");
        assert!(
            out.contains("flags |= 16") || out.contains("flags |= 0x10"),
            "got: {out}"
        );
    }

    #[test]
    fn leaves_member_lhs_alone() {
        // `obj.x = obj.x + 1` — could double-evaluate getter.
        let src = "function f(obj) { obj.x = obj.x + 1; }";
        let out = apply_to_source(&CompoundAssignmentCanonical, src).expect("parse");
        assert!(out.contains("obj.x = obj.x"), "got: {out}");
    }

    #[test]
    fn leaves_computed_lhs_alone() {
        let src = "function f(arr, i) { arr[i] = arr[i] + 1; }";
        let out = apply_to_source(&CompoundAssignmentCanonical, src).expect("parse");
        assert!(out.contains("arr[i] = arr[i]"), "got: {out}");
    }

    #[test]
    fn leaves_different_identifier_alone() {
        // `x = y + 1` — not self-op.
        let src = "function f(x, y) { x = y + 1; return x; }";
        let out = apply_to_source(&CompoundAssignmentCanonical, src).expect("parse");
        assert!(out.contains("x = y + 1"), "got: {out}");
    }

    #[test]
    fn leaves_non_compound_operator_alone() {
        // `x = x === y` — strict-equality has no compound assignment.
        let src = "function f(x, y) { x = x === y; }";
        let out = apply_to_source(&CompoundAssignmentCanonical, src).expect("parse");
        assert!(out.contains("x = x === y"), "got: {out}");
    }

    #[test]
    fn idempotent_on_already_compound() {
        let src = "function f(x) { x += 1; return x; }";
        let first = apply_to_source(&CompoundAssignmentCanonical, src).expect("first");
        let second = apply_to_source(&CompoundAssignmentCanonical, &first).expect("second");
        assert_eq!(first, second);
    }
}
