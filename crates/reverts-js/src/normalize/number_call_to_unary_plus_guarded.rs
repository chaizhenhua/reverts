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

/// `NumberCallToUnaryPlusGuarded` rewrites `Number(x)` into `+x` —
/// **but only when a static scope check proves `Number` is not
/// shadowed anywhere in the program** (no local binding named
/// `Number`, no `with` statement).
///
/// Per ECMA-262 §21.1.1.1 (`Number ( value )`), calling the global
/// `Number` as a function returns `ToNumber(value)`. Per §13.5.4
/// (`UnaryExpression: + UnaryExpression`), the result of `+x` is
/// `ToNumber(x)`. The two forms therefore produce **exactly** the same
/// value for every operand, including `NaN`, `BigInt` (both throw
/// `TypeError`), `Symbol` (both throw `TypeError`), and objects with
/// custom `Symbol.toPrimitive` — **as long as the identifier
/// `Number` refers to the global constructor**.
///
/// Restrictions mirror the `Boolean` guarded pass:
///
/// * Direct `Number(arg)` calls only.
/// * Exactly one positional, non-spread argument. `Number()` is `0`;
///   `Number(a, b)` ignores `b` but `+a` would silently drop it.
/// * `new Number(arg)` constructs a wrapper object — left alone.
pub struct NumberCallToUnaryPlusGuarded;

impl NormalizationPass for NumberCallToUnaryPlusGuarded {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::NumberCallToUnaryPlusGuarded
    }
    fn version(&self) -> u32 {
        1
    }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        if super::shadow_check::program_can_shadow(program, "Number") {
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
        if !matches_number_call(expr) {
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
            .expect("matches_number_call verified exactly one argument");
        let arg_expr = match arg {
            Argument::SpreadElement(_) => {
                unreachable!("matches_number_call excluded spread arguments")
            }
            other => other.into_expression(),
        };
        *expr = self
            .builder
            .expression_unary(SPAN, UnaryOperator::UnaryPlus, arg_expr);
    }
}

fn matches_number_call(expr: &Expression<'_>) -> bool {
    let Expression::CallExpression(c) = expr else {
        return false;
    };
    let Expression::Identifier(i) = &c.callee else {
        return false;
    };
    if i.name.as_str() != "Number" {
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
    fn rewrites_number_call_when_not_shadowed() {
        let out = apply_to_source(
            &NumberCallToUnaryPlusGuarded,
            "function f(x) { return Number(x); }",
        )
        .expect("parse");
        assert!(out.contains("+x"), "got: {out}");
        assert!(!out.contains("Number("), "got: {out}");
    }

    #[test]
    fn rewrites_number_call_on_string_arg() {
        let out = apply_to_source(
            &NumberCallToUnaryPlusGuarded,
            r#"function f(s) { return Number(s); }"#,
        )
        .expect("parse");
        assert!(out.contains("+s"), "got: {out}");
    }

    #[test]
    fn bails_out_when_number_is_shadowed() {
        let src = r#"
            function f(x) {
                let Number = String;
                return Number(x);
            }
        "#;
        let out = apply_to_source(&NumberCallToUnaryPlusGuarded, src).expect("parse");
        assert!(out.contains("Number(x)"), "got: {out}");
        assert!(!out.contains("+x"), "got: {out}");
    }

    #[test]
    fn bails_out_on_param_named_number() {
        let src = "function f(Number, x) { return Number(x); }";
        let out = apply_to_source(&NumberCallToUnaryPlusGuarded, src).expect("parse");
        assert!(out.contains("Number(x)"), "got: {out}");
    }

    #[test]
    fn bails_out_on_with_statement() {
        let src = r#"
            function f(o, x) {
                with (o) { return Number(x); }
            }
        "#;
        let out = apply_to_source(&NumberCallToUnaryPlusGuarded, src).expect("parse");
        assert!(out.contains("Number(x)"), "got: {out}");
    }

    #[test]
    fn leaves_zero_arg_number_alone() {
        // `Number()` returns 0; `+ <nothing>` is invalid.
        let src = "function f() { return Number(); }";
        let out = apply_to_source(&NumberCallToUnaryPlusGuarded, src).expect("parse");
        assert!(out.contains("Number()"), "got: {out}");
    }

    #[test]
    fn leaves_multi_arg_number_alone() {
        let src = "function f(a, b) { return Number(a, b); }";
        let out = apply_to_source(&NumberCallToUnaryPlusGuarded, src).expect("parse");
        assert!(out.contains("Number(a, b)"), "got: {out}");
    }

    #[test]
    fn leaves_new_number_alone() {
        let src = "function f(x) { return new Number(x); }";
        let out = apply_to_source(&NumberCallToUnaryPlusGuarded, src).expect("parse");
        assert!(out.contains("new Number"), "got: {out}");
    }

    #[test]
    fn leaves_member_number_alone() {
        let src = "function f(x) { return globalThis.Number(x); }";
        let out = apply_to_source(&NumberCallToUnaryPlusGuarded, src).expect("parse");
        assert!(out.contains("globalThis.Number"), "got: {out}");
    }

    #[test]
    fn leaves_spread_argument_alone() {
        let src = "function f(args) { return Number(...args); }";
        let out = apply_to_source(&NumberCallToUnaryPlusGuarded, src).expect("parse");
        assert!(out.contains("Number(...args)"), "got: {out}");
    }

    #[test]
    fn idempotent_when_already_unary_plus() {
        let src = "function f(x) { return +x; }";
        let first = apply_to_source(&NumberCallToUnaryPlusGuarded, src).expect("first");
        let second = apply_to_source(&NumberCallToUnaryPlusGuarded, &first).expect("second");
        assert_eq!(first, second);
    }
}
