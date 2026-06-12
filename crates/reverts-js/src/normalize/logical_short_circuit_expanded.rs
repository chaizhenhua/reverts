use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Expression, Program, Statement};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::{walk_block_statement, walk_function_body};
use oxc_span::SPAN;
use oxc_syntax::operator::{LogicalOperator, UnaryOperator};
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `LogicalShortCircuitExpanded` rewrites short-circuit logical
/// expressions at statement position back into the explicit
/// conditional form:
///
/// ```js
/// // minifier-compact
/// a && b();
/// a || b();
///
/// // human-readable, semantically identical (at statement position)
/// if (a) b();
/// if (!a) b();
/// ```
///
/// The rewrite is **only** valid when the logical expression appears
/// as the entire body of an `ExpressionStatement` — anywhere else
/// (`return a && b`, `let x = a || b`, function arguments, etc.) the
/// short-circuit's value flows into the surrounding expression and the
/// `if`-statement form would discard it.
///
/// Minifiers consistently prefer `&&` / `||` over `if` at statement
/// position because the compact form saves bytes; this pass restores
/// the original control-flow shape so the cascade matcher's CFG axis
/// (which differentiates `LogicalExpression` from `IfStatement` even
/// when identifier-blind) aligns between minified bundle and source.
pub struct LogicalShortCircuitExpanded;

impl NormalizationPass for LogicalShortCircuitExpanded {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::LogicalShortCircuitExpanded
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
    // Only rewrite ExpressionStatement whose expression is a logical
    // `&&` or `||`. Parenthesized wrappers (added by some codegen
    // configurations) are transparent.
    let Statement::ExpressionStatement(es) = slot else {
        return;
    };
    let op = match strip_parens_ref(&es.expression) {
        Expression::LogicalExpression(l) => l.operator,
        _ => return,
    };
    if !matches!(op, LogicalOperator::And | LogicalOperator::Or) {
        return;
    }
    let placeholder = builder.statement_expression(SPAN, builder.expression_null_literal(SPAN));
    let owned = mem::replace(slot, placeholder);
    let Statement::ExpressionStatement(es_box) = owned else {
        unreachable!()
    };
    let es = es_box.unbox();
    let inner = strip_parens(es.expression);
    let Expression::LogicalExpression(l_box) = inner else {
        unreachable!()
    };
    let l = l_box.unbox();
    // Build `if (test) <body>;` where:
    //   - for `&&`: test = left, body = right
    //   - for `||`: test = !left, body = right
    let (test, body_expr) = match l.operator {
        LogicalOperator::And => (l.left, l.right),
        LogicalOperator::Or => {
            let negated = builder.expression_unary(SPAN, UnaryOperator::LogicalNot, l.left);
            (negated, l.right)
        }
        LogicalOperator::Coalesce => return, // shouldn't get here (filtered above)
    };
    let body_stmt = Statement::ExpressionStatement(
        builder.alloc(builder.expression_statement(SPAN, body_expr)),
    );
    *slot = builder.statement_if(SPAN, test, body_stmt, None);
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
    fn expands_logical_and_at_statement_position() {
        let out = apply_to_source(
            &LogicalShortCircuitExpanded,
            "function f(x) { x && doSomething(); }",
        )
        .expect("parse");
        assert!(out.contains("if (x)"), "got: {out}");
        assert!(out.contains("doSomething()"), "got: {out}");
        assert!(!out.contains("&&"), "got: {out}");
    }

    #[test]
    fn expands_logical_or_with_negation() {
        let out = apply_to_source(
            &LogicalShortCircuitExpanded,
            "function f(x) { x || init(); }",
        )
        .expect("parse");
        assert!(out.contains("if (!x)"), "got: {out}");
        assert!(out.contains("init()"), "got: {out}");
        assert!(!out.contains("||"), "got: {out}");
    }

    #[test]
    fn leaves_return_logical_alone() {
        // `return a && b` carries the value out — must not become
        // `if (a) return b;` (would lose the falsy `a` path).
        let out = apply_to_source(
            &LogicalShortCircuitExpanded,
            "function f(a, b) { return a && b; }",
        )
        .expect("parse");
        assert!(out.contains("a && b"), "got: {out}");
    }

    #[test]
    fn leaves_assignment_logical_alone() {
        let out = apply_to_source(
            &LogicalShortCircuitExpanded,
            "function f(a, b) { var x = a || b; return x; }",
        )
        .expect("parse");
        assert!(out.contains("a || b"), "got: {out}");
    }

    #[test]
    fn leaves_nullish_coalescing_alone() {
        // `a ?? b` semantics differ from `if (a == null) b` because of
        // explicit null/undefined check, so we don't touch it.
        let out = apply_to_source(&LogicalShortCircuitExpanded, "function f(a, b) { a ?? b; }")
            .expect("parse");
        assert!(out.contains("??"), "got: {out}");
    }

    #[test]
    fn idempotent_on_already_expanded() {
        let src = "function f(x) { if (x) doSomething(); }";
        let first = apply_to_source(&LogicalShortCircuitExpanded, src).expect("first");
        let second = apply_to_source(&LogicalShortCircuitExpanded, &first).expect("second");
        assert_eq!(first, second);
    }
}
