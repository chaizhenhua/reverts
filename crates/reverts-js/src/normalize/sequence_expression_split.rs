use oxc_allocator::{Allocator, Vec as ArenaVec};
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Expression, Program, Statement};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::{walk_block_statement, walk_function_body};
use oxc_span::SPAN;
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `SequenceExpressionSplit` rewrites comma-sequence expressions that
/// appear in statement position back into separate statements:
///
/// ```js
/// // before (minifier-compact)
/// a(); (b(), c(), d());
/// // after (human-readable, semantically identical)
/// a(); b(); c(); d();
/// ```
///
/// The rewrite is safe **only** at statement position:
/// `Statement::ExpressionStatement(SequenceExpression([…]))`. A comma
/// operator in any other position (e.g. `return (a, b, c);`,
/// `f((a, b, c))`, `let x = (a, b, c)`) carries a value back to the
/// surrounding expression and cannot be split without changing
/// semantics.
///
/// Minifiers prefer the sequence form because a comma is a single byte
/// whereas a separator semicolon plus the whitespace the codegen
/// surrounds it with is several. Splitting realigns the statement
/// count between minified bundles and original source — a hard
/// requirement for the cascade matcher's `ExactKey` lookup which keys
/// on `(param_count, statement_count, ast_hash)`.
pub struct SequenceExpressionSplit;

impl NormalizationPass for SequenceExpressionSplit {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::SequenceExpressionSplit
    }
    fn version(&self) -> u32 {
        1
    }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut visitor = Splitter {
            builder: AstBuilder::new(alloc),
        };
        visitor.visit_program(program);
        split_statements(&visitor.builder, &mut program.body);
    }
}

struct Splitter<'a> {
    builder: AstBuilder<'a>,
}

impl<'a> VisitMut<'a> for Splitter<'a> {
    fn visit_function_body(&mut self, body: &mut oxc_ast::ast::FunctionBody<'a>) {
        walk_function_body(self, body);
        split_statements(&self.builder, &mut body.statements);
    }
    fn visit_block_statement(&mut self, b: &mut oxc_ast::ast::BlockStatement<'a>) {
        walk_block_statement(self, b);
        split_statements(&self.builder, &mut b.body);
    }
}

fn split_statements<'a>(builder: &AstBuilder<'a>, statements: &mut ArenaVec<'a, Statement<'a>>) {
    if !statements.iter().any(is_sequence_statement) {
        return;
    }
    let old = mem::replace(statements, ArenaVec::new_in(builder.allocator));
    for stmt in old {
        match stmt {
            Statement::ExpressionStatement(es_box) => {
                let es = es_box.unbox();
                let inner = strip_parens(es.expression);
                if let Expression::SequenceExpression(seq_box) = inner {
                    let seq = seq_box.unbox();
                    for expr in seq.expressions {
                        statements.push(Statement::ExpressionStatement(
                            builder.alloc(builder.expression_statement(SPAN, expr)),
                        ));
                    }
                } else {
                    statements.push(Statement::ExpressionStatement(
                        builder.alloc(builder.expression_statement(SPAN, inner)),
                    ));
                }
            }
            other => statements.push(other),
        }
    }
}

fn strip_parens<'a>(expr: Expression<'a>) -> Expression<'a> {
    match expr {
        Expression::ParenthesizedExpression(p) => strip_parens(p.unbox().expression),
        other => other,
    }
}

fn is_sequence_statement(stmt: &Statement<'_>) -> bool {
    let Statement::ExpressionStatement(es) = stmt else {
        return false;
    };
    matches!(
        strip_parens_ref(&es.expression),
        Expression::SequenceExpression(_)
    )
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
    fn splits_sequence_at_top_level() {
        let out = apply_to_source(
            &SequenceExpressionSplit,
            "function f() { a(); (b(), c(), d()); }",
        )
        .expect("parse");
        // 4 separate statements now.
        let count = out.matches("();").count();
        assert_eq!(count, 4, "got: {out}");
    }

    #[test]
    fn splits_sequence_inside_block() {
        let out = apply_to_source(
            &SequenceExpressionSplit,
            "function f() { if (x) { a(), b(); } }",
        )
        .expect("parse");
        assert!(out.contains("a();") && out.contains("b();"), "got: {out}");
    }

    #[test]
    fn leaves_return_sequence_alone() {
        // `return (a, b, c)` is semantically NOT equivalent to
        // splitting because the value flows into return.
        let out = apply_to_source(
            &SequenceExpressionSplit,
            "function f() { return (a, b, c); }",
        )
        .expect("parse");
        // The sequence should survive (rendered as either `a, b, c` or
        // `(a, b, c)` depending on codegen).
        assert!(
            out.contains("a, b, c") || out.contains("a,b,c"),
            "got: {out}"
        );
    }

    #[test]
    fn leaves_assignment_sequence_alone() {
        // `x = (a, b, c)` carries the value of c into x.
        let out = apply_to_source(
            &SequenceExpressionSplit,
            "function f() { var x = (a, b, c); return x; }",
        )
        .expect("parse");
        assert!(
            out.contains("a, b, c") || out.contains("a,b,c"),
            "got: {out}"
        );
    }

    #[test]
    fn idempotent_on_already_split() {
        let src = "function f() { a(); b(); c(); }";
        let first = apply_to_source(&SequenceExpressionSplit, src).expect("first");
        let second = apply_to_source(&SequenceExpressionSplit, &first).expect("second");
        assert_eq!(first, second);
    }
}
