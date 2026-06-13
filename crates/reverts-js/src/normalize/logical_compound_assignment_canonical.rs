use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{AssignmentTarget, Expression, Program};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_expression;
use oxc_span::SPAN;
use oxc_syntax::operator::{AssignmentOperator, LogicalOperator};
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `LogicalCompoundAssignmentCanonical` rewrites
/// `x = x <logical_op> rhs` (where `<logical_op>` is `||`, `&&`, or
/// `??`) into the ES2021 logical-assignment form `x <logical_op>= rhs`.
///
/// Restricted to plain-identifier LHS — same scope-equivalence
/// argument as `compound_assignment_canonical`:
///
/// * Identifier reads are pure (no getter/proxy concerns), so reading
///   `x` once vs twice doesn't matter.
/// * For local variables, the "self-write" in the long form
///   (`x = x` when the short-circuit returns the truthy/non-nullish
///   value) is observably identical to the no-write in the short form.
///   Neither form invokes a setter; both leave the binding with the
///   same value.
///
/// Per ECMA-262:
///
/// * §13.15.3 (`LogicalAssignment` evaluation) — `target <op>= value`
///   reads target, short-circuits on the appropriate condition, and
///   only evaluates+assigns `value` when the short-circuit fails.
/// * §13.13.1 (`LogicalExpression` evaluation) — same short-circuit
///   logic, but always followed by the assignment in the
///   `x = x || rhs` form.
///
/// For identifier LHS the two forms produce identical observable
/// state. The rewrite is **strictly spec-equivalent**.
///
/// Member-expression LHS (`obj.foo = obj.foo || rhs`) is **not**
/// rewritten because the long form invokes the setter unconditionally
/// while the short form only fires the setter on the false-branch —
/// observably different for objects with custom setters or Proxies.
pub struct LogicalCompoundAssignmentCanonical;

impl NormalizationPass for LogicalCompoundAssignmentCanonical {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::LogicalCompoundAssignmentCanonical
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
        let Some(_) = matches_self_logical_assignment(expr) else {
            return;
        };
        let placeholder = self.builder.expression_null_literal(SPAN);
        let owned = mem::replace(expr, placeholder);
        let Expression::AssignmentExpression(assign_box) = owned else {
            unreachable!()
        };
        let assign = assign_box.unbox();
        let AssignmentTarget::AssignmentTargetIdentifier(lhs_box) = assign.left else {
            unreachable!("matches_self_logical_assignment verified identifier LHS")
        };
        let lhs = lhs_box.unbox();
        let Expression::LogicalExpression(log_box) = assign.right else {
            unreachable!()
        };
        let log = log_box.unbox();
        let compound_op = compound_op_for(log.operator);
        let new_lhs = AssignmentTarget::AssignmentTargetIdentifier(
            self.builder
                .alloc(self.builder.identifier_reference(SPAN, lhs.name.as_str())),
        );
        *expr = self
            .builder
            .expression_assignment(SPAN, compound_op, new_lhs, log.right);
    }
}

fn matches_self_logical_assignment(expr: &Expression<'_>) -> Option<LogicalOperator> {
    let Expression::AssignmentExpression(assign) = expr else {
        return None;
    };
    if !matches!(assign.operator, AssignmentOperator::Assign) {
        return None;
    }
    let AssignmentTarget::AssignmentTargetIdentifier(lhs) = &assign.left else {
        return None;
    };
    let Expression::LogicalExpression(log) = &assign.right else {
        return None;
    };
    // Inner LHS-of-logical must be the same identifier by name.
    let Expression::Identifier(inner) = &log.left else {
        return None;
    };
    if inner.name != lhs.name {
        return None;
    }
    Some(log.operator)
}

fn compound_op_for(op: LogicalOperator) -> AssignmentOperator {
    match op {
        LogicalOperator::Or => AssignmentOperator::LogicalOr,
        LogicalOperator::And => AssignmentOperator::LogicalAnd,
        LogicalOperator::Coalesce => AssignmentOperator::LogicalNullish,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn rewrites_or_self_assignment() {
        let out = apply_to_source(
            &LogicalCompoundAssignmentCanonical,
            "function f(x, v) { x = x || v; return x; }",
        )
        .expect("parse");
        assert!(out.contains("x ||= v"), "got: {out}");
    }

    #[test]
    fn rewrites_and_self_assignment() {
        let out = apply_to_source(
            &LogicalCompoundAssignmentCanonical,
            "function f(x, v) { x = x && v; return x; }",
        )
        .expect("parse");
        assert!(out.contains("x &&= v"), "got: {out}");
    }

    #[test]
    fn rewrites_nullish_self_assignment() {
        let out = apply_to_source(
            &LogicalCompoundAssignmentCanonical,
            "function f(x, v) { x = x ?? v; return x; }",
        )
        .expect("parse");
        assert!(out.contains("x ??= v"), "got: {out}");
    }

    #[test]
    fn leaves_member_lhs_alone() {
        // `obj.foo = obj.foo || v` — setter would fire unconditionally
        // in the long form but only on false-branch in the compound
        // form. Different. Reject.
        let src = "function f(obj, v) { obj.foo = obj.foo || v; }";
        let out = apply_to_source(&LogicalCompoundAssignmentCanonical, src).expect("parse");
        assert!(out.contains("obj.foo = obj.foo || v"), "got: {out}");
    }

    #[test]
    fn leaves_different_identifier_alone() {
        let src = "function f(x, y, v) { x = y || v; return x; }";
        let out = apply_to_source(&LogicalCompoundAssignmentCanonical, src).expect("parse");
        assert!(out.contains("x = y || v"), "got: {out}");
    }

    #[test]
    fn leaves_non_logical_op_alone() {
        // `x = x + v` is arithmetic; handled by compound_assignment_canonical.
        let src = "function f(x, v) { x = x + v; return x; }";
        let out = apply_to_source(&LogicalCompoundAssignmentCanonical, src).expect("parse");
        assert!(out.contains("x = x + v"), "got: {out}");
    }

    #[test]
    fn idempotent_on_already_compound() {
        let src = "function f(x, v) { x ||= v; return x; }";
        let first = apply_to_source(&LogicalCompoundAssignmentCanonical, src).expect("first");
        let second = apply_to_source(&LogicalCompoundAssignmentCanonical, &first).expect("second");
        assert_eq!(first, second);
    }
}
