use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Expression, Program};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_expression;
use oxc_span::SPAN;
use oxc_syntax::operator::UnaryOperator;
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `BooleanUndefinedCanonicalised` expands the universal minifier
/// shortenings back to their human-readable forms so fingerprints
/// computed on minified output match fingerprints computed on the
/// original source. The single direction of rewriting is "from
/// minified shortcut → expanded form":
///
/// * `!0`     → `true`
/// * `!1`     → `false`
/// * `void X` → `undefined` (for literal X with no side effects;
///   minifiers only emit `void 0` for this purpose)
/// * `Foo(...)` → `new Foo(...)` when `Foo` is a built-in constructor
///   (`Error`, `TypeError`, `RangeError`, …) whose `new`-prefixed form
///   is spec-equivalent and is what un-minified source typically uses.
///
/// Each rewrite is local and lossless for fingerprinting purposes:
/// the rewritten AST evaluates to the same value as the original at
/// runtime in every context where these minifier patterns occur.
pub struct BooleanUndefinedCanonicalised;

impl NormalizationPass for BooleanUndefinedCanonicalised {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::BooleanUndefinedCanonicalised
    }
    fn version(&self) -> u32 {
        1
    }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut visitor = Canonicaliser {
            builder: AstBuilder::new(alloc),
        };
        visitor.visit_program(program);
    }
}

struct Canonicaliser<'a> {
    builder: AstBuilder<'a>,
}

impl<'a> VisitMut<'a> for Canonicaliser<'a> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        // Recurse first so nested patterns inside larger expressions
        // are rewritten bottom-up.
        walk_expression(self, expr);
        if let Some(replacement) = canonicalise(&self.builder, expr) {
            *expr = replacement;
            return;
        }
        // Promote `Foo(args)` → `new Foo(args)` for built-in
        // constructors. We need to take ownership of the arguments,
        // so swap-and-replace via a Null sentinel.
        if let Expression::CallExpression(c) = expr
            && let Expression::Identifier(callee) = &c.callee
            && is_new_optional_builtin(callee.name.as_str())
        {
            // Take ownership of the CallExpression.
            let placeholder = self.builder.expression_null_literal(SPAN);
            let owned = mem::replace(expr, placeholder);
            let Expression::CallExpression(call_box) = owned else {
                unreachable!()
            };
            let call = call_box.unbox();
            *expr = self.builder.expression_new(
                SPAN,
                call.callee,
                call.arguments,
                Option::<oxc_allocator::Box<oxc_ast::ast::TSTypeParameterInstantiation>>::None,
            );
        }
    }
}

fn is_new_optional_builtin(name: &str) -> bool {
    matches!(
        name,
        "Error"
            | "TypeError"
            | "RangeError"
            | "ReferenceError"
            | "SyntaxError"
            | "URIError"
            | "EvalError"
            | "AggregateError"
    )
}

fn canonicalise<'a>(builder: &AstBuilder<'a>, expr: &Expression<'a>) -> Option<Expression<'a>> {
    let Expression::UnaryExpression(u) = expr else {
        return None;
    };
    match u.operator {
        UnaryOperator::LogicalNot => {
            // `!0` → `true`, `!1` → `false`. Any other numeric `!N` is
            // either ambiguous or a real boolean negation; leave it.
            if let Expression::NumericLiteral(n) = &u.argument {
                if n.value == 0.0 {
                    return Some(builder.expression_boolean_literal(SPAN, true));
                }
                if n.value == 1.0 {
                    return Some(builder.expression_boolean_literal(SPAN, false));
                }
            }
            None
        }
        UnaryOperator::Void => {
            // `void <any>` evaluates to `undefined`. Minifiers emit this
            // (almost always `void 0`) as a shorter substitute for the
            // identifier `undefined`. Replacing with an Identifier is
            // safe because the operand is invariably side-effect-free
            // when emitted by a minifier — but to be conservative we
            // only rewrite when the operand is a literal (no side
            // effects possible).
            if matches!(
                &u.argument,
                Expression::NumericLiteral(_)
                    | Expression::StringLiteral(_)
                    | Expression::BooleanLiteral(_)
                    | Expression::NullLiteral(_)
            ) {
                return Some(builder.expression_identifier_reference(SPAN, "undefined"));
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn rewrites_bang_zero_to_true() {
        let out = apply_to_source(
            &BooleanUndefinedCanonicalised,
            "function f() { return !0; }",
        )
        .expect("parse");
        assert!(out.contains("return true"), "got: {out}");
        assert!(!out.contains("!0"), "got: {out}");
    }

    #[test]
    fn rewrites_bang_one_to_false() {
        let out = apply_to_source(
            &BooleanUndefinedCanonicalised,
            "function f() { return !1; }",
        )
        .expect("parse");
        assert!(out.contains("return false"), "got: {out}");
    }

    #[test]
    fn rewrites_void_zero_to_undefined() {
        let out = apply_to_source(
            &BooleanUndefinedCanonicalised,
            "function f(x) { return x === void 0; }",
        )
        .expect("parse");
        assert!(out.contains("undefined"), "got: {out}");
        assert!(!out.contains("void 0"), "got: {out}");
    }

    #[test]
    fn rewrites_nested_bang_zero_in_object_literal() {
        let out = apply_to_source(
            &BooleanUndefinedCanonicalised,
            "function f() { return {a:!0, b:!1}; }",
        )
        .expect("parse");
        assert!(out.contains("a: true"), "got: {out}");
        assert!(out.contains("b: false"), "got: {out}");
    }

    #[test]
    fn leaves_real_logical_negation_alone() {
        let out = apply_to_source(
            &BooleanUndefinedCanonicalised,
            "function f(x) { return !x; }",
        )
        .expect("parse");
        assert!(out.contains("!x"), "got: {out}");
    }

    #[test]
    fn leaves_bang_two_alone() {
        // `!2` is not a minifier shortening; leave it.
        let out = apply_to_source(
            &BooleanUndefinedCanonicalised,
            "function f() { return !2; }",
        )
        .expect("parse");
        assert!(out.contains("!2"), "got: {out}");
    }

    #[test]
    fn rewrites_typeerror_to_new_typeerror() {
        let out = apply_to_source(
            &BooleanUndefinedCanonicalised,
            "function f() { throw TypeError('bad'); }",
        )
        .expect("parse");
        assert!(out.contains("new TypeError"), "got: {out}");
    }

    #[test]
    fn rewrites_rangeerror_to_new_rangeerror() {
        let out = apply_to_source(
            &BooleanUndefinedCanonicalised,
            "function f() { throw RangeError('bad'); }",
        )
        .expect("parse");
        assert!(out.contains("new RangeError"), "got: {out}");
    }

    #[test]
    fn leaves_user_class_call_alone() {
        let out = apply_to_source(
            &BooleanUndefinedCanonicalised,
            "function f() { return myFactory(1); }",
        )
        .expect("parse");
        assert!(!out.contains("new myFactory"), "got: {out}");
    }

    #[test]
    fn idempotent_when_no_match() {
        let src = "function add(a, b) { return a + b; }";
        let first = apply_to_source(&BooleanUndefinedCanonicalised, src).expect("first");
        let second = apply_to_source(&BooleanUndefinedCanonicalised, &first).expect("second");
        assert_eq!(first, second);
    }
}
