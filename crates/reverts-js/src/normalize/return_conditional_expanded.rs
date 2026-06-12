use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Expression, Program, Statement};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::{walk_block_statement, walk_function_body};
use oxc_span::SPAN;
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `ReturnConditionalExpanded` rewrites a `return <cond> ? <a> : <b>;`
/// statement into the two-statement form `if (<cond>) return <a>;
/// return <b>;`. Both are semantically identical (both branches
/// exit the function with the chosen value) but the minified form is
/// one statement while the expanded form is two — so the cascade
/// matcher's `statement_count`-keyed lookup diverges between
/// minified bundle and source unless we explicitly split.
///
/// This rewrite is **only** safe when the conditional appears as the
/// entire argument of a `ReturnStatement`. Anywhere else it carries
/// its value into a surrounding expression.
pub struct ReturnConditionalExpanded;

impl NormalizationPass for ReturnConditionalExpanded {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::ReturnConditionalExpanded
    }
    fn version(&self) -> u32 {
        1
    }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut visitor = Expander {
            builder: AstBuilder::new(alloc),
        };
        visitor.visit_program(program);
        rewrite_statements(&visitor.builder, &mut program.body);
    }
}

struct Expander<'a> {
    builder: AstBuilder<'a>,
}

impl<'a> VisitMut<'a> for Expander<'a> {
    fn visit_function_body(&mut self, body: &mut oxc_ast::ast::FunctionBody<'a>) {
        walk_function_body(self, body);
        rewrite_statements(&self.builder, &mut body.statements);
    }
    fn visit_block_statement(&mut self, b: &mut oxc_ast::ast::BlockStatement<'a>) {
        walk_block_statement(self, b);
        rewrite_statements(&self.builder, &mut b.body);
    }
}

fn rewrite_statements<'a>(
    builder: &AstBuilder<'a>,
    statements: &mut oxc_allocator::Vec<'a, Statement<'a>>,
) {
    if !statements.iter().any(is_return_conditional) {
        return;
    }
    let old = mem::replace(statements, oxc_allocator::Vec::new_in(builder.allocator));
    for stmt in old {
        match stmt {
            Statement::ReturnStatement(ret_box) => {
                let ret = ret_box.unbox();
                let Some(arg) = ret.argument else {
                    statements.push(Statement::ReturnStatement(
                        builder.alloc(builder.return_statement(SPAN, None)),
                    ));
                    continue;
                };
                let arg = strip_parens(arg);
                let Expression::ConditionalExpression(cond_box) = arg else {
                    statements.push(Statement::ReturnStatement(
                        builder.alloc(builder.return_statement(SPAN, Some(arg))),
                    ));
                    continue;
                };
                let cond = cond_box.unbox();
                let return_a = Statement::ReturnStatement(
                    builder.alloc(builder.return_statement(SPAN, Some(cond.consequent))),
                );
                let if_stmt = builder.statement_if(SPAN, cond.test, return_a, None);
                let return_b = Statement::ReturnStatement(
                    builder.alloc(builder.return_statement(SPAN, Some(cond.alternate))),
                );
                statements.push(if_stmt);
                statements.push(return_b);
            }
            other => statements.push(other),
        }
    }
}

fn is_return_conditional(stmt: &Statement<'_>) -> bool {
    let Statement::ReturnStatement(ret) = stmt else {
        return false;
    };
    let Some(arg) = &ret.argument else {
        return false;
    };
    matches!(strip_parens_ref(arg), Expression::ConditionalExpression(_))
}

fn strip_parens<'a>(expr: Expression<'a>) -> Expression<'a> {
    match expr {
        Expression::ParenthesizedExpression(p) => strip_parens(p.unbox().expression),
        other => other,
    }
}

fn strip_parens_ref<'a, 'b>(expr: &'b Expression<'a>) -> &'b Expression<'a> {
    match expr {
        Expression::ParenthesizedExpression(p) => strip_parens_ref(&p.expression),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn expands_return_ternary() {
        let out = apply_to_source(
            &ReturnConditionalExpanded,
            "function f(x) { return x ? 1 : 2; }",
        )
        .expect("parse");
        assert!(out.contains("if (x)"), "got: {out}");
        assert!(out.contains("return 1"), "got: {out}");
        assert!(out.contains("return 2"), "got: {out}");
    }

    #[test]
    fn leaves_non_conditional_return_alone() {
        let src = "function f(x) { return x + 1; }";
        let out = apply_to_source(&ReturnConditionalExpanded, src).expect("parse");
        assert!(out.contains("return x + 1"), "got: {out}");
    }

    #[test]
    fn leaves_void_return_alone() {
        let src = "function f() { return; }";
        let out = apply_to_source(&ReturnConditionalExpanded, src).expect("parse");
        assert!(out.contains("return;"), "got: {out}");
    }

    #[test]
    fn leaves_expression_ternary_alone() {
        // `let y = a ? 1 : 2;` must not be touched (value goes into y).
        let src = "function f(a) { let y = a ? 1 : 2; return y; }";
        let out = apply_to_source(&ReturnConditionalExpanded, src).expect("parse");
        assert!(out.contains("? 1 : 2"), "got: {out}");
    }

    #[test]
    fn idempotent_on_already_expanded() {
        let src = "function f(x) { if (x) return 1; return 2; }";
        let first = apply_to_source(&ReturnConditionalExpanded, src).expect("first");
        let second = apply_to_source(&ReturnConditionalExpanded, &first).expect("second");
        assert_eq!(first, second);
    }
}
