use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Expression, Program, Statement};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::{walk_block_statement, walk_function_body};
use oxc_span::SPAN;
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `ConditionalStatementExpanded` rewrites ternary expressions that
/// appear at statement position back to explicit `if`/`else`:
///
/// ```js
/// // minifier-compact
/// cond ? doA() : doB();
///
/// // human-readable, semantically identical (at statement position)
/// if (cond) doA(); else doB();
/// ```
///
/// Only valid in `ExpressionStatement` position. `let x = a ? b : c`,
/// `return a ? b : c`, or `f(a ? b : c)` all carry the value into the
/// surrounding context and must not be rewritten.
pub struct ConditionalStatementExpanded;

impl NormalizationPass for ConditionalStatementExpanded {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::ConditionalStatementExpanded
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
    for slot in statements.iter_mut() {
        rewrite_slot(builder, slot);
    }
}

fn rewrite_slot<'a>(builder: &AstBuilder<'a>, slot: &mut Statement<'a>) {
    let Statement::ExpressionStatement(es) = slot else {
        return;
    };
    if !matches!(
        strip_parens_ref(&es.expression),
        Expression::ConditionalExpression(_)
    ) {
        return;
    }
    let placeholder = builder.statement_expression(SPAN, builder.expression_null_literal(SPAN));
    let owned = mem::replace(slot, placeholder);
    let Statement::ExpressionStatement(es_box) = owned else {
        unreachable!()
    };
    let es = es_box.unbox();
    let Expression::ConditionalExpression(cond_box) = strip_parens(es.expression) else {
        unreachable!()
    };
    let cond = cond_box.unbox();
    let consequent_stmt = Statement::ExpressionStatement(
        builder.alloc(builder.expression_statement(SPAN, cond.consequent)),
    );
    let alternate_stmt = Statement::ExpressionStatement(
        builder.alloc(builder.expression_statement(SPAN, cond.alternate)),
    );
    *slot = builder.statement_if(SPAN, cond.test, consequent_stmt, Some(alternate_stmt));
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
    fn expands_ternary_at_statement_position() {
        let out = apply_to_source(
            &ConditionalStatementExpanded,
            "function f(x) { x ? doA() : doB(); }",
        )
        .expect("parse");
        assert!(out.contains("if (x)"), "got: {out}");
        assert!(out.contains("doA()"), "got: {out}");
        assert!(out.contains("else"), "got: {out}");
        assert!(out.contains("doB()"), "got: {out}");
    }

    #[test]
    fn leaves_return_ternary_alone() {
        let out = apply_to_source(
            &ConditionalStatementExpanded,
            "function f(x) { return x ? 1 : 2; }",
        )
        .expect("parse");
        assert!(out.contains("? 1 : 2"), "got: {out}");
    }

    #[test]
    fn leaves_assignment_ternary_alone() {
        let out = apply_to_source(
            &ConditionalStatementExpanded,
            "function f(x) { var y = x ? 1 : 2; return y; }",
        )
        .expect("parse");
        assert!(out.contains("? 1 : 2"), "got: {out}");
    }

    #[test]
    fn idempotent_on_already_expanded() {
        let src = "function f(x) { if (x) doA(); else doB(); }";
        let first = apply_to_source(&ConditionalStatementExpanded, src).expect("first");
        let second = apply_to_source(&ConditionalStatementExpanded, &first).expect("second");
        assert_eq!(first, second);
    }
}
