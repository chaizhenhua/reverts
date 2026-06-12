use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Expression, Program};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_expression;
use oxc_span::SPAN;
use oxc_syntax::operator::UnaryOperator;
use reverts_ir::NormalizationPassId;

use super::NormalizationPass;

/// `BooleanUndefinedCanonicalised` rewrites the two universal
/// minifier-only boolean shortenings back to literal booleans:
///
/// * `!0` → `true`
/// * `!1` → `false`
///
/// Both rewrites are **strictly spec-equivalent** in every context:
/// the only operand is a `NumericLiteral` with no identifier
/// resolution, and `!0` / `!1` always evaluate to the literal `true`
/// / `false` per ECMA-262 §13.5.7 (UnaryExpression `!`) and §7.1.2
/// (ToBoolean). There is no shadowing, no runtime dispatch, and no
/// way for the rewrite to change observable behaviour.
///
/// Earlier revisions also rewrote `void <literal> → undefined` and
/// `TypeError(args) → new TypeError(args)` for built-in error
/// constructors. Both rewrites can change behaviour when the global
/// `undefined` is shadowed (legal in non-strict mode) or when a
/// built-in error constructor has been shadowed with an arrow
/// function (which throws `TypeError` when invoked with `new`). The
/// project's policy is "never transform what is not fully
/// equivalent", so those rewrites were removed.
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
        walk_expression(self, expr);
        if let Some(replacement) = canonicalise(&self.builder, expr) {
            *expr = replacement;
        }
    }
}

fn canonicalise<'a>(builder: &AstBuilder<'a>, expr: &Expression<'a>) -> Option<Expression<'a>> {
    let Expression::UnaryExpression(u) = expr else {
        return None;
    };
    if !matches!(u.operator, UnaryOperator::LogicalNot) {
        return None;
    }
    let Expression::NumericLiteral(n) = &u.argument else {
        return None;
    };
    if n.value == 0.0 {
        Some(builder.expression_boolean_literal(SPAN, true))
    } else if n.value == 1.0 {
        Some(builder.expression_boolean_literal(SPAN, false))
    } else {
        None
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
        // `!x` is a real boolean negation against an identifier whose
        // value is unknown — must not be touched.
        let out = apply_to_source(
            &BooleanUndefinedCanonicalised,
            "function f(x) { return !x; }",
        )
        .expect("parse");
        assert!(out.contains("!x"), "got: {out}");
    }

    #[test]
    fn leaves_bang_two_alone() {
        let out = apply_to_source(
            &BooleanUndefinedCanonicalised,
            "function f() { return !2; }",
        )
        .expect("parse");
        assert!(out.contains("!2"), "got: {out}");
    }

    #[test]
    fn leaves_void_zero_alone_in_modern_strict_pass() {
        // `void 0` ↔ `undefined` is NOT fully equivalent because the
        // identifier `undefined` is shadowable in non-strict mode.
        // The pass no longer touches it.
        let out = apply_to_source(
            &BooleanUndefinedCanonicalised,
            "function f(x) { return x === void 0; }",
        )
        .expect("parse");
        assert!(out.contains("void 0"), "got: {out}");
    }

    #[test]
    fn leaves_call_to_builtin_alone() {
        // `TypeError(msg)` ↔ `new TypeError(msg)` is NOT fully
        // equivalent if `TypeError` is shadowed by an arrow function
        // (`new arrow()` throws). The pass no longer touches it.
        let out = apply_to_source(
            &BooleanUndefinedCanonicalised,
            "function f() { throw TypeError('bad'); }",
        )
        .expect("parse");
        assert!(out.contains("throw TypeError"), "got: {out}");
        assert!(!out.contains("new TypeError"), "got: {out}");
    }

    #[test]
    fn idempotent_when_no_match() {
        let src = "function add(a, b) { return a + b; }";
        let first = apply_to_source(&BooleanUndefinedCanonicalised, src).expect("first");
        let second = apply_to_source(&BooleanUndefinedCanonicalised, &first).expect("second");
        assert_eq!(first, second);
    }
}
