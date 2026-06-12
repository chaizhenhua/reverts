use oxc_ast::Visit;
use oxc_ast::ast::{Expression, FunctionBody, Statement};
use reverts_ir::hash::fnv1a_of_string_set;
use std::collections::BTreeSet;

#[must_use]
pub fn compute(body: &FunctionBody<'_>) -> Option<u64> {
    let mut s: BTreeSet<String> = BTreeSet::new();
    let mut v = V { s: &mut s };
    for stmt in &body.statements {
        v.visit_statement(stmt);
    }
    fnv1a_of_string_set(s.iter().map(String::as_str), b"throw|")
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
                // Minifier-stable canonicalisation: `throw TypeError(m)`
                // is the minified form of `throw new TypeError(m)` for
                // built-in error constructors (semantically equivalent
                // per the spec). Record it under the same `n:<name>`
                // tag as the `new`-form so both versions collide.
                Expression::CallExpression(c) => {
                    if let Expression::Identifier(i) = &c.callee
                        && is_new_optional_builtin(i.name.as_str())
                    {
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

fn is_new_optional_builtin(name: &str) -> bool {
    matches!(
        name,
        "Error"
            | "TypeError"
            | "RangeError"
            | "ReferenceError"
            | "SyntaxError"
            | "URIError"
            | "EvalError"
            | "AggregateError"
    )
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

    #[test]
    fn throw_set_canonicalises_throw_call_builtin_to_throw_new_builtin() {
        let with_new = hash_first("function f() { throw new TypeError('x'); }");
        let without_new = hash_first("function f() { throw TypeError('x'); }");
        assert_eq!(
            with_new, without_new,
            "throw TypeError ↔ throw new TypeError must share throw_set"
        );
    }
}
