use oxc_allocator::{Allocator, Vec as ArenaVec};
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Program, Statement};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::{walk_block_statement, walk_function_body};
use oxc_span::SPAN;
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `IfReturnElseFlattened` removes the `else` clause from an
/// `if (cond) { return ...; } else { ... }` when the consequent
/// terminates control flow with `return` or `throw`. The else body
/// is hoisted to immediately follow the `if`, producing
/// `if (cond) { return ...; } ...rest`.
///
/// The rewrite is **strictly spec-equivalent** because reaching the
/// else branch requires the consequent NOT to take effect — which
/// only happens when `cond` is falsy. Whether the else is attached
/// to the `if` or floated as a sibling statement does not change
/// any observable execution: in the truthy branch the function has
/// already returned, in the falsy branch the rest-after-if runs
/// (whether labelled `else` or not).
///
/// This rewrite involves only pure syntax — no identifier dispatch,
/// no shadowing concerns.
pub struct IfReturnElseFlattened;

impl NormalizationPass for IfReturnElseFlattened {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::IfReturnElseFlattened
    }
    fn version(&self) -> u32 {
        1
    }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut visitor = Flattener {
            builder: AstBuilder::new(alloc),
        };
        visitor.visit_program(program);
        flatten_statements(&visitor.builder, &mut program.body);
    }
}

struct Flattener<'a> {
    builder: AstBuilder<'a>,
}

impl<'a> VisitMut<'a> for Flattener<'a> {
    fn visit_function_body(&mut self, body: &mut oxc_ast::ast::FunctionBody<'a>) {
        walk_function_body(self, body);
        flatten_statements(&self.builder, &mut body.statements);
    }
    fn visit_block_statement(&mut self, b: &mut oxc_ast::ast::BlockStatement<'a>) {
        walk_block_statement(self, b);
        flatten_statements(&self.builder, &mut b.body);
    }
}

fn flatten_statements<'a>(builder: &AstBuilder<'a>, statements: &mut ArenaVec<'a, Statement<'a>>) {
    if !statements.iter().any(needs_flatten) {
        return;
    }
    let old = mem::replace(statements, ArenaVec::new_in(builder.allocator));
    for stmt in old {
        match stmt {
            Statement::IfStatement(if_box) if has_terminating_consequent_with_else(&if_box) => {
                let if_stmt = if_box.unbox();
                let alternate = if_stmt.alternate.expect("verified Some by needs_flatten");
                // Rebuild the if with no alternate.
                let new_if = builder.statement_if(SPAN, if_stmt.test, if_stmt.consequent, None);
                statements.push(new_if);
                // Hoist the alternate body. If it's a BlockStatement,
                // splice its inner statements; otherwise push as-is.
                match alternate {
                    Statement::BlockStatement(b_box) => {
                        let b = b_box.unbox();
                        for s in b.body {
                            statements.push(s);
                        }
                    }
                    other => statements.push(other),
                }
            }
            other => statements.push(other),
        }
    }
}

fn needs_flatten(stmt: &Statement<'_>) -> bool {
    let Statement::IfStatement(i) = stmt else {
        return false;
    };
    has_terminating_consequent_with_else(i)
}

fn has_terminating_consequent_with_else(if_stmt: &oxc_ast::ast::IfStatement<'_>) -> bool {
    if if_stmt.alternate.is_none() {
        return false;
    }
    stmt_terminates(&if_stmt.consequent)
}

/// Whether the statement always exits the function on its execution
/// path. Conservative: only return-statement (with or without value)
/// and throw-statement count, plus block-statements whose last
/// statement also terminates.
fn stmt_terminates(stmt: &Statement<'_>) -> bool {
    match stmt {
        Statement::ReturnStatement(_) | Statement::ThrowStatement(_) => true,
        Statement::BlockStatement(b) => match b.body.last() {
            Some(last) => stmt_terminates(last),
            None => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn flattens_if_return_else_block() {
        let out = apply_to_source(
            &IfReturnElseFlattened,
            "function f(x) { if (x) { return 1; } else { return 2; } }",
        )
        .expect("parse");
        assert!(out.contains("return 1"), "got: {out}");
        assert!(out.contains("return 2"), "got: {out}");
        // The `else` should be gone — both returns become sequential.
        assert!(!out.contains("else"), "got: {out}");
    }

    #[test]
    fn flattens_if_throw_else_block() {
        let out = apply_to_source(
            &IfReturnElseFlattened,
            "function f(x) { if (!x) { throw new Error('bad'); } else { return x + 1; } }",
        )
        .expect("parse");
        assert!(!out.contains("else"), "got: {out}");
        assert!(out.contains("throw"), "got: {out}");
        assert!(out.contains("return x + 1"), "got: {out}");
    }

    #[test]
    fn flattens_with_multi_statement_else_body() {
        let out = apply_to_source(
            &IfReturnElseFlattened,
            "function f(x) { if (x) { return 1; } else { let y = x * 2; return y; } }",
        )
        .expect("parse");
        assert!(!out.contains("else"), "got: {out}");
        assert!(out.contains("let y"), "got: {out}");
    }

    #[test]
    fn leaves_if_without_terminating_consequent_alone() {
        // `if (x) doSomething();` (no return) — else stays.
        let out = apply_to_source(
            &IfReturnElseFlattened,
            "function f(x) { if (x) doSomething(); else doOther(); }",
        )
        .expect("parse");
        assert!(out.contains("else"), "got: {out}");
    }

    #[test]
    fn leaves_if_without_else_alone() {
        let src = "function f(x) { if (x) return 1; doMore(); }";
        let out = apply_to_source(&IfReturnElseFlattened, src).expect("parse");
        assert!(out.contains("return 1"), "got: {out}");
    }

    #[test]
    fn idempotent_when_no_match() {
        let src = "function add(a, b) { return a + b; }";
        let first = apply_to_source(&IfReturnElseFlattened, src).expect("first");
        let second = apply_to_source(&IfReturnElseFlattened, &first).expect("second");
        assert_eq!(first, second);
    }
}
