use oxc_ast::Visit;
use oxc_ast::ast::{Expression, FunctionBody};
use reverts_ir::hash::{FNV_OFFSET_BASIS, update_fnv1a};
use std::collections::BTreeSet;

#[derive(Debug, Default)]
struct Collector {
    pattern: BTreeSet<String>,
    shape: BTreeSet<String>,
}

#[must_use]
pub fn compute(body: &FunctionBody<'_>) -> (Option<u64>, Option<u64>) {
    let mut c = Collector::default();
    let mut v = V { c: &mut c };
    for s in &body.statements {
        v.visit_statement(s);
    }
    (
        hash_set(&c.pattern, b"acc_pat|"),
        hash_set(&c.shape, b"acc_shape|"),
    )
}

fn hash_set(set: &BTreeSet<String>, tag: &[u8]) -> Option<u64> {
    if set.is_empty() {
        return None;
    }
    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, tag);
    for k in set {
        update_fnv1a(&mut hash, k.as_bytes());
        update_fnv1a(&mut hash, b"|");
    }
    Some(hash)
}

struct V<'a> {
    c: &'a mut Collector,
}

impl<'a> Visit<'a> for V<'_> {
    fn visit_expression(&mut self, e: &Expression<'a>) {
        match e {
            Expression::StaticMemberExpression(m) => {
                let depth = chain_depth(e);
                self.c
                    .pattern
                    .insert(format!("s:{}@{depth}", m.property.name.as_str()));
                self.c.shape.insert(format!("s@{depth}"));
            }
            Expression::ComputedMemberExpression(_) => {
                let depth = chain_depth(e);
                self.c.pattern.insert(format!("c@{depth}"));
                self.c.shape.insert(format!("c@{depth}"));
            }
            _ => {}
        }
        oxc_ast::visit::walk::walk_expression(self, e);
    }
}

fn chain_depth(e: &Expression<'_>) -> u32 {
    fn inner(e: &Expression<'_>, depth: u32) -> u32 {
        match e {
            Expression::StaticMemberExpression(m) => inner(&m.object, depth + 1),
            Expression::ComputedMemberExpression(m) => inner(&m.object, depth + 1),
            _ => depth,
        }
    }
    inner(e, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn run(src: &str) -> (Option<u64>, Option<u64>) {
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
    fn access_pattern_keeps_property_names_shape_drops_them() {
        let (p1, s1) = run("function f(o) { return o.foo; }");
        let (p2, s2) = run("function f(o) { return o.bar; }");
        assert_ne!(p1, p2, "pattern must differ on property name");
        assert_eq!(s1, s2, "shape must collide regardless of property name");
    }

    #[test]
    fn access_returns_none_when_no_member_access() {
        let (p, s) = run("function f(a, b) { return a + b; }");
        assert!(p.is_none());
        assert!(s.is_none());
    }
}
