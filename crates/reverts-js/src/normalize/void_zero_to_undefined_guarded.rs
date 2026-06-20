use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::Visit;
use oxc_ast::ast::{
    ArrowFunctionExpression, BindingIdentifier, Expression, Function, Program, WithStatement,
};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::{
    walk_arrow_function_expression, walk_expression, walk_function, walk_with_statement,
};
use oxc_span::SPAN;
use oxc_syntax::operator::UnaryOperator;
use oxc_syntax::scope::ScopeFlags;
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `VoidZeroToUndefinedGuarded` rewrites the minifier idiom `void
/// <number>` (typically `void 0`, occasionally `void 1` or other
/// constants) back into the canonical identifier `undefined` — **but
/// only when a static scope check proves that `undefined` is not
/// shadowed anywhere in the program** (no local binding named
/// `undefined`, and no `with` statement that could dynamically
/// introduce one).
///
/// Per ECMA-262 §13.5.2.1 (`UnaryExpression: void UnaryExpression`),
/// `void X` evaluates `X` then returns the `undefined` value. When
/// `X` is a numeric literal no side effect runs and the result is
/// literally `undefined`. Per §10.2 (`OrdinaryFunctionCreate`) and
/// §10.2.1.4 (`OrdinaryCallEvaluateBody`), reading the global
/// identifier `undefined` reads the *property* `undefined` of the
/// global object, which is non-writable since ES5 (§19.1.1.3).
///
/// So `void <num>` and `undefined` produce the same value **unless**
/// some lexical scope introduces a binding `undefined` that shadows
/// the global. The pass tracks shadowing at **function granularity**:
/// it rewrites a `void <num>` site only when neither the enclosing
/// function (its parameters or any binding it introduces) nor any
/// ancestor function/`with` scope binds `undefined`. A module-scope
/// binding of `undefined` taints the whole file. Block-scoped bindings
/// (`let undefined` inside a nested block) conservatively taint their
/// whole containing function — never the reverse — so the rewrite is
/// always **strictly spec-equivalent**: a shadowed site is never
/// rewritten, while sites in the (overwhelmingly common) unshadowed
/// functions of a file that merely *contains* a stray `undefined`
/// binding elsewhere are recovered (the previous whole-file bail left
/// them all as `void 0`).
///
/// Why bother? Cascade matching against un-minified package source
/// frequently has `return undefined;` or `=== undefined` literally,
/// while bundlers emit `return void 0;` or `=== void 0`. The two
/// forms hash differently across multiple axes (`ast`, `cfg`,
/// `return_pattern`, `literal_shape`). This pass closes the gap when
/// scope analysis can prove the rewrite safe.
///
/// Restricted to `void <NumericLiteral>`: numeric literals are
/// guaranteed side-effect-free per §6.2 (Primitive Values). Other
/// `void <expr>` forms (calls, member access, etc.) may have side
/// effects and are conservatively left alone.
pub struct VoidZeroToUndefinedGuarded;

impl NormalizationPass for VoidZeroToUndefinedGuarded {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::VoidZeroToUndefinedGuarded
    }
    fn version(&self) -> u32 {
        2
    }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        // A `undefined` binding at module scope shadows the global for
        // the entire file — nothing is safe to rewrite.
        if scope_binds_undefined(program.body.iter()) {
            return;
        }
        let mut visitor = Rewriter {
            builder: AstBuilder::new(alloc),
            tainted_depth: 0,
        };
        visitor.visit_program(program);
    }
}

struct Rewriter<'a> {
    builder: AstBuilder<'a>,
    /// Number of enclosing scopes that shadow `undefined` (functions
    /// binding it, or `with` statements). A `void <num>` site is only
    /// rewritten when this is zero.
    tainted_depth: u32,
}

impl<'a> VisitMut<'a> for Rewriter<'a> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        walk_expression(self, expr);
        if self.tainted_depth != 0 || !matches_void_zero(expr) {
            return;
        }
        let placeholder = self.builder.expression_null_literal(SPAN);
        let _ = mem::replace(expr, placeholder);
        *expr = self
            .builder
            .expression_identifier_reference(SPAN, "undefined");
    }

    fn visit_function(&mut self, func: &mut Function<'a>, flags: ScopeFlags) {
        let shadows = function_binds_undefined(func);
        if shadows {
            self.tainted_depth += 1;
        }
        walk_function(self, func, flags);
        if shadows {
            self.tainted_depth -= 1;
        }
    }

    fn visit_arrow_function_expression(&mut self, arrow: &mut ArrowFunctionExpression<'a>) {
        let shadows = arrow_binds_undefined(arrow);
        if shadows {
            self.tainted_depth += 1;
        }
        walk_arrow_function_expression(self, arrow);
        if shadows {
            self.tainted_depth -= 1;
        }
    }

    fn visit_with_statement(&mut self, stmt: &mut WithStatement<'a>) {
        // `with (o)` can dynamically introduce an `undefined` binding
        // for the duration of its body — taint just that body.
        self.tainted_depth += 1;
        walk_with_statement(self, stmt);
        self.tainted_depth -= 1;
    }
}

/// Immutable scan that reports whether a single scope (a set of
/// sibling statements, plus optionally formal parameters) directly
/// binds `undefined`. It descends into nested blocks (`if`, `for`,
/// `try`/`catch`, plain blocks) because their lexical bindings
/// conservatively belong to the containing function's taint, but it
/// does **not** descend into nested functions/arrows — those open a
/// new scope handled by their own taint frame. A nested function
/// *declaration* still contributes its own name as a binding in the
/// scope being scanned.
struct BindsUndefined {
    found: bool,
}

impl<'a> Visit<'a> for BindsUndefined {
    fn visit_binding_identifier(&mut self, ident: &BindingIdentifier<'a>) {
        if ident.name == "undefined" {
            self.found = true;
        }
    }

    fn visit_function(&mut self, func: &Function<'a>, _flags: ScopeFlags) {
        // Record the function's own name (a binding in the enclosing
        // scope) but do not descend into its params/body (inner scope).
        if let Some(id) = &func.id {
            if id.name == "undefined" {
                self.found = true;
            }
        }
    }

    fn visit_arrow_function_expression(&mut self, _arrow: &ArrowFunctionExpression<'a>) {
        // Arrows are anonymous and open an inner scope — skip entirely.
    }
}

fn scope_binds_undefined<'a, 'b>(
    statements: impl Iterator<Item = &'b oxc_ast::ast::Statement<'a>>,
) -> bool
where
    'a: 'b,
{
    let mut scan = BindsUndefined { found: false };
    for stmt in statements {
        if scan.found {
            break;
        }
        scan.visit_statement(stmt);
    }
    scan.found
}

fn function_binds_undefined(func: &Function<'_>) -> bool {
    let mut scan = BindsUndefined { found: false };
    scan.visit_formal_parameters(&func.params);
    if !scan.found {
        if let Some(body) = &func.body {
            for stmt in &body.statements {
                if scan.found {
                    break;
                }
                scan.visit_statement(stmt);
            }
        }
    }
    scan.found
}

fn arrow_binds_undefined(arrow: &ArrowFunctionExpression<'_>) -> bool {
    let mut scan = BindsUndefined { found: false };
    scan.visit_formal_parameters(&arrow.params);
    if !scan.found {
        for stmt in &arrow.body.statements {
            if scan.found {
                break;
            }
            scan.visit_statement(stmt);
        }
    }
    scan.found
}

fn matches_void_zero(expr: &Expression<'_>) -> bool {
    let Expression::UnaryExpression(u) = expr else {
        return false;
    };
    if !matches!(u.operator, UnaryOperator::Void) {
        return false;
    }
    matches!(&u.argument, Expression::NumericLiteral(_))
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
    fn rewrites_void_one_too() {
        // `void 1` is also a side-effect-free undefined-producer (the
        // inner literal is discarded). With `undefined` not shadowed
        // the rewrite is safe.
        let src = "function f() { return void 1; }";
        let out = apply_to_source(&VoidZeroToUndefinedGuarded, src).expect("parse");
        assert!(out.contains("return undefined"), "got: {out}");
        assert!(!out.contains("void 1"), "got: {out}");
    }

    #[test]
    fn rewrites_void_any_numeric_literal() {
        // Other numeric literals (large, decimal, etc.) are still
        // side-effect-free.
        let src = "function f() { return void 42; }";
        let out = apply_to_source(&VoidZeroToUndefinedGuarded, src).expect("parse");
        assert!(out.contains("return undefined"), "got: {out}");
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

    #[test]
    fn rewrites_unshadowed_function_even_when_a_sibling_shadows() {
        // The old whole-file guard bailed on the entire file because
        // `shadowed` binds `undefined`. Scope-precise tracking rewrites
        // `clean`'s site while leaving `shadowed`'s alone.
        let src = r#"
            function shadowed(undefined) { return void 0; }
            function clean(x) { return x === void 0; }
        "#;
        let out = apply_to_source(&VoidZeroToUndefinedGuarded, src).expect("parse");
        assert!(
            out.contains("x === undefined"),
            "clean not rewritten: {out}"
        );
        assert!(
            out.contains("function shadowed(undefined)") && out.contains("return void 0"),
            "shadowed wrongly rewritten: {out}"
        );
    }

    #[test]
    fn taints_nested_function_when_an_ancestor_shadows() {
        // `inner`'s `void 0` is shadowed by `outer`'s param via the
        // scope chain, so it must be kept even though `inner` itself
        // does not bind `undefined`.
        let src = r#"
            function outer(undefined) {
                function inner() { return void 0; }
                return inner;
            }
        "#;
        let out = apply_to_source(&VoidZeroToUndefinedGuarded, src).expect("parse");
        assert!(
            out.contains("return void 0"),
            "ancestor shadow ignored: {out}"
        );
        assert!(!out.contains("return undefined"), "got: {out}");
    }

    #[test]
    fn rewrites_sibling_of_nested_shadowing_function() {
        // `outer` does not bind `undefined`; only the nested `inner`
        // does (via its own param). `outer`'s own `void 0` is NOT
        // shadowed and must be rewritten.
        let src = r#"
            function outer() {
                function inner(undefined) { return void 0; }
                return inner === void 0;
            }
        "#;
        let out = apply_to_source(&VoidZeroToUndefinedGuarded, src).expect("parse");
        assert!(
            out.contains("inner === undefined"),
            "outer site not rewritten: {out}"
        );
        assert!(
            out.contains("function inner(undefined)") && out.contains("return void 0"),
            "inner wrongly rewritten: {out}"
        );
    }

    #[test]
    fn module_scope_binding_bails_whole_file() {
        // A `undefined` binding at module scope shadows the global for
        // every nested scope, so nothing is rewritten.
        let src = r#"
            let undefined = 1;
            function f() { return void 0; }
        "#;
        let out = apply_to_source(&VoidZeroToUndefinedGuarded, src).expect("parse");
        assert!(
            out.contains("return void 0"),
            "module shadow ignored: {out}"
        );
    }

    #[test]
    fn rewrites_outside_with_but_keeps_inside() {
        // `with` only shadows its own block; a `void 0` before it in
        // the same function is still safe.
        let src = r#"
            function f(o) {
                let a = void 0;
                with (o) { return void 0; }
            }
        "#;
        let out = apply_to_source(&VoidZeroToUndefinedGuarded, src).expect("parse");
        assert!(
            out.contains("let a = undefined"),
            "pre-with not rewritten: {out}"
        );
        assert!(out.contains("with(o)"), "got: {out}");
        assert!(
            out.contains("return void 0"),
            "with-body wrongly rewritten: {out}"
        );
    }
}
