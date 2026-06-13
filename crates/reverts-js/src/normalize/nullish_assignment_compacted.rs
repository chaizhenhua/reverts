use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{AssignmentTarget, Expression, Program, Statement};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::{walk_block_statement, walk_function_body};
use oxc_span::SPAN;
use oxc_syntax::operator::{AssignmentOperator, BinaryOperator};
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `NullishAssignmentCompacted` rewrites the explicit
/// "set-if-nullish" pattern into the compact `??=` form:
///
/// ```text
/// if (obj.foo == null) obj.foo = val;     →   obj.foo ??= val;
/// if (x == null) x = val;                 →   x ??= val;
/// ```
///
/// Per ECMA-262:
///
/// * §13.15.2 (`LogicalAssignment` evaluation) — `target ??= value`
///   evaluates `target` once; if the result is not null and not
///   undefined, the expression's value is that result and no
///   assignment happens. Otherwise `value` is evaluated and assigned
///   to `target`.
/// * §7.2.14 (`IsLooselyEqual`) — `null == undefined`,
///   `null == null`, and `undefined == undefined` all evaluate to
///   `true`; every other operand pair (excluding the operands above
///   that produce the same equivalence) is `false`. So `x == null`
///   is exactly the "is null or undefined" predicate.
///
/// Both forms read the LHS once (the explicit if reads `obj.foo` in
/// the test, then writes — the compound form is defined as "read,
/// short-circuit, write" with one read). Same getter call count, same
/// setter call count. The rewrite is **strictly spec-equivalent**
/// when the LHS receiver is a pure identifier (no double-evaluation
/// concern — and we already enforce that for the static-member case).
///
/// The pass restricts to:
///
/// * Statement position only — `if (...) ...` is a statement, not
///   an expression. Embedded conditionals aren't this shape.
/// * IfStatement with `alternate: None`. An `else` branch makes the
///   semantics differ.
/// * Test must be `<LHS> == null` (or commutative `null == <LHS>`).
///   `=== null` is too strict (misses `undefined`) and wouldn't fold
///   to `??=`.
/// * Body must be a single AssignmentExpressionStatement whose LHS
///   matches the test's operand exactly (by name and structure) and
///   whose operator is `=`.
/// * LHS must be either a plain identifier OR a `Identifier.prop`
///   static member access (pure-identifier receiver). Computed
///   member access and call-expression receivers are rejected for
///   the same reason `compound_assignment_canonical` rejects them.
pub struct NullishAssignmentCompacted;

impl NormalizationPass for NullishAssignmentCompacted {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::NullishAssignmentCompacted
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
        if !matches_nullish_if(stmt) {
            continue;
        }
        let placeholder = builder.statement_empty(SPAN);
        let Statement::IfStatement(if_box) = mem::replace(stmt, placeholder) else {
            unreachable!()
        };
        let if_stmt = if_box.unbox();
        // Extract the inner assignment.
        let body_stmt = unwrap_single_stmt(if_stmt.consequent);
        let Statement::ExpressionStatement(expr_box) = body_stmt else {
            unreachable!("matches_nullish_if verified single ExpressionStatement consequent")
        };
        let expr = expr_box.unbox().expression;
        let Expression::AssignmentExpression(assign_box) = expr else {
            unreachable!()
        };
        let assign = assign_box.unbox();
        // Rebuild as `<LHS> ??= rhs`.
        let new_assign = builder.expression_assignment(
            SPAN,
            AssignmentOperator::LogicalNullish,
            assign.left,
            assign.right,
        );
        *stmt = builder.statement_expression(SPAN, new_assign);
    }
}

fn unwrap_single_stmt<'a>(stmt: Statement<'a>) -> Statement<'a> {
    match stmt {
        Statement::BlockStatement(b_box) => {
            let mut b = b_box.unbox();
            // `matches_nullish_if` only accepts a block with exactly
            // one statement, or a bare statement directly. Any other
            // shape would have been rejected before we got here.
            assert_eq!(
                b.body.len(),
                1,
                "matches_nullish_if must have verified single-statement consequent"
            );
            b.body.remove(0)
        }
        other => other,
    }
}

fn matches_nullish_if(stmt: &Statement<'_>) -> bool {
    let Statement::IfStatement(if_stmt) = stmt else {
        return false;
    };
    if if_stmt.alternate.is_some() {
        return false;
    }
    // Test must be `<LHS> == null` or `null == <LHS>`.
    let Some(test_lhs) = test_target(&if_stmt.test) else {
        return false;
    };
    // Body must be a single ExpressionStatement with a `=`
    // assignment whose LHS matches the test target.
    let body_stmt = peek_single_stmt(&if_stmt.consequent);
    let Some(Statement::ExpressionStatement(e)) = body_stmt else {
        return false;
    };
    let Expression::AssignmentExpression(assign) = &e.expression else {
        return false;
    };
    if !matches!(assign.operator, AssignmentOperator::Assign) {
        return false;
    }
    target_matches(&assign.left, test_lhs)
}

#[derive(Copy, Clone)]
enum TestTarget<'a> {
    Identifier(&'a str),
    Member { object: &'a str, property: &'a str },
}

fn test_target<'a>(expr: &'a Expression<'_>) -> Option<TestTarget<'a>> {
    let Expression::BinaryExpression(bin) = expr else {
        return None;
    };
    if !matches!(bin.operator, BinaryOperator::Equality) {
        return None;
    }
    let (target_expr, other) = match (&bin.left, &bin.right) {
        (e, Expression::NullLiteral(_)) => (e, &bin.right),
        (Expression::NullLiteral(_), e) => (e, &bin.left),
        _ => return None,
    };
    let _ = other;
    match target_expr {
        Expression::Identifier(i) => Some(TestTarget::Identifier(i.name.as_str())),
        Expression::StaticMemberExpression(m) => {
            let Expression::Identifier(obj) = &m.object else {
                return None;
            };
            Some(TestTarget::Member {
                object: obj.name.as_str(),
                property: m.property.name.as_str(),
            })
        }
        _ => None,
    }
}

fn target_matches(target: &AssignmentTarget<'_>, test_lhs: TestTarget<'_>) -> bool {
    match (target, test_lhs) {
        (AssignmentTarget::AssignmentTargetIdentifier(id), TestTarget::Identifier(name)) => {
            id.name.as_str() == name
        }
        (AssignmentTarget::StaticMemberExpression(m), TestTarget::Member { object, property }) => {
            let Expression::Identifier(obj) = &m.object else {
                return false;
            };
            obj.name.as_str() == object && m.property.name.as_str() == property && !m.optional
        }
        _ => false,
    }
}

fn peek_single_stmt<'a, 'b>(stmt: &'b Statement<'a>) -> Option<&'b Statement<'a>> {
    match stmt {
        Statement::BlockStatement(b) if b.body.len() == 1 => Some(&b.body[0]),
        Statement::BlockStatement(_) => None,
        other => Some(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn compacts_identifier_target() {
        let out = apply_to_source(
            &NullishAssignmentCompacted,
            "function f(x, val) { if (x == null) x = val; }",
        )
        .expect("parse");
        assert!(out.contains("x ??= val"), "got: {out}");
        assert!(!out.contains("if ("), "got: {out}");
    }

    #[test]
    fn compacts_static_member_target() {
        let out = apply_to_source(
            &NullishAssignmentCompacted,
            "function f(obj, val) { if (obj.foo == null) obj.foo = val; }",
        )
        .expect("parse");
        assert!(out.contains("obj.foo ??= val"), "got: {out}");
    }

    #[test]
    fn compacts_with_block_body() {
        let out = apply_to_source(
            &NullishAssignmentCompacted,
            "function f(obj, val) { if (obj.x == null) { obj.x = val; } }",
        )
        .expect("parse");
        assert!(out.contains("obj.x ??= val"), "got: {out}");
    }

    #[test]
    fn compacts_with_commutative_test() {
        let out = apply_to_source(
            &NullishAssignmentCompacted,
            "function f(x, val) { if (null == x) x = val; }",
        )
        .expect("parse");
        assert!(out.contains("x ??= val"), "got: {out}");
    }

    #[test]
    fn leaves_strict_null_test_alone() {
        // `=== null` is too strict (misses undefined) — does NOT
        // match `??=` semantics.
        let src = "function f(x, val) { if (x === null) x = val; }";
        let out = apply_to_source(&NullishAssignmentCompacted, src).expect("parse");
        assert!(out.contains("if ("), "got: {out}");
        assert!(!out.contains("??="), "got: {out}");
    }

    #[test]
    fn leaves_if_with_else_alone() {
        let src = "function f(x, val) { if (x == null) x = val; else x = 0; }";
        let out = apply_to_source(&NullishAssignmentCompacted, src).expect("parse");
        assert!(out.contains("if ("), "got: {out}");
    }

    #[test]
    fn leaves_mismatched_target_alone() {
        // Test checks `x`, body assigns to `y` — not a self-op.
        let src = "function f(x, y, val) { if (x == null) y = val; }";
        let out = apply_to_source(&NullishAssignmentCompacted, src).expect("parse");
        assert!(out.contains("if ("), "got: {out}");
    }

    #[test]
    fn leaves_computed_target_alone() {
        // `obj[k]` — index could be `k++` etc.; same concern as
        // compound_assignment_canonical.
        let src = "function f(obj, k, val) { if (obj[k] == null) obj[k] = val; }";
        let out = apply_to_source(&NullishAssignmentCompacted, src).expect("parse");
        assert!(out.contains("if ("), "got: {out}");
    }

    #[test]
    fn leaves_call_receiver_target_alone() {
        // `f().x` — would call `f()` twice in the long form.
        let src = "function g(f, val) { if (f().x == null) f().x = val; }";
        let out = apply_to_source(&NullishAssignmentCompacted, src).expect("parse");
        assert!(out.contains("if ("), "got: {out}");
    }

    #[test]
    fn idempotent_on_already_compacted() {
        let src = "function f(x, val) { x ??= val; }";
        let first = apply_to_source(&NullishAssignmentCompacted, src).expect("first");
        let second = apply_to_source(&NullishAssignmentCompacted, &first).expect("second");
        assert_eq!(first, second);
    }
}
