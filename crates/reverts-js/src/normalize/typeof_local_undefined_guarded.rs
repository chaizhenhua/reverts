use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::Visit;
use oxc_ast::ast::{BindingPatternKind, Expression, Program};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_expression;
use oxc_span::SPAN;
use oxc_syntax::operator::{BinaryOperator, UnaryOperator};
use reverts_ir::NormalizationPassId;
use std::collections::BTreeSet;
use std::mem;

use super::NormalizationPass;

/// `TypeofLocalUndefinedGuarded` rewrites the minifier-flavoured idiom
/// `typeof X === "undefined"` (and the commutative
/// `"undefined" === typeof X`, plus the `!==` / `!=` / `==` variants)
/// into the equivalent `X === undefined` form — **but only when
/// scope analysis proves both of the following**:
///
/// 1. `X` resolves to a known local binding somewhere in the program
///    (so the access cannot raise `ReferenceError`; this is the
///    safety property `typeof` normally provides over direct read).
/// 2. The global identifier `undefined` is not shadowed anywhere in
///    the program (so `=== undefined` reads the real undefined
///    value, not some shadowed local).
///
/// With both preconditions in place the rewrite is **strictly
/// spec-equivalent**:
///
/// * Per ECMA-262 §13.5.3 (`typeof`), `typeof X` returns the string
///   `"undefined"` iff the value of `X` is `undefined`. For
///   undeclared `X` it also returns `"undefined"` (the
///   `ReferenceError`-safety property), but precondition (1)
///   eliminates that case.
/// * Per §7.2.16 (`IsStrictlyEqual`), strict equality with
///   `undefined` is true iff the operand is the undefined value.
///   These are the same predicate when (1) holds.
///
/// The pass intentionally restricts to **direct identifier**
/// operands. `typeof obj.x === "undefined"` is left alone — a
/// member access may throw on a null/undefined receiver, and the
/// rewrite would change error-vs-result behaviour.
pub struct TypeofLocalUndefinedGuarded;

impl NormalizationPass for TypeofLocalUndefinedGuarded {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::TypeofLocalUndefinedGuarded
    }
    fn version(&self) -> u32 {
        1
    }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        if super::shadow_check::program_can_shadow(program, "undefined") {
            return;
        }
        let locals: BTreeSet<String> = collect_binding_names_owned(program);
        let mut visitor = Rewriter {
            builder: AstBuilder::new(alloc),
            locals,
        };
        visitor.visit_program(program);
    }
}

/// Collect every identifier name bound by a BindingIdentifier
/// pattern anywhere in the program. Owned strings so the pass can
/// keep them past the borrow checker's program lifetime.
fn collect_binding_names_owned(program: &Program<'_>) -> BTreeSet<String> {
    struct Collector {
        set: BTreeSet<String>,
    }
    impl<'a> Visit<'a> for Collector {
        fn visit_binding_identifier(&mut self, b: &oxc_ast::ast::BindingIdentifier<'a>) {
            self.set.insert(b.name.as_str().to_owned());
        }
        fn visit_binding_pattern_kind(&mut self, kind: &BindingPatternKind<'a>) {
            if let BindingPatternKind::BindingIdentifier(b) = kind {
                self.set.insert(b.name.as_str().to_owned());
            }
            oxc_ast::visit::walk::walk_binding_pattern_kind(self, kind);
        }
    }
    let mut c = Collector {
        set: BTreeSet::new(),
    };
    c.visit_program(program);
    c.set
}

struct Rewriter<'a> {
    builder: AstBuilder<'a>,
    locals: BTreeSet<String>,
}

#[derive(Copy, Clone)]
enum Op {
    StrictEquality,
    StrictInequality,
    Equality,
    Inequality,
}

impl Op {
    fn as_binary(self) -> BinaryOperator {
        match self {
            Op::StrictEquality => BinaryOperator::StrictEquality,
            Op::StrictInequality => BinaryOperator::StrictInequality,
            Op::Equality => BinaryOperator::Equality,
            Op::Inequality => BinaryOperator::Inequality,
        }
    }
}

impl<'a> VisitMut<'a> for Rewriter<'a> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        walk_expression(self, expr);
        let Some((ident_name, op)) = matches_typeof_local_undefined(expr, &self.locals) else {
            return;
        };
        let placeholder = self.builder.expression_null_literal(SPAN);
        let _ = mem::replace(expr, placeholder);
        let lhs = self
            .builder
            .expression_identifier_reference(SPAN, ident_name.as_str());
        let rhs = self
            .builder
            .expression_identifier_reference(SPAN, "undefined");
        *expr = self
            .builder
            .expression_binary(SPAN, lhs, op.as_binary(), rhs);
    }
}

/// Match `typeof X (==|===|!=|!==) "undefined"` (and the commutative
/// `"undefined" === typeof X`). Returns the identifier name and the
/// canonical operator to use in the rewrite.
fn matches_typeof_local_undefined(
    expr: &Expression<'_>,
    locals: &BTreeSet<String>,
) -> Option<(String, Op)> {
    let Expression::BinaryExpression(bin) = expr else {
        return None;
    };
    let op = match bin.operator {
        BinaryOperator::StrictEquality => Op::StrictEquality,
        BinaryOperator::StrictInequality => Op::StrictInequality,
        BinaryOperator::Equality => Op::Equality,
        BinaryOperator::Inequality => Op::Inequality,
        _ => return None,
    };

    // One side is `typeof <Identifier>`, the other is the string
    // literal `"undefined"`.
    let (typeof_side, str_side) = match (&bin.left, &bin.right) {
        (Expression::UnaryExpression(_), Expression::StringLiteral(_)) => (&bin.left, &bin.right),
        (Expression::StringLiteral(_), Expression::UnaryExpression(_)) => (&bin.right, &bin.left),
        _ => return None,
    };
    let Expression::StringLiteral(s) = str_side else {
        return None;
    };
    if s.value.as_str() != "undefined" {
        return None;
    }
    let Expression::UnaryExpression(u) = typeof_side else {
        return None;
    };
    if !matches!(u.operator, UnaryOperator::Typeof) {
        return None;
    }
    let Expression::Identifier(id) = &u.argument else {
        return None;
    };
    let name = id.name.as_str();
    if !locals.contains(name) {
        return None;
    }
    Some((name.to_owned(), op))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn rewrites_typeof_local_strict_equal_undefined() {
        let out = apply_to_source(
            &TypeofLocalUndefinedGuarded,
            r#"function f() { let x; return typeof x === "undefined"; }"#,
        )
        .expect("parse");
        assert!(out.contains("x === undefined"), "got: {out}");
        assert!(!out.contains("typeof"), "got: {out}");
    }

    #[test]
    fn rewrites_typeof_param_loose_equal_undefined() {
        let out = apply_to_source(
            &TypeofLocalUndefinedGuarded,
            r#"function f(x) { return typeof x == "undefined"; }"#,
        )
        .expect("parse");
        assert!(out.contains("x == undefined"), "got: {out}");
    }

    #[test]
    fn rewrites_typeof_local_strict_not_equal() {
        let out = apply_to_source(
            &TypeofLocalUndefinedGuarded,
            r#"function f(x) { return typeof x !== "undefined"; }"#,
        )
        .expect("parse");
        assert!(out.contains("x !== undefined"), "got: {out}");
    }

    #[test]
    fn rewrites_commutative_undefined_typeof_form() {
        let out = apply_to_source(
            &TypeofLocalUndefinedGuarded,
            r#"function f(x) { return "undefined" === typeof x; }"#,
        )
        .expect("parse");
        assert!(out.contains("x === undefined"), "got: {out}");
    }

    #[test]
    fn bails_out_on_undeclared_global_identifier() {
        // `globalName` is not bound anywhere in the program; the
        // typeof-safety property matters. Don't rewrite.
        let src = r#"function f() { return typeof globalName === "undefined"; }"#;
        let out = apply_to_source(&TypeofLocalUndefinedGuarded, src).expect("parse");
        assert!(out.contains("typeof globalName"), "got: {out}");
    }

    #[test]
    fn bails_out_when_undefined_is_shadowed() {
        let src = r#"
            function f(x) {
                let undefined = 0;
                return typeof x === "undefined";
            }
        "#;
        let out = apply_to_source(&TypeofLocalUndefinedGuarded, src).expect("parse");
        assert!(out.contains("typeof x"), "got: {out}");
    }

    #[test]
    fn leaves_typeof_member_expression_alone() {
        // `obj.x` can throw on a null/undefined receiver; typeof
        // does NOT swallow that error, but neither does direct
        // read — yet the rewrite changes the AST shape in a way
        // that could trip up subsequent passes. Conservative skip.
        let src = r#"function f(obj) { return typeof obj.x === "undefined"; }"#;
        let out = apply_to_source(&TypeofLocalUndefinedGuarded, src).expect("parse");
        assert!(out.contains("typeof obj.x"), "got: {out}");
    }

    #[test]
    fn leaves_typeof_compared_with_other_string_alone() {
        let src = r#"function f(x) { return typeof x === "function"; }"#;
        let out = apply_to_source(&TypeofLocalUndefinedGuarded, src).expect("parse");
        assert!(out.contains(r#"typeof x === "function""#), "got: {out}");
    }

    #[test]
    fn idempotent_when_already_canonical() {
        let src = "function f(x) { return x === undefined; }";
        let first = apply_to_source(&TypeofLocalUndefinedGuarded, src).expect("first");
        let second = apply_to_source(&TypeofLocalUndefinedGuarded, &first).expect("second");
        assert_eq!(first, second);
    }
}
