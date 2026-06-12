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

/// `NumberCoercionCanonical` rewrites the verbose `Number(x)` coercion
/// call into the unary `+x` form. Per ECMA-262 §21.1.1 `Number()`
/// called as a function with one argument invokes `ToNumber(x)` and
/// returns the result — exactly what `+x` does (§13.5.6). Both forms
/// throw `TypeError` for `Symbol`/`BigInt` operands in the same way,
/// produce `NaN` for the same inputs, and have no other observable
/// difference.
///
/// Minifiers prefer `+x` (1 byte vs 7+). Source code commonly uses
/// `Number(x)` for clarity. The cascade fingerprint matches via the
/// AST hash, which distinguishes `CallExpression` from
/// `UnaryExpression`; this pass converges the two forms.
///
/// The rewrite fires only for the exact one-argument shape. We do
/// not rewrite `Number()` (no args → returns `0`, has no
/// straightforward `+` analogue) or `Number(x, y)` (extra args are
/// unusual and may indicate a different intent).
pub struct NumberCoercionCanonical;

impl NormalizationPass for NumberCoercionCanonical {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::NumberCoercionCanonical
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
        if !is_number_unary_call(expr) {
            return;
        }
        let placeholder = self.builder.expression_null_literal(SPAN);
        let owned = mem::replace(expr, placeholder);
        let Expression::CallExpression(call_box) = owned else {
            unreachable!()
        };
        let call = call_box.unbox();
        let mut args = call.arguments.into_iter();
        let arg = args.next().expect("len==1 checked");
        let Some(inner) = argument_to_expression(arg) else {
            // Pathological argument variant (TS-only) — leave as
            // boolean-literal sentinel so we don't leave a NullLiteral
            // placeholder. In practice unreachable.
            *expr = self.builder.expression_null_literal(SPAN);
            return;
        };
        *expr = self
            .builder
            .expression_unary(SPAN, UnaryOperator::UnaryPlus, inner);
    }
}

fn is_number_unary_call(expr: &Expression<'_>) -> bool {
    let Expression::CallExpression(c) = expr else {
        return false;
    };
    let Expression::Identifier(callee) = &c.callee else {
        return false;
    };
    if callee.name != "Number" {
        return false;
    }
    if c.arguments.len() != 1 {
        return false;
    }
    !matches!(c.arguments[0], Argument::SpreadElement(_))
}

fn argument_to_expression<'a>(arg: Argument<'a>) -> Option<Expression<'a>> {
    // Same conversion table as `boolean_coercion_canonical`. The two
    // could share a helper module but the table is short enough to
    // duplicate without much harm.
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
    fn rewrites_number_of_identifier() {
        let out = apply_to_source(
            &NumberCoercionCanonical,
            "function f(x) { return Number(x); }",
        )
        .expect("parse");
        assert!(out.contains("+x"), "got: {out}");
        assert!(!out.contains("Number("), "got: {out}");
    }

    #[test]
    fn rewrites_number_of_call() {
        let out = apply_to_source(
            &NumberCoercionCanonical,
            "function f() { return Number(parseInput()); }",
        )
        .expect("parse");
        assert!(out.contains("+parseInput()"), "got: {out}");
    }

    #[test]
    fn leaves_number_with_no_args_alone() {
        let src = "function f() { return Number(); }";
        let out = apply_to_source(&NumberCoercionCanonical, src).expect("parse");
        assert!(out.contains("Number()"), "got: {out}");
    }

    #[test]
    fn leaves_number_with_two_args_alone() {
        // `Number(x, y)` — extra arg is ignored at runtime but the
        // intent is non-trivial; keep the call.
        let src = "function f(a, b) { return Number(a, b); }";
        let out = apply_to_source(&NumberCoercionCanonical, src).expect("parse");
        assert!(out.contains("Number("), "got: {out}");
    }

    #[test]
    fn leaves_other_coercion_calls_alone() {
        let src = "function f(x) { return Boolean(x); }";
        let out = apply_to_source(&NumberCoercionCanonical, src).expect("parse");
        assert!(out.contains("Boolean(x)"), "got: {out}");
    }

    #[test]
    fn idempotent_on_already_canonical() {
        let src = "function f(x) { return +x; }";
        let first = apply_to_source(&NumberCoercionCanonical, src).expect("first");
        let second = apply_to_source(&NumberCoercionCanonical, &first).expect("second");
        assert_eq!(first, second);
    }
}
