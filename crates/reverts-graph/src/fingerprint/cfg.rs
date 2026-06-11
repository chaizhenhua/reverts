//! Control-flow topology hash. Identifier-blind, expression-blind: only the
//! shape of statements + branches contributes. Distinct from `ast` (which
//! also hashes expressions) and from `structural_anchor` (which counts but
//! does not record branching topology).

use oxc_ast::ast::{FunctionBody, Statement};
use reverts_ir::hash::{FNV_OFFSET_BASIS, update_fnv1a};

#[must_use]
pub fn compute(body: &FunctionBody<'_>) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, b"cfg|");
    walk_statements(&mut hash, &body.statements);
    hash
}

fn walk_statements(hash: &mut u64, stmts: &[Statement<'_>]) {
    update_fnv1a(hash, b"(");
    for stmt in stmts {
        walk_statement(hash, stmt);
    }
    update_fnv1a(hash, b")");
}

fn walk_statement(hash: &mut u64, stmt: &Statement<'_>) {
    match stmt {
        Statement::BlockStatement(b) => {
            update_fnv1a(hash, b"blk");
            walk_statements(hash, &b.body);
        }
        Statement::IfStatement(i) => {
            update_fnv1a(hash, b"if");
            walk_statement(hash, &i.consequent);
            update_fnv1a(hash, b"|else");
            if let Some(alt) = &i.alternate {
                walk_statement(hash, alt);
            } else {
                update_fnv1a(hash, b"_");
            }
            update_fnv1a(hash, b";");
        }
        Statement::ForStatement(s) => {
            update_fnv1a(hash, b"for");
            walk_statement(hash, &s.body);
            update_fnv1a(hash, b";");
        }
        Statement::WhileStatement(s) => {
            update_fnv1a(hash, b"wh");
            walk_statement(hash, &s.body);
            update_fnv1a(hash, b";");
        }
        Statement::DoWhileStatement(s) => {
            update_fnv1a(hash, b"dw");
            walk_statement(hash, &s.body);
            update_fnv1a(hash, b";");
        }
        Statement::ForInStatement(s) => {
            update_fnv1a(hash, b"fin");
            walk_statement(hash, &s.body);
            update_fnv1a(hash, b";");
        }
        Statement::ForOfStatement(s) => {
            update_fnv1a(hash, b"fof");
            walk_statement(hash, &s.body);
            update_fnv1a(hash, b";");
        }
        Statement::SwitchStatement(s) => {
            update_fnv1a(hash, b"sw");
            for case in &s.cases {
                update_fnv1a(hash, b"|case");
                walk_statements(hash, &case.consequent);
            }
            update_fnv1a(hash, b";");
        }
        Statement::TryStatement(s) => {
            update_fnv1a(hash, b"try");
            walk_statements(hash, &s.block.body);
            if let Some(handler) = &s.handler {
                update_fnv1a(hash, b"|catch");
                walk_statements(hash, &handler.body.body);
            }
            if let Some(fin) = &s.finalizer {
                update_fnv1a(hash, b"|fin");
                walk_statements(hash, &fin.body);
            }
            update_fnv1a(hash, b";");
        }
        Statement::ReturnStatement(_) => update_fnv1a(hash, b"ret;"),
        Statement::ThrowStatement(_) => update_fnv1a(hash, b"thr;"),
        Statement::BreakStatement(_) => update_fnv1a(hash, b"brk;"),
        Statement::ContinueStatement(_) => update_fnv1a(hash, b"cont;"),
        Statement::LabeledStatement(s) => {
            update_fnv1a(hash, b"lbl");
            walk_statement(hash, &s.body);
            update_fnv1a(hash, b";");
        }
        // Any other statement is an opaque flow point (expression statements,
        // declarations, with-statement, etc.). Collapsing them all to a single
        // tag is the intended distinction from `ast` axis — only branching
        // topology shapes the cfg hash.
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
    fn cfg_collides_when_only_expressions_differ_under_same_branches() {
        let a = hash_first("function f(x) { if (x > 0) return 1; return 2; }");
        let b = hash_first("function f(x) { if (x.foo()) return 'a'; return 'b'; }");
        assert_eq!(a, b);
    }

    #[test]
    fn cfg_distinguishes_if_with_else_from_if_without_else() {
        let with_else = hash_first("function f(x) { if (x) return 1; else return 2; }");
        let no_else = hash_first("function f(x) { if (x) return 1; return 2; }");
        assert_ne!(with_else, no_else);
    }

    #[test]
    fn cfg_distinguishes_loop_kinds() {
        let for_loop = hash_first("function f(xs) { for (let x of xs) {} }");
        let while_loop = hash_first("function f(xs) { while (xs.length) {} }");
        assert_ne!(for_loop, while_loop);
    }

    #[test]
    fn cfg_distinguishes_try_with_catch_from_bare_try() {
        let with_catch = hash_first("function f() { try { return 1; } catch (e) { throw e; } }");
        let with_finally = hash_first("function f() { try { return 1; } finally { return 2; } }");
        assert_ne!(with_catch, with_finally);
    }

    #[test]
    fn cfg_collides_under_identifier_rename() {
        let a = hash_first("function f(a, b) { try { return a + b; } catch (err) { throw err; } }");
        let b = hash_first("function g(x, y) { try { return x + y; } catch (z) { throw z; } }");
        assert_eq!(a, b);
    }
}
