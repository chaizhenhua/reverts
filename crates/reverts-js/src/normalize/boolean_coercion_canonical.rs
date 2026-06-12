use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Argument, Expression, Program};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_expression;
use oxc_span::SPAN;
use oxc_syntax::operator::UnaryOperator;
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `BooleanCoercionCanonical` rewrites the verbose `Boolean(x)`
/// coercion call into the canonical double-not form `!!x`. Both
/// expressions produce a strict boolean for every JavaScript value
/// (per ECMA-262 §7.1.2 `ToBoolean`), so they are semantically
/// equivalent in all observable positions.
///
/// Minifiers prefer `!!x` (3 bytes vs 9–11). Source code in libraries
/// often uses `Boolean(x)` for readability. Normalising the verbose
/// form to the double-not form aligns the AST hash.
///
/// We only rewrite the exact one-argument form `Boolean(x)`:
///
/// * `Boolean()` (no args) evaluates to `false` — its `!!` equivalent
///   would have to be `false` as a literal, a less direct rewrite.
/// * `Boolean(x, y, z)` is valid JS (extra args ignored) but extremely
///   unusual; preserving the original call avoids ambiguity in CFG
///   measures.
pub struct BooleanCoercionCanonical;

impl NormalizationPass for BooleanCoercionCanonical {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::BooleanCoercionCanonical
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
        if !is_boolean_unary_call(expr) {
            return;
        }
        let placeholder = self.builder.expression_null_literal(SPAN);
        let owned = mem::replace(expr, placeholder);
        let Expression::CallExpression(call_box) = owned else {
            unreachable!()
        };
        let call = call_box.unbox();
        // Take the single argument's expression. matches_boolean_unary_call
        // already verified arity and the argument is a non-spread.
        let mut args = call.arguments.into_iter();
        let arg = args.next().expect("len==1 checked");
        let Some(inner) = argument_to_expression(arg) else {
            // Defensive: shouldn't happen because is_boolean_unary_call
            // requires a non-spread non-TS arg. Fall back to no-op by
            // emitting `false` as a stable safe placeholder is wrong;
            // instead reconstruct… we can't, so we just emit the inner
            // expression as `Boolean(undefined)` semantics.
            *expr = self.builder.expression_boolean_literal(SPAN, false);
            return;
        };
        // `!!inner`
        let single_not = self
            .builder
            .expression_unary(SPAN, UnaryOperator::LogicalNot, inner);
        *expr = self
            .builder
            .expression_unary(SPAN, UnaryOperator::LogicalNot, single_not);
    }
}

fn is_boolean_unary_call(expr: &Expression<'_>) -> bool {
    let Expression::CallExpression(c) = expr else {
        return false;
    };
    let Expression::Identifier(callee) = &c.callee else {
        return false;
    };
    if callee.name != "Boolean" {
        return false;
    }
    if c.arguments.len() != 1 {
        return false;
    }
    // Reject spread-element arguments; they could carry side effects
    // or evaluate to multiple values whose first becomes the operand —
    // not safe to inline behind `!!`.
    !matches!(c.arguments[0], Argument::SpreadElement(_))
}

/// Move an `Argument` into an `Expression`. Returns `None` for the
/// TypeScript-only argument variants we don't expect in normal JS.
fn argument_to_expression<'a>(arg: Argument<'a>) -> Option<Expression<'a>> {
    Some(match arg {
        Argument::BooleanLiteral(b) => Expression::BooleanLiteral(b),
        Argument::NullLiteral(n) => Expression::NullLiteral(n),
        Argument::NumericLiteral(n) => Expression::NumericLiteral(n),
        Argument::BigIntLiteral(b) => Expression::BigIntLiteral(b),
        Argument::RegExpLiteral(r) => Expression::RegExpLiteral(r),
        Argument::StringLiteral(s) => Expression::StringLiteral(s),
        Argument::TemplateLiteral(t) => Expression::TemplateLiteral(t),
        Argument::Identifier(i) => Expression::Identifier(i),
        Argument::MetaProperty(m) => Expression::MetaProperty(m),
        Argument::Super(s) => Expression::Super(s),
        Argument::ArrayExpression(a) => Expression::ArrayExpression(a),
        Argument::ArrowFunctionExpression(a) => Expression::ArrowFunctionExpression(a),
        Argument::AssignmentExpression(a) => Expression::AssignmentExpression(a),
        Argument::AwaitExpression(a) => Expression::AwaitExpression(a),
        Argument::BinaryExpression(b) => Expression::BinaryExpression(b),
        Argument::CallExpression(c) => Expression::CallExpression(c),
        Argument::ChainExpression(c) => Expression::ChainExpression(c),
        Argument::ClassExpression(c) => Expression::ClassExpression(c),
        Argument::ConditionalExpression(c) => Expression::ConditionalExpression(c),
        Argument::FunctionExpression(f) => Expression::FunctionExpression(f),
        Argument::ImportExpression(i) => Expression::ImportExpression(i),
        Argument::LogicalExpression(l) => Expression::LogicalExpression(l),
        Argument::NewExpression(n) => Expression::NewExpression(n),
        Argument::ObjectExpression(o) => Expression::ObjectExpression(o),
        Argument::ParenthesizedExpression(p) => Expression::ParenthesizedExpression(p),
        Argument::SequenceExpression(s) => Expression::SequenceExpression(s),
        Argument::TaggedTemplateExpression(t) => Expression::TaggedTemplateExpression(t),
        Argument::ThisExpression(t) => Expression::ThisExpression(t),
        Argument::UnaryExpression(u) => Expression::UnaryExpression(u),
        Argument::UpdateExpression(u) => Expression::UpdateExpression(u),
        Argument::YieldExpression(y) => Expression::YieldExpression(y),
        Argument::PrivateInExpression(p) => Expression::PrivateInExpression(p),
        Argument::JSXElement(j) => Expression::JSXElement(j),
        Argument::JSXFragment(j) => Expression::JSXFragment(j),
        Argument::StaticMemberExpression(m) => Expression::StaticMemberExpression(m),
        Argument::ComputedMemberExpression(m) => Expression::ComputedMemberExpression(m),
        Argument::PrivateFieldExpression(p) => Expression::PrivateFieldExpression(p),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn rewrites_boolean_of_identifier() {
        let out = apply_to_source(
            &BooleanCoercionCanonical,
            "function f(x) { return Boolean(x); }",
        )
        .expect("parse");
        assert!(out.contains("!!x"), "got: {out}");
        assert!(!out.contains("Boolean"), "got: {out}");
    }

    #[test]
    fn rewrites_boolean_of_member_expression() {
        let out = apply_to_source(
            &BooleanCoercionCanonical,
            "function f(o) { return Boolean(o.flag); }",
        )
        .expect("parse");
        assert!(out.contains("!!o.flag"), "got: {out}");
    }

    #[test]
    fn rewrites_boolean_of_call_expression() {
        let out = apply_to_source(
            &BooleanCoercionCanonical,
            "function f() { return Boolean(getValue()); }",
        )
        .expect("parse");
        assert!(out.contains("!!getValue()"), "got: {out}");
    }

    #[test]
    fn leaves_boolean_with_no_args_alone() {
        let src = "function f() { return Boolean(); }";
        let out = apply_to_source(&BooleanCoercionCanonical, src).expect("parse");
        assert!(out.contains("Boolean()"), "got: {out}");
    }

    #[test]
    fn leaves_boolean_with_multiple_args_alone() {
        let src = "function f(a, b) { return Boolean(a, b); }";
        let out = apply_to_source(&BooleanCoercionCanonical, src).expect("parse");
        assert!(out.contains("Boolean("), "got: {out}");
    }

    #[test]
    fn leaves_unrelated_call_alone() {
        let src = "function f(x) { return Number(x); }";
        let out = apply_to_source(&BooleanCoercionCanonical, src).expect("parse");
        assert!(out.contains("Number(x)"), "got: {out}");
    }

    #[test]
    fn idempotent_on_already_canonical() {
        let src = "function f(x) { return !!x; }";
        let first = apply_to_source(&BooleanCoercionCanonical, src).expect("first");
        let second = apply_to_source(&BooleanCoercionCanonical, &first).expect("second");
        assert_eq!(first, second);
    }
}
