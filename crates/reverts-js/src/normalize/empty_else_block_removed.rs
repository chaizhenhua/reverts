use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Program, Statement};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_statement;
use reverts_ir::NormalizationPassId;

use super::NormalizationPass;

/// `EmptyElseBlockRemoved` strips an empty alternate branch from an
/// `if` statement: `if (c) body; else {}` → `if (c) body;`.
///
/// Per ECMA-262 §14.6 (`IfStatement` evaluation), the alternate
/// branch is only entered when `ToBoolean(c)` is false. If the
/// alternate is an empty `BlockStatement` with zero inner statements,
/// entering it performs no observable work — control flows past the
/// `if` unchanged. Dropping the alternate is therefore **strictly
/// spec-equivalent** for an empty `BlockStatement` (or an
/// `EmptyStatement`).
///
/// The pass deliberately:
///
/// * Only strips truly empty alternates. Any inner statement (even
///   a lone `EmptyStatement` from a stray `;`) at most makes the
///   block one element — and `walk_statement`'s recursive call would
///   visit a `BlockStatement { body: [EmptyStatement] }`, which the
///   ast-hash's block-unwrap canonicalisation already collapses.
/// * Does NOT touch `if (c) body;` (no alternate at all) — that's
///   already the canonical "no else" form.
pub struct EmptyElseBlockRemoved;

impl NormalizationPass for EmptyElseBlockRemoved {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::EmptyElseBlockRemoved
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
    fn visit_statement(&mut self, stmt: &mut Statement<'a>) {
        walk_statement(self, stmt);
        let Statement::IfStatement(if_stmt) = stmt else {
            return;
        };
        let drop_alt = match &if_stmt.alternate {
            Some(Statement::BlockStatement(b)) => b.body.is_empty(),
            Some(Statement::EmptyStatement(_)) => true,
            _ => false,
        };
        if drop_alt {
            if_stmt.alternate = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn strips_empty_else_block() {
        let out = apply_to_source(
            &EmptyElseBlockRemoved,
            "function f(c) { if (c) doWork(); else {} }",
        )
        .expect("parse");
        assert!(!out.contains("else"), "got: {out}");
        assert!(out.contains("doWork()"), "got: {out}");
    }

    #[test]
    fn strips_else_with_empty_statement() {
        let out = apply_to_source(
            &EmptyElseBlockRemoved,
            "function f(c) { if (c) doWork(); else ; }",
        )
        .expect("parse");
        assert!(!out.contains("else"), "got: {out}");
    }

    #[test]
    fn leaves_else_with_body_alone() {
        let src = "function f(c) { if (c) doA(); else doB(); }";
        let out = apply_to_source(&EmptyElseBlockRemoved, src).expect("parse");
        assert!(out.contains("else"), "got: {out}");
    }

    #[test]
    fn leaves_else_with_inner_statement_alone() {
        // `else { ; }` has an EmptyStatement inside the block — not
        // strictly an empty block, so we leave it. Codegen / ast-hash
        // canonicalisation handles the rest.
        let src = "function f(c) { if (c) doA(); else { ; } }";
        let out = apply_to_source(&EmptyElseBlockRemoved, src).expect("parse");
        // The block body has one EmptyStatement, so the else stays.
        assert!(out.contains("else"), "got: {out}");
    }

    #[test]
    fn leaves_if_without_else_alone() {
        let src = "function f(c) { if (c) doA(); }";
        let out = apply_to_source(&EmptyElseBlockRemoved, src).expect("parse");
        assert!(out.contains("if ("), "got: {out}");
    }

    #[test]
    fn strips_nested_empty_else() {
        let out = apply_to_source(
            &EmptyElseBlockRemoved,
            "function f(a, b) { if (a) { if (b) doInner(); else {} } }",
        )
        .expect("parse");
        // The inner else should be stripped.
        // The outer if has no else, so should be untouched.
        let else_count = out.matches("else").count();
        assert_eq!(else_count, 0, "got: {out}");
        assert!(out.contains("doInner()"), "got: {out}");
    }

    #[test]
    fn idempotent_when_no_match() {
        let src = "function f(c) { if (c) doA(); }";
        let first = apply_to_source(&EmptyElseBlockRemoved, src).expect("first");
        let second = apply_to_source(&EmptyElseBlockRemoved, &first).expect("second");
        assert_eq!(first, second);
    }
}
