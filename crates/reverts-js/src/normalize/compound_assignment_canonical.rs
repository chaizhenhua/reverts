use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{AssignmentTarget, Expression, Program};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_expression;
use oxc_span::SPAN;
use oxc_syntax::operator::{AssignmentOperator, BinaryOperator};
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `CompoundAssignmentCanonical` rewrites `LHS = LHS op y` into the
/// compound form `LHS op= y` for every binary operator that has a
/// compound counterpart. Two LHS shapes qualify:
///
/// 1. Plain identifier LHS — `x = x + 1` ↔ `x += 1`.
/// 2. **Static member access on a plain identifier receiver** —
///    `obj.field = obj.field + 1` ↔ `obj.field += 1`. The receiver
///    `obj` is an Identifier read (pure, no getters/proxies on a
///    bare variable read), and the property's getter/setter pair is
///    called once in both forms (the long form reads the property
///    once on the RHS and writes once on the LHS; the compound form
///    reads once and writes once — same observable trace).
///
/// The pass deliberately rejects:
///
/// * Computed member LHS (`arr[i] = arr[i] + 1`) — the index
///   expression `i` is read twice in the long form and might have
///   side effects (e.g. `i++`).
/// * Call-expression receivers (`f().x = f().x + 1`) — would invoke
///   `f()` twice; the compound form invokes it once.
/// * Different-property mismatches (`obj.x = obj.y + 1`) — not a
///   self-op.
///
/// All accepted rewrites are **strictly spec-equivalent**: the long
/// form's evaluation order matches the compound form's per §13.15
/// (`Assignment Expressions`) when the LHS receiver is a pure
/// identifier.
pub struct CompoundAssignmentCanonical;

impl NormalizationPass for CompoundAssignmentCanonical {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::CompoundAssignmentCanonical
    }
    fn version(&self) -> u32 {
        2
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
        let new_lhs = match assign.left {
            AssignmentTarget::AssignmentTargetIdentifier(id_box) => {
                let id = id_box.unbox();
                AssignmentTarget::AssignmentTargetIdentifier(
                    self.builder
                        .alloc(self.builder.identifier_reference(SPAN, id.name.as_str())),
                )
            }
            AssignmentTarget::StaticMemberExpression(mem_box) => {
                let mem = mem_box.unbox();
                let Expression::Identifier(obj_id) = mem.object else {
                    unreachable!("matches_self_op_assignment verified identifier receiver")
                };
                let receiver = self
                    .builder
                    .expression_identifier_reference(SPAN, obj_id.name.as_str());
                let prop = self
                    .builder
                    .identifier_name(SPAN, mem.property.name.as_str());
                AssignmentTarget::StaticMemberExpression(
                    self.builder.alloc(self.builder.static_member_expression(
                        SPAN,
                        receiver,
                        prop,
                        mem.optional,
                    )),
                )
            }
            _ => unreachable!("matches_self_op_assignment verified accepted LHS kind"),
        };
        let Expression::BinaryExpression(bin_box) = assign.right else {
            unreachable!()
        };
        let bin = bin_box.unbox();
        let compound_op = compound_op_for(bin.operator)
            .expect("matches_self_op_assignment already verified compound exists");
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

/// Match either of these self-op assignment shapes:
///
/// * `Ident(name) = Ident(name) <op> rhs`
/// * `Ident(obj).prop = Ident(obj).prop <op> rhs`
///
/// where `op` has a compound counterpart. Returns the binary
/// operator if matched, else `None`.
fn matches_self_op_assignment(expr: &Expression<'_>) -> Option<BinaryOperator> {
    let Expression::AssignmentExpression(assign) = expr else {
        return None;
    };
    if !matches!(assign.operator, AssignmentOperator::Assign) {
        return None;
    }
    let Expression::BinaryExpression(bin) = &assign.right else {
        return None;
    };
    let _ = compound_op_for(bin.operator)?;
    match &assign.left {
        AssignmentTarget::AssignmentTargetIdentifier(lhs) => {
            // RHS-left must be the same identifier by name.
            let Expression::Identifier(inner) = &bin.left else {
                return None;
            };
            if inner.name != lhs.name {
                return None;
            }
            Some(bin.operator)
        }
        AssignmentTarget::StaticMemberExpression(lhs) => {
            // Receiver must be a plain identifier (no getters
            // re-invoked, no call expressions to repeat).
            let Expression::Identifier(lhs_obj) = &lhs.object else {
                return None;
            };
            // RHS-left must be a static member with the same
            // identifier receiver AND the same property name.
            let Expression::StaticMemberExpression(rhs_mem) = &bin.left else {
                return None;
            };
            let Expression::Identifier(rhs_obj) = &rhs_mem.object else {
                return None;
            };
            if lhs_obj.name != rhs_obj.name || lhs.property.name != rhs_mem.property.name {
                return None;
            }
            // Both sides must share `optional` (`obj.x` vs `obj?.x`
            // differ in semantics).
            if lhs.optional != rhs_mem.optional {
                return None;
            }
            Some(bin.operator)
        }
        _ => None,
    }
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
    fn rewrites_static_member_lhs_with_identifier_receiver() {
        // `obj.x = obj.x + 1` with `obj` an Identifier — the receiver
        // is pure, getter (if any) is invoked once in both forms, so
        // the compound rewrite is observably equivalent.
        let out = apply_to_source(
            &CompoundAssignmentCanonical,
            "function f(obj) { obj.count = obj.count + 1; }",
        )
        .expect("parse");
        assert!(out.contains("obj.count += 1"), "got: {out}");
        assert!(!out.contains("obj.count = obj.count"), "got: {out}");
    }

    #[test]
    fn rewrites_static_member_lhs_with_minus_operator() {
        let out = apply_to_source(
            &CompoundAssignmentCanonical,
            "function f(state) { state.remaining = state.remaining - amount; }",
        )
        .expect("parse");
        assert!(out.contains("state.remaining -= amount"), "got: {out}");
    }

    #[test]
    fn leaves_different_property_alone() {
        // `obj.x = obj.y + 1` — different properties, not a self-op.
        let src = "function f(obj) { obj.x = obj.y + 1; }";
        let out = apply_to_source(&CompoundAssignmentCanonical, src).expect("parse");
        assert!(out.contains("obj.x = obj.y + 1"), "got: {out}");
    }

    #[test]
    fn leaves_different_receiver_alone() {
        // `a.x = b.x + 1` — different receivers, not a self-op.
        let src = "function f(a, b) { a.x = b.x + 1; }";
        let out = apply_to_source(&CompoundAssignmentCanonical, src).expect("parse");
        assert!(out.contains("a.x = b.x + 1"), "got: {out}");
    }

    #[test]
    fn leaves_call_receiver_alone() {
        // `f().x = f().x + 1` — would call `f()` twice in the long
        // form vs once in the compound form. Stay rejected.
        let src = "function g(f) { f().x = f().x + 1; }";
        let out = apply_to_source(&CompoundAssignmentCanonical, src).expect("parse");
        assert!(out.contains("f().x = f().x"), "got: {out}");
    }

    #[test]
    fn leaves_computed_lhs_alone() {
        // `arr[i] = arr[i] + 1` — `i` could be `i++` etc. Stay rejected.
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
