use oxc_ast::Visit;
use oxc_ast::ast::{Expression, FunctionBody, TemplateElement};
use reverts_ir::hash::{FNV_OFFSET_BASIS, update_fnv1a};
use std::collections::BTreeSet;

#[must_use]
pub fn compute(body: &FunctionBody<'_>) -> Option<u64> {
    let mut anchors: BTreeSet<String> = BTreeSet::new();
    let mut v = V {
        anchors: &mut anchors,
    };
    for s in &body.statements {
        v.visit_statement(s);
    }
    if anchors.is_empty() {
        return None;
    }
    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, b"lit_anchor|");
    for a in &anchors {
        update_fnv1a(&mut hash, a.as_bytes());
        update_fnv1a(&mut hash, b"|");
    }
    Some(hash)
}

struct V<'a> {
    anchors: &'a mut BTreeSet<String>,
}

impl<'a> Visit<'a> for V<'_> {
    fn visit_expression(&mut self, e: &Expression<'a>) {
        match e {
            Expression::StringLiteral(s) => {
                let trimmed = s.value.as_str().trim();
                if trimmed.len() >= 3 {
                    self.anchors.insert(format!("s:{trimmed}"));
                }
            }
            Expression::RegExpLiteral(r) => {
                self.anchors
                    .insert(format!("r:{}/{}", r.regex.pattern, r.regex.flags));
            }
            Expression::BigIntLiteral(b) => {
                self.anchors.insert(format!("b:{}", b.raw));
            }
            _ => {}
        }
        oxc_ast::visit::walk::walk_expression(self, e);
    }

    fn visit_template_element(&mut self, t: &TemplateElement<'a>) {
        let v = t
            .value
            .cooked
            .as_deref()
            .unwrap_or(t.value.raw.as_str())
            .trim();
        if v.len() >= 3 {
            self.anchors.insert(format!("s:{v}"));
        }
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
    fn literal_anchor_none_for_function_without_literals() {
        assert!(hash_first("function f(a) { return a + 1; }").is_none());
    }

    #[test]
    fn literal_anchor_collects_strings_above_min_len() {
        let h = hash_first("function f() { throw new Error('Unexpected input value'); }");
        assert!(h.is_some());
    }

    #[test]
    fn literal_anchor_drops_short_strings() {
        let h = hash_first("function f() { return 'a'; }");
        assert!(h.is_none());
    }
}
