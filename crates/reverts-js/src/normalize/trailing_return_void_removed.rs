use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{FunctionBody, Program, Statement};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_function_body;
use reverts_ir::NormalizationPassId;

use super::NormalizationPass;

/// `TrailingReturnVoidRemoved` strips a final `return;` (no argument)
/// from any function body when it is the last statement in that body.
///
/// Per ECMA-262:
///
/// * §10.2.1 (`OrdinaryCallEvaluateBody`) — when control reaches the
///   end of a function body without a `return`, the call result is
///   `undefined`.
/// * §14.10.1 (`ReturnStatement` evaluation) — `return;` (no
///   argument) sets the completion value to `undefined`.
///
/// Both forms therefore produce the same `undefined` call result and
/// stop further statements after the trailing `return;` from running
/// (there are none — it is the last statement). The rewrite is
/// **strictly spec-equivalent**: no identifier dispatch, no shadowing
/// concerns (the result is literal `undefined`, produced by the call
/// completion path, not by reading the `undefined` global).
///
/// The pass deliberately does NOT touch:
///
/// * `return value;` (the argument changes the completion value).
/// * `return;` that is NOT the last statement — removing it would
///   let the following dead code execute.
/// * `return;` nested inside a try/catch/finally — removing the
///   try-block's terminator can change whether `finally` sees a
///   normal completion. Only outermost-of-body trailing returns are
///   touched.
pub struct TrailingReturnVoidRemoved;

impl NormalizationPass for TrailingReturnVoidRemoved {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::TrailingReturnVoidRemoved
    }
    fn version(&self) -> u32 {
        1
    }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut visitor = Stripper {
            _alloc: AstBuilder::new(alloc),
        };
        visitor.visit_program(program);
    }
}

struct Stripper<'a> {
    _alloc: AstBuilder<'a>,
}

impl<'a> VisitMut<'a> for Stripper<'a> {
    fn visit_function_body(&mut self, body: &mut FunctionBody<'a>) {
        walk_function_body(self, body);
        // Strip a trailing `return;` (no argument). Walk-first so any
        // nested function bodies have already been processed.
        if matches!(
            body.statements.last(),
            Some(Statement::ReturnStatement(r)) if r.argument.is_none()
        ) {
            body.statements.pop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn strips_trailing_bare_return_from_function() {
        let out = apply_to_source(
            &TrailingReturnVoidRemoved,
            "function f() { sideEffect(); return; }",
        )
        .expect("parse");
        assert!(!out.contains("return;"), "got: {out}");
        assert!(out.contains("sideEffect()"), "got: {out}");
    }

    #[test]
    fn strips_lone_bare_return_from_function() {
        let out =
            apply_to_source(&TrailingReturnVoidRemoved, "function f() { return; }").expect("parse");
        assert!(!out.contains("return"), "got: {out}");
    }

    #[test]
    fn keeps_return_with_value_alone() {
        let src = "function f() { return 42; }";
        let out = apply_to_source(&TrailingReturnVoidRemoved, src).expect("parse");
        assert!(out.contains("return 42"), "got: {out}");
    }

    #[test]
    fn keeps_mid_body_return_alone() {
        // `return;` in the middle of a body is NOT removable — the
        // statements after it would otherwise execute.
        let src = "function f(c) { if (c) return; sideEffect(); }";
        let out = apply_to_source(&TrailingReturnVoidRemoved, src).expect("parse");
        assert!(out.contains("return"), "got: {out}");
    }

    #[test]
    fn strips_inside_arrow_function_body() {
        let out = apply_to_source(
            &TrailingReturnVoidRemoved,
            "const f = () => { sideEffect(); return; };",
        )
        .expect("parse");
        assert!(!out.contains("return"), "got: {out}");
        assert!(out.contains("sideEffect"), "got: {out}");
    }

    #[test]
    fn strips_inside_nested_function() {
        let out = apply_to_source(
            &TrailingReturnVoidRemoved,
            "function outer() { function inner() { work(); return; } inner(); }",
        )
        .expect("parse");
        assert!(!out.contains("return;"), "got: {out}");
        assert!(out.contains("work()"), "got: {out}");
        assert!(out.contains("inner()"), "got: {out}");
    }

    #[test]
    fn strips_inside_method_body() {
        let out = apply_to_source(
            &TrailingReturnVoidRemoved,
            "class C { m() { work(); return; } }",
        )
        .expect("parse");
        assert!(!out.contains("return;"), "got: {out}");
        assert!(out.contains("work()"), "got: {out}");
    }

    #[test]
    fn idempotent_when_no_trailing_return() {
        let src = "function f() { sideEffect(); }";
        let first = apply_to_source(&TrailingReturnVoidRemoved, src).expect("first");
        let second = apply_to_source(&TrailingReturnVoidRemoved, &first).expect("second");
        assert_eq!(first, second);
    }
}
