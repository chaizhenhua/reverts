use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::Visit;
use oxc_ast::ast::{Expression, Program};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_expression;
use oxc_span::SPAN;
use oxc_syntax::operator::UnaryOperator;
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `VoidZeroToUndefinedGuarded` rewrites the minifier idiom `void 0`
/// (a 6-byte boolean-typed `undefined`-producer) back into the
/// canonical identifier `undefined` — **but only when a static scope
/// check proves that `undefined` is not shadowed anywhere in the
/// program** (no local binding named `undefined`, and no `with`
/// statement that could dynamically introduce one).
///
/// Per ECMA-262 §13.5.2.1 (`UnaryExpression: void UnaryExpression`),
/// `void X` evaluates `X` then returns the `undefined` value. When
/// `X` is the numeric literal `0` no side effect runs and the result
/// is literally `undefined`. Per §10.2 (`OrdinaryFunctionCreate`)
/// and §10.2.1.4 (`OrdinaryCallEvaluateBody`), reading the global
/// identifier `undefined` reads the *property* `undefined` of the
/// global object, which is non-writable since ES5 (§19.1.1.3).
///
/// So `void 0` and `undefined` produce the same value **unless**
/// some lexical scope introduces a binding `undefined` that shadows
/// the global. The pass conservatively bails out whenever it sees
/// any binding of that name anywhere in the file. With that
/// precondition the rewrite is **strictly spec-equivalent** —
/// shadowing concerns are eliminated by the static check, not
/// papered over.
///
/// Why bother? Cascade matching against un-minified package source
/// frequently has `return undefined;` or `=== undefined` literally,
/// while bundlers emit `return void 0;` or `=== void 0`. The two
/// forms hash differently across multiple axes (`ast`, `cfg`,
/// `return_pattern`, `literal_shape`). This pass closes the gap when
/// scope analysis can prove the rewrite safe.
pub struct VoidZeroToUndefinedGuarded;

impl NormalizationPass for VoidZeroToUndefinedGuarded {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::VoidZeroToUndefinedGuarded
    }
    fn version(&self) -> u32 {
        1
    }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        if program_can_shadow_undefined(program) {
            return;
        }
        let mut visitor = Rewriter {
            builder: AstBuilder::new(alloc),
        };
        visitor.visit_program(program);
    }
}

/// Returns true if any lexical/var binding in the program is named
/// `undefined`, OR any `with` statement is present (which can
/// dynamically introduce one). When either is the case the rewrite is
/// **not** safe and the pass must be a no-op.
fn program_can_shadow_undefined(program: &Program<'_>) -> bool {
    struct Checker {
        found: bool,
    }
    impl<'a> Visit<'a> for Checker {
        fn visit_binding_identifier(&mut self, b: &oxc_ast::ast::BindingIdentifier<'a>) {
            if b.name.as_str() == "undefined" {
                self.found = true;
            }
        }
        fn visit_with_statement(&mut self, _: &oxc_ast::ast::WithStatement<'a>) {
            // `with (o) { ... }` makes `o.undefined` an in-scope read,
            // so any `undefined` inside the body would resolve to a
            // potentially-non-undefined value.
            self.found = true;
        }
    }
    let mut c = Checker { found: false };
    c.visit_program(program);
    c.found
}

struct Rewriter<'a> {
    builder: AstBuilder<'a>,
}

impl<'a> VisitMut<'a> for Rewriter<'a> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        walk_expression(self, expr);
        if !matches_void_zero(expr) {
            return;
        }
        let placeholder = self.builder.expression_null_literal(SPAN);
        let _ = mem::replace(expr, placeholder);
        *expr = self
            .builder
            .expression_identifier_reference(SPAN, "undefined");
    }
}

fn matches_void_zero(expr: &Expression<'_>) -> bool {
    let Expression::UnaryExpression(u) = expr else {
        return false;
    };
    if !matches!(u.operator, UnaryOperator::Void) {
        return false;
    }
    matches!(&u.argument, Expression::NumericLiteral(n) if n.value == 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn rewrites_void_zero_when_no_shadow() {
        let out = apply_to_source(
            &VoidZeroToUndefinedGuarded,
            "function f(x) { return x === void 0; }",
        )
        .expect("parse");
        assert!(out.contains("undefined"), "got: {out}");
        assert!(!out.contains("void 0"), "got: {out}");
    }

    #[test]
    fn rewrites_void_zero_at_return_position() {
        let out = apply_to_source(
            &VoidZeroToUndefinedGuarded,
            "function f() { return void 0; }",
        )
        .expect("parse");
        assert!(out.contains("return undefined"), "got: {out}");
    }

    #[test]
    fn bails_out_when_local_binding_named_undefined_exists() {
        // `let undefined = 1;` shadows the global. The pass must NOT
        // rewrite `void 0` to `undefined` because the rewrite would
        // observably produce `1` instead of the real undefined value.
        let src = r#"
            function bad() {
                let undefined = 1;
                return void 0;
            }
        "#;
        let out = apply_to_source(&VoidZeroToUndefinedGuarded, src).expect("parse");
        assert!(out.contains("void 0"), "got: {out}");
        assert!(!out.contains("return undefined"), "got: {out}");
    }

    #[test]
    fn bails_out_on_param_named_undefined() {
        let src = "function bad(undefined) { return void 0; }";
        let out = apply_to_source(&VoidZeroToUndefinedGuarded, src).expect("parse");
        assert!(out.contains("void 0"), "got: {out}");
    }

    #[test]
    fn bails_out_on_with_statement() {
        // The `with` statement dynamically introduces a scope; we
        // can't statically prove `undefined` isn't shadowed.
        let src = r#"
            function lax(o) {
                with (o) { return void 0; }
            }
        "#;
        let out = apply_to_source(&VoidZeroToUndefinedGuarded, src).expect("parse");
        assert!(out.contains("void 0"), "got: {out}");
    }

    #[test]
    fn leaves_void_one_alone() {
        // Only `void 0` is rewritten — `void 1` (rare) is conservatively kept.
        let src = "function f() { return void 1; }";
        let out = apply_to_source(&VoidZeroToUndefinedGuarded, src).expect("parse");
        assert!(out.contains("void 1"), "got: {out}");
    }

    #[test]
    fn leaves_void_with_side_effect_alone() {
        // `void something()` runs the call for side effects. The pass
        // must not collapse it to `undefined`.
        let src = "function f() { return void log(); }";
        let out = apply_to_source(&VoidZeroToUndefinedGuarded, src).expect("parse");
        assert!(out.contains("void log()"), "got: {out}");
    }

    #[test]
    fn idempotent_when_already_undefined() {
        let src = "function f(x) { return x === undefined; }";
        let first = apply_to_source(&VoidZeroToUndefinedGuarded, src).expect("first");
        let second = apply_to_source(&VoidZeroToUndefinedGuarded, &first).expect("second");
        assert_eq!(first, second);
    }
}
