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

/// `BooleanCallToDoubleNotGuarded` rewrites the spec-equivalent
/// coercion form `Boolean(x)` into `!!x` — **but only when a static
/// scope check proves `Boolean` is not shadowed anywhere in the
/// program** (no local binding named `Boolean`, no `with` statement).
///
/// Per ECMA-262 §20.3.1.1 (`Boolean ( value )`), calling the global
/// `Boolean` as a function returns `ToBoolean(value)`. Per §13.5.7
/// (`UnaryExpression: ! UnaryExpression`), `!x` is the negation of
/// `ToBoolean(x)`; `!!x` is its double-negation, which recovers the
/// same boolean. So `Boolean(x)` and `!!x` produce **exactly** the
/// same value for every operand, including `NaN`, `Symbol`,
/// `document.all`, and exotic proxies — **as long as the
/// identifier `Boolean` refers to the global constructor**.
///
/// When a local binding `Boolean` shadows the global, calling it
/// could mean anything. The guard makes this rewrite strictly
/// spec-equivalent only when shadowing is proven absent.
///
/// The pass restricts to:
///
/// * Direct `Boolean(arg)` calls — not `obj.Boolean(arg)` (different
///   identity), not `new Boolean(arg)` (returns a wrapper object).
/// * Exactly one positional argument, non-spread. `Boolean()` returns
///   `false`; `!!` requires an operand. `Boolean(a, b)` ignores `b`
///   per spec but `!!a` would drop it — extras might have side
///   effects.
pub struct BooleanCallToDoubleNotGuarded;

impl NormalizationPass for BooleanCallToDoubleNotGuarded {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::BooleanCallToDoubleNotGuarded
    }
    fn version(&self) -> u32 {
        1
    }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        if super::shadow_check::program_can_shadow(program, "Boolean") {
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

impl<'a> VisitMut<'a> for Rewriter<'a> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        walk_expression(self, expr);
        if !matches_boolean_call(expr) {
            return;
        }
        let placeholder = self.builder.expression_null_literal(SPAN);
        let owned = mem::replace(expr, placeholder);
        let Expression::CallExpression(call_box) = owned else {
            unreachable!()
        };
        let call = call_box.unbox();
        let mut args = call.arguments;
        let arg = args
            .pop()
            .expect("matches_boolean_call verified exactly one argument");
        let arg_expr = match arg {
            Argument::SpreadElement(_) => {
                unreachable!("matches_boolean_call excluded spread arguments")
            }
            other => other.into_expression(),
        };
        let inner = self
            .builder
            .expression_unary(SPAN, UnaryOperator::LogicalNot, arg_expr);
        *expr = self
            .builder
            .expression_unary(SPAN, UnaryOperator::LogicalNot, inner);
    }
}

fn matches_boolean_call(expr: &Expression<'_>) -> bool {
    let Expression::CallExpression(c) = expr else {
        return false;
    };
    let Expression::Identifier(i) = &c.callee else {
        return false;
    };
    if i.name.as_str() != "Boolean" {
        return false;
    }
    if c.arguments.len() != 1 {
        return false;
    }
    !matches!(c.arguments.first(), Some(Argument::SpreadElement(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn rewrites_boolean_call_when_not_shadowed() {
        let out = apply_to_source(
            &BooleanCallToDoubleNotGuarded,
            "function f(x) { return Boolean(x); }",
        )
        .expect("parse");
        assert!(out.contains("!!x"), "got: {out}");
        assert!(!out.contains("Boolean("), "got: {out}");
    }

    #[test]
    fn rewrites_boolean_call_with_complex_arg() {
        let out = apply_to_source(
            &BooleanCallToDoubleNotGuarded,
            "function f(a, b) { return Boolean(a && b); }",
        )
        .expect("parse");
        assert!(out.contains("!!"), "got: {out}");
        assert!(!out.contains("Boolean"), "got: {out}");
    }

    #[test]
    fn bails_out_when_boolean_is_shadowed() {
        let src = r#"
            function f(x) {
                let Boolean = String;
                return Boolean(x);
            }
        "#;
        let out = apply_to_source(&BooleanCallToDoubleNotGuarded, src).expect("parse");
        assert!(out.contains("Boolean(x)"), "got: {out}");
        assert!(!out.contains("!!x"), "got: {out}");
    }

    #[test]
    fn bails_out_on_param_named_boolean() {
        let src = "function f(Boolean, x) { return Boolean(x); }";
        let out = apply_to_source(&BooleanCallToDoubleNotGuarded, src).expect("parse");
        assert!(out.contains("Boolean(x)"), "got: {out}");
    }

    #[test]
    fn bails_out_on_with_statement() {
        let src = r#"
            function f(o, x) {
                with (o) { return Boolean(x); }
            }
        "#;
        let out = apply_to_source(&BooleanCallToDoubleNotGuarded, src).expect("parse");
        assert!(out.contains("Boolean(x)"), "got: {out}");
    }

    #[test]
    fn leaves_zero_arg_boolean_alone() {
        let src = "function f() { return Boolean(); }";
        let out = apply_to_source(&BooleanCallToDoubleNotGuarded, src).expect("parse");
        assert!(out.contains("Boolean()"), "got: {out}");
    }

    #[test]
    fn leaves_multi_arg_boolean_alone() {
        let src = "function f(a, b) { return Boolean(a, b); }";
        let out = apply_to_source(&BooleanCallToDoubleNotGuarded, src).expect("parse");
        assert!(out.contains("Boolean(a, b)"), "got: {out}");
    }

    #[test]
    fn leaves_new_boolean_alone() {
        let src = "function f(x) { return new Boolean(x); }";
        let out = apply_to_source(&BooleanCallToDoubleNotGuarded, src).expect("parse");
        assert!(out.contains("new Boolean"), "got: {out}");
    }

    #[test]
    fn leaves_member_boolean_alone() {
        let src = "function f(x) { return globalThis.Boolean(x); }";
        let out = apply_to_source(&BooleanCallToDoubleNotGuarded, src).expect("parse");
        assert!(out.contains("globalThis.Boolean"), "got: {out}");
    }

    #[test]
    fn leaves_spread_argument_alone() {
        let src = "function f(args) { return Boolean(...args); }";
        let out = apply_to_source(&BooleanCallToDoubleNotGuarded, src).expect("parse");
        assert!(out.contains("Boolean(...args)"), "got: {out}");
    }

    #[test]
    fn idempotent_when_already_double_not() {
        let src = "function f(x) { return !!x; }";
        let first = apply_to_source(&BooleanCallToDoubleNotGuarded, src).expect("first");
        let second = apply_to_source(&BooleanCallToDoubleNotGuarded, &first).expect("second");
        assert_eq!(first, second);
    }
}
