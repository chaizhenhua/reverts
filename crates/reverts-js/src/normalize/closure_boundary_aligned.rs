use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{ArrowFunctionExpression, Expression, FunctionBody, Program, Statement};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_function_body;
use oxc_span::SPAN;
use reverts_ir::NormalizationPassId;

use super::NormalizationPass;

/// `ClosureBoundaryAligned` replaces a function body of the form
/// `return (() => { ... })();` with the arrow's inner statements, aligning
/// the closure boundary with non-IIFE definitions. Expression-form arrows
/// (`(() => expr)()`) are converted to `return expr;`.
///
/// # Inlining preconditions
///
/// All three conditions below must hold; otherwise the pass leaves the
/// source untouched. Each precondition exists at the source — not as a
/// fallback — because lifting an IIFE that violates any of them would
/// change observable semantics:
///
/// * **`!arrow.r#async`** — an async IIFE returns a Promise; the outer
///   `return (async () => { … })()` evaluates to a Promise too. Inlining
///   the async body into a non-async outer would erase the Promise
///   wrapping and turn awaited values into raw expressions, breaking any
///   caller that `.then(…)`s the outer result.
///
/// * **`call.arguments.is_empty()`** — passed-in arguments could carry
///   side effects (e.g. `(() => { … })(globalLog())`); lifting the body
///   would drop those evaluations.
///
/// * **`arrow.params.items.is_empty()`** — even if no arguments are
///   passed, parameter declarations with default expressions (`((x = f()) =>
///   { … })()`) execute on call. Lifting the body would silently skip
///   those defaults.
///
/// The regression tests in this module pin each boundary so future
/// "improvements" that relax any condition trigger a visible failure
/// rather than a silent semantic drift.
pub struct ClosureBoundaryAligned;

impl NormalizationPass for ClosureBoundaryAligned {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::ClosureBoundaryAligned
    }

    fn version(&self) -> u32 {
        1
    }

    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut visitor = Aligner {
            builder: AstBuilder::new(alloc),
        };
        visitor.visit_program(program);
    }
}

struct Aligner<'a> {
    builder: AstBuilder<'a>,
}

/// Strip parentheses from an expression, returning a reference to the inner expression.
fn strip_parens<'a, 'b>(expr: &'b Expression<'a>) -> &'b Expression<'a> {
    match expr {
        Expression::ParenthesizedExpression(paren) => strip_parens(&paren.expression),
        other => other,
    }
}

/// Strip parentheses from an expression by value, returning the inner expression.
fn strip_parens_owned<'a>(expr: Expression<'a>) -> Expression<'a> {
    match expr {
        Expression::ParenthesizedExpression(paren) => strip_parens_owned(paren.unbox().expression),
        other => other,
    }
}

impl<'a> VisitMut<'a> for Aligner<'a> {
    fn visit_function_body(&mut self, body: &mut FunctionBody<'a>) {
        // Recurse first so nested IIFEs are inlined bottom-up.
        walk_function_body(self, body);

        if !is_iife_body(body) {
            return;
        }

        // Drain the single statement so we can take ownership and extract
        // call → arrow → arrow.body.
        let stmt = body
            .statements
            .drain(..)
            .next()
            .expect("len==1 checked above");
        if let Statement::ReturnStatement(ret_box) = stmt {
            let ret = ret_box.unbox();
            if let Some(Expression::CallExpression(call_box)) = ret.argument {
                let call = call_box.unbox();
                // The callee may be parenthesized: `(() => {...})`.
                let callee = strip_parens_owned(call.callee);
                if let Expression::ArrowFunctionExpression(arrow_box) = callee {
                    let arrow = arrow_box.unbox();
                    inline_arrow_body(&self.builder, body, arrow);
                    return;
                }
            }
        }
        // `is_iife_body` was true but extraction failed — should be unreachable.
        unreachable!("is_iife_body returned true but pattern extraction failed");
    }
}

fn is_iife_body(body: &FunctionBody<'_>) -> bool {
    if body.statements.len() != 1 {
        return false;
    }
    let Statement::ReturnStatement(ret) = &body.statements[0] else {
        return false;
    };
    let Some(Expression::CallExpression(call)) = &ret.argument else {
        return false;
    };
    if !call.arguments.is_empty() {
        return false;
    }
    // The callee may be wrapped in parentheses: `(() => {...})`.
    let callee = strip_parens(&call.callee);
    let Expression::ArrowFunctionExpression(arrow) = callee else {
        return false;
    };
    !arrow.r#async && arrow.params.items.is_empty()
}

fn inline_arrow_body<'a>(
    builder: &AstBuilder<'a>,
    outer_body: &mut FunctionBody<'a>,
    arrow: ArrowFunctionExpression<'a>,
) {
    let expression = arrow.expression;
    let arrow_body = arrow.body.unbox();
    if expression {
        // Expression-form arrow: `(() => expr)()`.
        // The FunctionBody contains a single ExpressionStatement wrapping the value.
        // Replace outer body with `return <value>;`.
        if let Some(stmt) = arrow_body.statements.into_iter().next()
            && let Statement::ExpressionStatement(expr_stmt) = stmt
        {
            let expr = expr_stmt.unbox().expression;
            let ret_stmt = builder.statement_return(SPAN, Some(expr));
            outer_body.statements.push(ret_stmt);
        }
    } else {
        // Block-form arrow: lift its statements into the outer body.
        for stmt in arrow_body.statements {
            outer_body.statements.push(stmt);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn iife_arrow_in_return_is_inlined() {
        let src = "function outer() { return (() => { let x = 1; return x; })(); }";
        let out = apply_to_source(&ClosureBoundaryAligned, src).expect("parses");
        assert!(!out.contains("() =>"), "expected arrow removed, got: {out}");
        assert!(
            out.contains("let x = 1") || out.contains("let x=1"),
            "got: {out}"
        );
    }

    #[test]
    fn non_iife_arrow_is_left_alone() {
        let src = "function outer() { return () => 1; }";
        let out = apply_to_source(&ClosureBoundaryAligned, src).expect("normalize should succeed");
        assert!(out.contains("=>"), "got: {out}");
    }

    #[test]
    fn iife_with_expression_body_is_inlined_as_return() {
        let src = "function outer() { return (() => 1)(); }";
        let out = apply_to_source(&ClosureBoundaryAligned, src).expect("parses");
        // Outer body should become `return 1;`
        assert!(out.contains("return 1"), "got: {out}");
        assert!(!out.contains("=>"), "got: {out}");
    }

    // -- Design-boundary regression locks -------------------------------------

    #[test]
    fn async_iife_is_not_inlined_promise_semantics_preserved() {
        // Inlining `return (async () => { ... })()` into a sync outer would
        // strip the Promise wrapping and break `.then` chains downstream.
        let src = "function outer() { return (async () => { return 1; })(); }";
        let out = apply_to_source(&ClosureBoundaryAligned, src).expect("parses");
        assert!(
            out.contains("async"),
            "async IIFE must remain to preserve Promise wrapping, got: {out}",
        );
    }

    #[test]
    fn iife_with_arguments_is_not_inlined_side_effects_preserved() {
        // The argument expression `globalLog()` may have side effects that
        // must execute on call; lifting the body would skip them entirely.
        let src = "function outer() { return (() => { return 1; })(globalLog()); }";
        let out = apply_to_source(&ClosureBoundaryAligned, src).expect("parses");
        assert!(
            out.contains("globalLog"),
            "argument with side effect must remain, got: {out}",
        );
        assert!(
            out.contains("=>") || out.contains("function"),
            "IIFE wrapping must remain, got: {out}",
        );
    }

    #[test]
    fn iife_with_params_is_not_inlined_default_initializers_preserved() {
        // Default parameter initializers (`x = f()`) execute on call. Lifting
        // the body into the outer scope would silently skip the default.
        let src = "function outer() { return ((x = f()) => { return x; })(); }";
        let out = apply_to_source(&ClosureBoundaryAligned, src).expect("parses");
        assert!(
            out.contains("=>") || out.contains("function"),
            "IIFE with params must remain to evaluate defaults, got: {out}",
        );
    }
}
