#![allow(dead_code)]

use oxc_ast::Visit;
use oxc_ast::ast::{Expression, FunctionBody, Statement};
use reverts_ir::hash::{FNV_OFFSET_BASIS, update_fnv1a};
use std::collections::BTreeSet;

#[must_use]
pub fn compute(body: &FunctionBody<'_>) -> Option<u64> {
    let mut s: BTreeSet<String> = BTreeSet::new();
    let mut v = V { s: &mut s };
    for stmt in &body.statements {
        v.visit_statement(stmt);
    }
    if s.is_empty() {
        return None;
    }
    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, b"throw|");
    for k in &s {
        update_fnv1a(&mut hash, k.as_bytes());
        update_fnv1a(&mut hash, b"|");
    }
    Some(hash)
}

struct V<'a> {
    s: &'a mut BTreeSet<String>,
}

impl<'a> Visit<'a> for V<'_> {
    fn visit_statement(&mut self, stmt: &Statement<'a>) {
        if let Statement::ThrowStatement(t) = stmt {
            match &t.argument {
                Expression::NewExpression(n) => {
                    if let Expression::Identifier(i) = &n.callee {
                        self.s.insert(format!("n:{}", i.name.as_str()));
                    } else {
                        self.s.insert("t:expr".to_string());
                    }
                }
                Expression::StringLiteral(_) | Expression::NumericLiteral(_) => {
                    self.s.insert("t:lit".to_string());
                }
                _ => {
                    self.s.insert("t:expr".to_string());
                }
            }
        }
        oxc_ast::visit::walk::walk_statement(self, stmt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn hash_first(src: &str) -> Option<u64> {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, src, SourceType::default()).parse();
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
    fn throw_set_distinguishes_type_vs_range_error() {
        let a = hash_first("function f() { throw new TypeError('x'); }");
        let b = hash_first("function f() { throw new RangeError('x'); }");
        assert_ne!(a, b);
    }

    #[test]
    fn throw_set_collides_for_same_constructor() {
        let a = hash_first("function f(e) { throw new Error('a'); }");
        let b = hash_first("function f(x) { throw new Error('b'); }");
        assert_eq!(a, b);
    }
}
