//! Normalized control-flow topology hash.
//!
//! This is a weak CFG axis layered next to the strict `cfg` axis. It keeps the
//! same expression-blind topology but canonicalizes common structured/unwrapped
//! return forms, e.g. `if (x) return a; return b;` and
//! `if (x) return a; else return b;`.

use oxc_ast::ast::{FunctionBody, Statement};
use reverts_ir::hash::{FNV_OFFSET_BASIS, update_fnv1a};

#[must_use]
pub fn compute(body: &FunctionBody<'_>) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, b"cfg_norm|");
    walk_statements(&mut hash, &body.statements);
    hash
}

fn walk_statements(hash: &mut u64, stmts: &[Statement<'_>]) {
    update_fnv1a(hash, b"(");
    let mut index = 0usize;
    while index < stmts.len() {
        if let Some((if_stmt, following)) =
            if_without_else_then_terminal_followed_by_terminal(stmts, index)
        {
            update_fnv1a(hash, b"if");
            walk_statement(hash, &if_stmt.consequent);
            update_fnv1a(hash, b"|else");
            walk_statement(hash, following);
            update_fnv1a(hash, b";");
            index += 2;
            continue;
        }
        walk_statement(hash, &stmts[index]);
        index += 1;
    }
    update_fnv1a(hash, b")");
}

fn if_without_else_then_terminal_followed_by_terminal<'a>(
    stmts: &'a [Statement<'a>],
    index: usize,
) -> Option<(&'a oxc_ast::ast::IfStatement<'a>, &'a Statement<'a>)> {
    let Statement::IfStatement(if_stmt) = &stmts[index] else {
        return None;
    };
    if if_stmt.alternate.is_some() || !is_terminal_statement(&if_stmt.consequent) {
        return None;
    }
    let following = stmts.get(index + 1)?;
    is_terminal_statement(following).then_some((if_stmt, following))
}

fn is_terminal_statement(stmt: &Statement<'_>) -> bool {
    match stmt {
        Statement::ReturnStatement(_) | Statement::ThrowStatement(_) => true,
        Statement::BlockStatement(block) if block.body.len() == 1 => {
            is_terminal_statement(&block.body[0])
        }
        _ => false,
    }
}

fn walk_statement(hash: &mut u64, stmt: &Statement<'_>) {
    if let Statement::BlockStatement(block) = stmt
        && block.body.len() == 1
    {
        walk_statement(hash, &block.body[0]);
        return;
    }
    match stmt {
        Statement::BlockStatement(block) => {
            update_fnv1a(hash, b"blk");
            walk_statements(hash, &block.body);
        }
        Statement::IfStatement(if_stmt) => {
            update_fnv1a(hash, b"if");
            walk_statement(hash, &if_stmt.consequent);
            update_fnv1a(hash, b"|else");
            if let Some(alt) = &if_stmt.alternate {
                walk_statement(hash, alt);
            } else {
                update_fnv1a(hash, b"_");
            }
            update_fnv1a(hash, b";");
        }
        Statement::ForStatement(stmt) => {
            update_fnv1a(hash, b"for");
            walk_statement(hash, &stmt.body);
            update_fnv1a(hash, b";");
        }
        Statement::WhileStatement(stmt) => {
            update_fnv1a(hash, b"wh");
            walk_statement(hash, &stmt.body);
            update_fnv1a(hash, b";");
        }
        Statement::DoWhileStatement(stmt) => {
            update_fnv1a(hash, b"dw");
            walk_statement(hash, &stmt.body);
            update_fnv1a(hash, b";");
        }
        Statement::ForInStatement(stmt) => {
            update_fnv1a(hash, b"fin");
            walk_statement(hash, &stmt.body);
            update_fnv1a(hash, b";");
        }
        Statement::ForOfStatement(stmt) => {
            update_fnv1a(hash, b"fof");
            walk_statement(hash, &stmt.body);
            update_fnv1a(hash, b";");
        }
        Statement::SwitchStatement(stmt) => {
            update_fnv1a(hash, b"sw");
            for case in &stmt.cases {
                update_fnv1a(hash, b"|case");
                walk_statements(hash, &case.consequent);
            }
            update_fnv1a(hash, b";");
        }
        Statement::TryStatement(stmt) => {
            update_fnv1a(hash, b"try");
            walk_statements(hash, &stmt.block.body);
            if let Some(handler) = &stmt.handler {
                update_fnv1a(hash, b"|catch");
                walk_statements(hash, &handler.body.body);
            }
            if let Some(fin) = &stmt.finalizer {
                update_fnv1a(hash, b"|fin");
                walk_statements(hash, &fin.body);
            }
            update_fnv1a(hash, b";");
        }
        Statement::ReturnStatement(_) => update_fnv1a(hash, b"ret;"),
        Statement::ThrowStatement(_) => update_fnv1a(hash, b"thr;"),
        Statement::BreakStatement(_) => update_fnv1a(hash, b"brk;"),
        Statement::ContinueStatement(_) => update_fnv1a(hash, b"cont;"),
        Statement::LabeledStatement(stmt) => {
            update_fnv1a(hash, b"lbl");
            walk_statement(hash, &stmt.body);
            update_fnv1a(hash, b";");
        }
        _ => update_fnv1a(hash, b".;"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn hash_first(src: &str) -> u64 {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, src, SourceType::default()).parse();
        assert!(parsed.errors.is_empty(), "parse: {:?}", parsed.errors);
        let func = parsed
            .program
            .body
            .iter()
            .find_map(|s| {
                if let oxc_ast::ast::Statement::FunctionDeclaration(f) = s {
                    Some(f)
                } else {
                    None
                }
            })
            .expect("function");
        compute(func.body.as_ref().expect("body"))
    }

    #[test]
    fn if_return_followed_by_return_matches_explicit_else_return() {
        let flat = hash_first("function f(x) { if (x) return 1; return 2; }");
        let with_else = hash_first("function f(x) { if (x) return 1; else return 2; }");
        assert_eq!(flat, with_else);
    }

    #[test]
    fn non_terminal_following_statement_is_not_folded() {
        let flat = hash_first("function f(x) { if (x) return 1; foo(); return 2; }");
        let with_else = hash_first("function f(x) { if (x) return 1; else foo(); return 2; }");
        assert_ne!(flat, with_else);
    }
}
