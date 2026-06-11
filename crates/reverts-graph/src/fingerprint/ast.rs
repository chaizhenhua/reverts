use oxc_ast::ast::{Expression, FunctionBody, Statement};
use reverts_ir::hash::{FNV_OFFSET_BASIS, update_fnv1a};

#[must_use]
pub fn compute(body: &FunctionBody<'_>) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, b"function_body|");
    for stmt in &body.statements {
        hash_statement(&mut hash, stmt);
    }
    hash
}

fn hash_statement(hash: &mut u64, stmt: &Statement<'_>) {
    update_fnv1a(hash, b"|stmt:");
    match stmt {
        Statement::BlockStatement(b) => {
            update_fnv1a(hash, b"block(");
            for s in &b.body {
                hash_statement(hash, s);
            }
            update_fnv1a(hash, b")");
        }
        Statement::ExpressionStatement(e) => {
            update_fnv1a(hash, b"expr(");
            hash_expression(hash, &e.expression);
            update_fnv1a(hash, b")");
        }
        Statement::ReturnStatement(r) => {
            update_fnv1a(hash, b"return(");
            if let Some(arg) = &r.argument {
                hash_expression(hash, arg);
            }
            update_fnv1a(hash, b")");
        }
        Statement::IfStatement(i) => {
            update_fnv1a(hash, b"if(");
            hash_expression(hash, &i.test);
            update_fnv1a(hash, b",");
            hash_statement(hash, &i.consequent);
            if let Some(alt) = &i.alternate {
                update_fnv1a(hash, b",");
                hash_statement(hash, alt);
            }
            update_fnv1a(hash, b")");
        }
        Statement::ForStatement(_) => update_fnv1a(hash, b"for"),
        Statement::WhileStatement(_) => update_fnv1a(hash, b"while"),
        Statement::DoWhileStatement(_) => update_fnv1a(hash, b"dowhile"),
        Statement::ForOfStatement(_) => update_fnv1a(hash, b"forof"),
        Statement::ForInStatement(_) => update_fnv1a(hash, b"forin"),
        Statement::TryStatement(_) => update_fnv1a(hash, b"try"),
        Statement::ThrowStatement(t) => {
            update_fnv1a(hash, b"throw(");
            hash_expression(hash, &t.argument);
            update_fnv1a(hash, b")");
        }
        Statement::SwitchStatement(_) => update_fnv1a(hash, b"switch"),
        Statement::VariableDeclaration(v) => {
            update_fnv1a(hash, b"var(");
            update_fnv1a(hash, format!("{:?}", v.kind).as_bytes());
            update_fnv1a(hash, format!("/{}", v.declarations.len()).as_bytes());
            update_fnv1a(hash, b")");
        }
        Statement::BreakStatement(_) => update_fnv1a(hash, b"break"),
        Statement::ContinueStatement(_) => update_fnv1a(hash, b"continue"),
        _ => update_fnv1a(hash, b"other"),
    }
}

fn hash_expression(hash: &mut u64, expr: &Expression<'_>) {
    use Expression as E;
    match expr {
        E::Identifier(_) => update_fnv1a(hash, b"id"),
        E::StringLiteral(_) => update_fnv1a(hash, b"str"),
        E::NumericLiteral(_) => update_fnv1a(hash, b"num"),
        E::BooleanLiteral(_) => update_fnv1a(hash, b"bool"),
        E::NullLiteral(_) => update_fnv1a(hash, b"null"),
        E::RegExpLiteral(_) => update_fnv1a(hash, b"re"),
        E::BinaryExpression(b) => {
            update_fnv1a(hash, b"bin(");
            update_fnv1a(hash, format!("{:?}", b.operator).as_bytes());
            update_fnv1a(hash, b",");
            hash_expression(hash, &b.left);
            update_fnv1a(hash, b",");
            hash_expression(hash, &b.right);
            update_fnv1a(hash, b")");
        }
        E::UnaryExpression(u) => {
            update_fnv1a(hash, b"un(");
            update_fnv1a(hash, format!("{:?}", u.operator).as_bytes());
            update_fnv1a(hash, b",");
            hash_expression(hash, &u.argument);
            update_fnv1a(hash, b")");
        }
        E::CallExpression(c) => {
            update_fnv1a(hash, b"call(");
            hash_expression(hash, &c.callee);
            update_fnv1a(hash, format!("/{}", c.arguments.len()).as_bytes());
            update_fnv1a(hash, b")");
        }
        E::StaticMemberExpression(_) => update_fnv1a(hash, b"smem"),
        E::ComputedMemberExpression(_) => update_fnv1a(hash, b"cmem"),
        E::ConditionalExpression(_) => update_fnv1a(hash, b"cond"),
        E::AssignmentExpression(_) => update_fnv1a(hash, b"assign"),
        E::ArrowFunctionExpression(_) => update_fnv1a(hash, b"arrow"),
        E::FunctionExpression(_) => update_fnv1a(hash, b"fnexpr"),
        E::ObjectExpression(_) => update_fnv1a(hash, b"obj"),
        E::ArrayExpression(_) => update_fnv1a(hash, b"arr"),
        E::AwaitExpression(_) => update_fnv1a(hash, b"await"),
        E::YieldExpression(_) => update_fnv1a(hash, b"yield"),
        E::TemplateLiteral(_) => update_fnv1a(hash, b"tpl"),
        E::ThisExpression(_) => update_fnv1a(hash, b"this"),
        E::NewExpression(n) => {
            update_fnv1a(hash, b"new(");
            hash_expression(hash, &n.callee);
            update_fnv1a(hash, format!("/{}", n.arguments.len()).as_bytes());
            update_fnv1a(hash, b")");
        }
        _ => update_fnv1a(hash, b"otherexpr"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn hash_first_function(src: &str) -> u64 {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, src, SourceType::default()).parse();
        assert!(parsed.errors.is_empty());
        let mut iter = parsed.program.body.iter().filter_map(|s| {
            if let oxc_ast::ast::Statement::FunctionDeclaration(f) = s {
                Some(f)
            } else {
                None
            }
        });
        let func = iter.next().expect("at least one function");
        compute(func.body.as_ref().expect("function has body"))
    }

    #[test]
    fn ast_hash_collides_for_alpha_renamed_functions() {
        let h1 = hash_first_function("function f(a, b) { return a + b; }");
        let h2 = hash_first_function("function g(x, y) { return x + y; }");
        assert_eq!(h1, h2, "α-renamed equivalents must collide");
    }

    #[test]
    fn ast_hash_differs_for_different_operator() {
        let h1 = hash_first_function("function f(a, b) { return a + b; }");
        let h2 = hash_first_function("function f(a, b) { return a - b; }");
        assert_ne!(h1, h2);
    }

    #[test]
    fn ast_hash_differs_for_different_statement_kind() {
        let h1 = hash_first_function("function f() { return 1; }");
        let h2 = hash_first_function("function f() { let x = 1; }");
        assert_ne!(h1, h2);
    }

    // -- Sugar-strict design intent (regression locks) ------------------------
    //
    // Legacy `reverts/src/engine/equivalence/ast_compare.rs` treats these pairs
    // as EQUIVALENT under its sugar rules. Our `ast` axis is intentionally
    // strict: it hashes the syntactic AST without sugar collapsing, so each
    // pair below must hash DIFFERENTLY. The `cfg` axis is the right place to
    // collide on control-flow sugar — `ast` is not.
    //
    // These reverse assertions pin the design intent: if someone later adds
    // sugar collapsing to `ast::compute` they will see test regressions and
    // be forced to decide whether the change is intentional.

    #[test]
    fn ast_hash_distinguishes_ternary_return_from_if_else_return() {
        // Legacy ast_compare.rs §"Conditional <-> If" considers these equivalent.
        let ternary = hash_first_function("function f(x) { return x ? 1 : 2; }");
        let if_else = hash_first_function("function f(x) { if (x) return 1; return 2; }");
        assert_ne!(
            ternary, if_else,
            "ast axis must remain syntactic; sugar collapsing belongs to cfg axis"
        );
    }

    #[test]
    fn ast_hash_distinguishes_var_let_const_kinds() {
        // Legacy `// Note: var/let/const kind differences are allowed with sugar rules`.
        // Our hasher mixes `format!("{:?}", v.kind)` into the digest so each
        // declarator kind has a distinct hash.
        let var_decl = hash_first_function("function f() { var x = 1; return x; }");
        let let_decl = hash_first_function("function f() { let x = 1; return x; }");
        let const_decl = hash_first_function("function f() { const x = 1; return x; }");
        assert_ne!(var_decl, let_decl);
        assert_ne!(let_decl, const_decl);
        assert_ne!(var_decl, const_decl);
    }

    #[test]
    fn ast_hash_distinguishes_arrow_from_function_expression() {
        // Legacy treats `() => x` and `function () { return x; }` as sugar
        // alternatives. The orchestrator's `ClosureBoundaryAligned` pass may
        // align some IIFE forms, but the raw `ast` axis must distinguish.
        let arrow = hash_first_function("function f() { return () => 1; }");
        let fn_expr = hash_first_function("function f() { return function () { return 1; }; }");
        assert_ne!(arrow, fn_expr);
    }

    #[test]
    fn ast_hash_distinguishes_sequence_from_last_expression() {
        // Legacy "Sequence unwrapping" collapses `(a, b, c)` to its last
        // element when comparing. The raw `ast` axis must keep the comma
        // expression distinct so that bundler-introduced sequencing is
        // visible at this level.
        let seq = hash_first_function("function f(a, b, c) { return (a, b, c); }");
        let last = hash_first_function("function f(a, b, c) { return c; }");
        assert_ne!(seq, last);
    }
}
