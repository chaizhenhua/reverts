#![allow(dead_code)]

use oxc_ast::Visit;
use oxc_ast::ast::{
    BindingPattern, BindingPatternKind, FormalParameters, FunctionBody, Statement,
    VariableDeclarator,
};
use reverts_ir::hash::{FNV_OFFSET_BASIS, update_fnv1a};
use std::collections::BTreeSet;

#[must_use]
pub fn compute(params: &FormalParameters<'_>, body: &FunctionBody<'_>) -> u64 {
    let mut tokens: BTreeSet<String> = BTreeSet::new();
    for param in &params.items {
        tokens.insert(param_token(param));
    }
    let mut v = V {
        tokens: &mut tokens,
    };
    for s in &body.statements {
        v.visit_statement(s);
    }
    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, b"binding|");
    for t in &tokens {
        update_fnv1a(&mut hash, t.as_bytes());
        update_fnv1a(&mut hash, b"|");
    }
    hash
}

fn param_token(param: &oxc_ast::ast::FormalParameter<'_>) -> String {
    let core = pattern_token(&param.pattern, 'p');
    // FormalParameter has no `default` field in oxc 0.42 — defaults are
    // encoded as AssignmentPattern inside pattern.kind. Treat +d if the
    // pattern kind is AssignmentPattern.
    if matches!(
        &param.pattern.kind,
        BindingPatternKind::AssignmentPattern(_)
    ) {
        format!("{core}+d")
    } else {
        core
    }
}

fn pattern_token(pat: &BindingPattern<'_>, prefix: char) -> String {
    match &pat.kind {
        BindingPatternKind::BindingIdentifier(_) => format!("{prefix}:i"),
        BindingPatternKind::ObjectPattern(o) => format!("{prefix}:o[{}]", o.properties.len()),
        BindingPatternKind::ArrayPattern(a) => format!("{prefix}:a[{}]", a.elements.len()),
        BindingPatternKind::AssignmentPattern(a) => pattern_token(&a.left, prefix),
    }
}

struct V<'a> {
    tokens: &'a mut BTreeSet<String>,
}

impl<'a> Visit<'a> for V<'_> {
    fn visit_statement(&mut self, s: &Statement<'a>) {
        if let Statement::VariableDeclaration(v) = s {
            for d in &v.declarations {
                self.tokens.insert(declarator_token(d));
            }
        }
        oxc_ast::visit::walk::walk_statement(self, s);
    }
}

fn declarator_token(d: &VariableDeclarator<'_>) -> String {
    pattern_token(&d.id, 'l')
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
        compute(&func.params, func.body.as_ref().expect("body"))
    }

    #[test]
    fn binding_pattern_collides_under_identifier_rename() {
        let a = hash_first("function f(a, b) { let x = 1; return a + b + x; }");
        let b = hash_first("function g(p, q) { let y = 1; return p + q + y; }");
        assert_eq!(a, b);
    }

    #[test]
    fn binding_pattern_distinguishes_destructure_from_identifier() {
        let flat = hash_first("function f(a) { return a; }");
        let obj = hash_first("function f({ a, b }) { return a + b; }");
        assert_ne!(flat, obj);
    }
}
