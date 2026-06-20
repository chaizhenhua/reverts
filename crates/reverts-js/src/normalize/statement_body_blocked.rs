use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Program, Statement};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_statement;
use oxc_span::SPAN;
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `StatementBodyBlocked` wraps the single-statement body of an `if` /
/// `else` / `for` / `for-in` / `for-of` / `while` / `do-while` in a block:
///
/// * `if (x) y();`        → `if (x) { y(); }`
/// * `while (x) y();`      → `while (x) { y(); }`
/// * `if (x) a(); else b();` → `if (x) { a(); } else { b(); }`
///
/// Minifiers drop the braces around single-statement bodies; authors keep them.
/// The AST distinguishes a bare `ExpressionStatement` body from a
/// `BlockStatement` body, so the two builds hash differently. Wrapping every
/// body in a block gives a single canonical shape. An `else if` chain
/// (`alternate` is itself an `IfStatement`) is also wrapped, which uniformly
/// canonicalizes `else if` into `else { if … }` — so a separate else-if merge
/// pass is unnecessary.
pub struct StatementBodyBlocked;

impl NormalizationPass for StatementBodyBlocked {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::StatementBodyBlocked
    }

    fn version(&self) -> u32 {
        1
    }

    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut visitor = Rewriter {
            builder: AstBuilder::new(alloc),
        };
        visitor.visit_program(program);
    }
}

struct Rewriter<'a> {
    builder: AstBuilder<'a>,
}

impl<'a> Rewriter<'a> {
    /// Wrap `stmt` in a `{ … }` unless it already is a block (or an empty
    /// statement, where braces would add a meaningless empty block).
    fn block(&self, stmt: &mut Statement<'a>) {
        if matches!(
            stmt,
            Statement::BlockStatement(_) | Statement::EmptyStatement(_)
        ) {
            return;
        }
        let owned = mem::replace(stmt, self.builder.statement_empty(SPAN));
        let body = self.builder.vec1(owned);
        *stmt = self.builder.statement_block(SPAN, body);
    }
}

impl<'a> VisitMut<'a> for Rewriter<'a> {
    fn visit_statement(&mut self, stmt: &mut Statement<'a>) {
        walk_statement(self, stmt);
        match stmt {
            Statement::IfStatement(if_stmt) => {
                self.block(&mut if_stmt.consequent);
                if let Some(alternate) = &mut if_stmt.alternate {
                    self.block(alternate);
                }
            }
            Statement::ForStatement(for_stmt) => self.block(&mut for_stmt.body),
            Statement::ForInStatement(for_stmt) => self.block(&mut for_stmt.body),
            Statement::ForOfStatement(for_stmt) => self.block(&mut for_stmt.body),
            Statement::WhileStatement(while_stmt) => self.block(&mut while_stmt.body),
            Statement::DoWhileStatement(do_stmt) => self.block(&mut do_stmt.body),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::StatementBodyBlocked;
    use crate::normalize::apply_to_source;

    fn run(src: &str) -> String {
        apply_to_source(&StatementBodyBlocked, src).expect("apply")
    }

    #[test]
    fn wraps_braceless_if_and_loop_bodies() {
        assert!(run("if (x) y();").contains('{'));
        assert!(run("while (x) y();").contains('{'));
        assert!(run("for (;;) y();").contains('{'));
    }

    #[test]
    fn wraps_else_branch_and_else_if() {
        let out = run("if (x) a(); else b();");
        assert_eq!(out.matches('{').count(), 2, "{out}");
        // else-if: alternate IfStatement also wrapped.
        assert!(run("if (x) a(); else if (y) b();").contains("else {"));
    }

    #[test]
    fn leaves_existing_block_alone() {
        let out = run("if (x) { y(); }");
        assert_eq!(out.matches('{').count(), 1, "{out}");
    }
}
