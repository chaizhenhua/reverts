use oxc_ast::Visit;
use oxc_ast::ast::{CallExpression, Expression, FunctionBody, NewExpression};
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
    update_fnv1a(&mut hash, b"callee|");
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
    fn visit_call_expression(&mut self, c: &CallExpression<'a>) {
        match &c.callee {
            Expression::Identifier(i) => {
                self.s.insert(format!("c:{}", i.name.as_str()));
            }
            Expression::StaticMemberExpression(m) => {
                self.s.insert(format!("cm:.{}", m.property.name.as_str()));
            }
            _ => {}
        }
        oxc_ast::visit::walk::walk_call_expression(self, c);
    }

    fn visit_new_expression(&mut self, n: &NewExpression<'a>) {
        if let Expression::Identifier(i) = &n.callee {
            self.s.insert(format!("nc:{}", i.name.as_str()));
        }
        oxc_ast::visit::walk::walk_new_expression(self, n);
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
    fn callee_set_keeps_static_member_names() {
        let a = hash_first("function f(o) { o.toString(); }");
        let b = hash_first("function f(o) { o.toJSON(); }");
        assert_ne!(a, b);
    }

    #[test]
    fn callee_set_collides_for_same_callees_different_receivers() {
        let a = hash_first("function f(o) { o.push(1); }");
        let b = hash_first("function f(x) { x.push(1); }");
        assert_eq!(a, b);
    }
}
