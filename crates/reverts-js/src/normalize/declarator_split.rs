use oxc_allocator::{Allocator, Vec as ArenaVec};
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Program, Statement, VariableDeclaration, VariableDeclarator};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_function_body;
use oxc_span::SPAN;
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `DeclaratorSplit` rewrites multi-declarator variable statements
/// (`var a = 1, b = 2;`) into separate single-declarator statements
/// (`var a = 1; var b = 2;`). The two forms are semantically identical
/// for top-level/function-scoped/block-scoped declarations; minifiers
/// prefer the compact comma-separated form to save bytes.
///
/// Splitting normalises the *statement count* of a function body so
/// that minified and source-form versions of the same code converge
/// in `statement_count` — the per-axis component used as part of
/// `ExactKey` and `CfgKey` for the cascade matcher.
pub struct DeclaratorSplit;

impl NormalizationPass for DeclaratorSplit {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::DeclaratorSplit
    }
    fn version(&self) -> u32 {
        1
    }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut visitor = Splitter {
            builder: AstBuilder::new(alloc),
        };
        visitor.visit_program(program);
        // Also split at the program top level (visit_program doesn't
        // re-enter for us).
        split_statements(&visitor.builder, &mut program.body);
    }
}

struct Splitter<'a> {
    builder: AstBuilder<'a>,
}

impl<'a> VisitMut<'a> for Splitter<'a> {
    fn visit_function_body(&mut self, body: &mut oxc_ast::ast::FunctionBody<'a>) {
        // Descend first so nested function bodies are split too.
        walk_function_body(self, body);
        split_statements(&self.builder, &mut body.statements);
    }
    fn visit_block_statement(&mut self, b: &mut oxc_ast::ast::BlockStatement<'a>) {
        oxc_ast::visit::walk_mut::walk_block_statement(self, b);
        split_statements(&self.builder, &mut b.body);
    }
}

fn split_statements<'a>(builder: &AstBuilder<'a>, statements: &mut ArenaVec<'a, Statement<'a>>) {
    if statements.iter().all(|s| !is_multi_declarator(s)) {
        return;
    }
    let old = mem::replace(statements, ArenaVec::new_in(builder.allocator));
    for stmt in old {
        match stmt {
            Statement::VariableDeclaration(v_box) if v_box.declarations.len() > 1 => {
                let v = v_box.unbox();
                let kind = v.kind;
                let declare = v.declare;
                for decl in v.declarations {
                    let mut single: ArenaVec<'a, VariableDeclarator<'a>> =
                        ArenaVec::new_in(builder.allocator);
                    single.push(decl);
                    let new_decl: VariableDeclaration<'a> =
                        builder.variable_declaration(SPAN, kind, single, declare);
                    statements.push(Statement::VariableDeclaration(builder.alloc(new_decl)));
                }
            }
            other => statements.push(other),
        }
    }
}

fn is_multi_declarator(stmt: &Statement<'_>) -> bool {
    matches!(stmt, Statement::VariableDeclaration(v) if v.declarations.len() > 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn splits_multi_var_at_top_level() {
        let out = apply_to_source(
            &DeclaratorSplit,
            "function f() { var a = 1, b = 2; return a + b; }",
        )
        .expect("parse");
        // Should produce two `var` statements.
        let count = out.matches("var ").count();
        assert_eq!(count, 2, "got: {out}");
    }

    #[test]
    fn splits_multi_let_inside_block() {
        let out = apply_to_source(
            &DeclaratorSplit,
            "function f() { if (x) { let a = 1, b = 2; } }",
        )
        .expect("parse");
        let count = out.matches("let ").count();
        assert_eq!(count, 2, "got: {out}");
    }

    #[test]
    fn splits_multi_const() {
        let out = apply_to_source(
            &DeclaratorSplit,
            "function f() { const a = 1, b = 2, c = 3; }",
        )
        .expect("parse");
        let count = out.matches("const ").count();
        assert_eq!(count, 3, "got: {out}");
    }

    #[test]
    fn leaves_single_declarator_alone() {
        let src = "function f() { var x = 1; return x; }";
        let out = apply_to_source(&DeclaratorSplit, src).expect("parse");
        assert_eq!(out.matches("var ").count(), 1, "got: {out}");
    }

    #[test]
    fn idempotent_when_already_single() {
        let src = "function f() { var a = 1; var b = 2; }";
        let first = apply_to_source(&DeclaratorSplit, src).expect("first");
        let second = apply_to_source(&DeclaratorSplit, &first).expect("second");
        assert_eq!(first, second);
    }
}
