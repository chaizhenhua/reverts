#![allow(dead_code)]

use oxc_ast::Visit;
use oxc_ast::ast::{Expression, FunctionBody, TemplateElement};
use reverts_ir::hash::{FNV_OFFSET_BASIS, update_fnv1a};

#[must_use]
pub fn compute(body: &FunctionBody<'_>) -> Option<u64> {
    let mut counts: [u32; 8] = [0; 8];
    let mut v = V {
        counts: &mut counts,
    };
    for s in &body.statements {
        v.visit_statement(s);
    }
    if counts.iter().sum::<u32>() == 0 {
        return None;
    }
    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, b"lit_shape|");
    for (i, c) in counts.iter().enumerate() {
        update_fnv1a(&mut hash, format!("{i}:{c}|").as_bytes());
    }
    Some(hash)
}

fn string_bucket(len: usize) -> usize {
    match len {
        0..=2 => 0,
        3..=8 => 1,
        9..=32 => 2,
        _ => 3,
    }
}

fn numeric_bucket(n: f64) -> usize {
    if !n.is_finite() {
        7
    } else if n.fract() != 0.0 {
        6
    } else if n.abs() <= 1.0 {
        4
    } else {
        5
    }
}

struct V<'a> {
    counts: &'a mut [u32; 8],
}

impl<'a> Visit<'a> for V<'_> {
    fn visit_expression(&mut self, e: &Expression<'a>) {
        match e {
            Expression::StringLiteral(s) => self.counts[string_bucket(s.value.len())] += 1,
            Expression::NumericLiteral(n) => self.counts[numeric_bucket(n.value)] += 1,
            Expression::BigIntLiteral(_) => self.counts[6] += 1,
            Expression::RegExpLiteral(_) => self.counts[7] += 1,
            _ => {}
        }
        oxc_ast::visit::walk::walk_expression(self, e);
    }

    fn visit_template_element(&mut self, t: &TemplateElement<'a>) {
        let len = t
            .value
            .cooked
            .as_deref()
            .unwrap_or(t.value.raw.as_str())
            .len();
        self.counts[string_bucket(len)] += 1;
        oxc_ast::visit::walk::walk_template_element(self, t);
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
    fn literal_shape_collides_for_same_buckets_different_content() {
        let a = hash_first("function f() { return 'foo'; }");
        let b = hash_first("function g() { return 'bar'; }");
        assert_eq!(a, b);
    }

    #[test]
    fn literal_shape_distinguishes_buckets() {
        let short = hash_first("function f() { return 'foo'; }");
        let long = hash_first("function f() { return 'a-much-longer-string'; }");
        assert_ne!(short, long);
    }
}
