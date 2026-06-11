use oxc_ast::Visit;
use oxc_ast::ast::{Expression, FunctionBody, Statement};
use reverts_ir::hash::{FNV_OFFSET_BASIS, update_fnv1a};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum ReturnKind {
    Void,
    Literal,
    Identifier,
    MemberChain,
    Call,
    Conditional,
    Await,
    Other,
}

#[must_use]
pub fn compute(body: &FunctionBody<'_>) -> u64 {
    let mut counts: BTreeMap<ReturnKind, u32> = BTreeMap::new();
    let mut visitor = V {
        counts: &mut counts,
    };
    for s in &body.statements {
        visitor.visit_statement(s);
    }

    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, b"ret|");
    for (kind, count) in counts {
        update_fnv1a(&mut hash, format!("{kind:?}={count}|").as_bytes());
    }
    hash
}

fn classify(expr: &Expression<'_>) -> ReturnKind {
    use Expression as E;
    match expr {
        E::StringLiteral(_)
        | E::NumericLiteral(_)
        | E::BooleanLiteral(_)
        | E::NullLiteral(_)
        | E::RegExpLiteral(_)
        | E::TemplateLiteral(_) => ReturnKind::Literal,
        E::Identifier(_) => ReturnKind::Identifier,
        E::StaticMemberExpression(_) | E::ComputedMemberExpression(_) => ReturnKind::MemberChain,
        E::CallExpression(_) | E::NewExpression(_) => ReturnKind::Call,
        E::ConditionalExpression(_) => ReturnKind::Conditional,
        E::AwaitExpression(_) => ReturnKind::Await,
        _ => ReturnKind::Other,
    }
}

struct V<'c> {
    counts: &'c mut BTreeMap<ReturnKind, u32>,
}

impl<'a> Visit<'a> for V<'_> {
    fn visit_statement(&mut self, s: &Statement<'a>) {
        if let Statement::ReturnStatement(r) = s {
            let kind = match &r.argument {
                Some(a) => classify(a),
                None => ReturnKind::Void,
            };
            *self.counts.entry(kind).or_default() += 1;
        }
        oxc_ast::visit::walk::walk_statement(self, s);
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
    fn return_pattern_distinguishes_void_from_value() {
        assert_ne!(
            hash_first("function f() { return; }"),
            hash_first("function f() { return 1; }"),
        );
    }

    #[test]
    fn return_pattern_collides_for_same_bucket() {
        assert_eq!(
            hash_first("function f(a) { return a.x.y; }"),
            hash_first("function f(z) { return z.q.r; }"),
        );
    }
}
