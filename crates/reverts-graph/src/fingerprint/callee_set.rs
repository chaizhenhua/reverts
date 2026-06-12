use oxc_ast::Visit;
use oxc_ast::ast::{CallExpression, Expression, FunctionBody, NewExpression};
use reverts_ir::hash::fnv1a_of_string_set;
use std::collections::BTreeSet;

/// Compute the callee_set axis with **scope-aware filtering** — local
/// binding names listed in `local_names` are dropped on the assumption
/// that they're minifier-renamed aliases of helpers whose stable
/// identity lives elsewhere. Builtin/method calls (`.toLocaleString`,
/// `Object.assign`, `Number`) are always recorded.
///
/// Passing an empty set behaves identically to a name-blind call:
/// every identifier callee gets recorded.
#[must_use]
pub fn compute_with_locals(body: &FunctionBody<'_>, local_names: &BTreeSet<&str>) -> Option<u64> {
    let mut s: BTreeSet<String> = BTreeSet::new();
    let mut v = V {
        s: &mut s,
        locals: local_names,
    };
    for stmt in &body.statements {
        v.visit_statement(stmt);
    }
    fnv1a_of_string_set(s.iter().map(String::as_str), b"callee|")
}

#[must_use]
pub fn compute(body: &FunctionBody<'_>) -> Option<u64> {
    compute_with_locals(body, &BTreeSet::new())
}

struct V<'a, 'l> {
    s: &'a mut BTreeSet<String>,
    locals: &'l BTreeSet<&'l str>,
}

impl<'a> Visit<'a> for V<'_, '_> {
    fn visit_call_expression(&mut self, c: &CallExpression<'a>) {
        match &c.callee {
            Expression::Identifier(i) => {
                let name = i.name.as_str();
                // Skip locally-bound callee names: in a minified
                // bundle these are unstable aliases of helpers whose
                // real names (`toLocaleString`, `helper`) live in the
                // un-minified source. Recording `c:K92` vs
                // `c:toLocaleString` would diverge the hash for what
                // is the same call structure.
                if !self.locals.contains(name) {
                    self.s.insert(format!("c:{name}"));
                }
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
            // Minifier-stable canonicalisation: for built-in
            // constructors whose `new Foo(...)` / `Foo(...)` invocations
            // are spec-equivalent, record under the call-form tag `c:`
            // so minified `TypeError(msg)` collides with un-minified
            // `new TypeError(msg)`. The list mirrors `ast::is_new_optional_builtin`.
            let name = i.name.as_str();
            if is_new_optional_builtin(name) {
                self.s.insert(format!("c:{name}"));
            } else if !self.locals.contains(name) {
                self.s.insert(format!("nc:{name}"));
            }
        }
        oxc_ast::visit::walk::walk_new_expression(self, n);
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
            | "Boolean"
            | "Number"
            | "String"
            | "Symbol"
            | "BigInt"
            | "Object"
            | "Array"
            | "Function"
            | "RegExp"
            | "Date"
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

    #[test]
    fn callee_set_canonicalises_new_typeerror_to_call_form() {
        let with_new = hash_first("function f() { throw new TypeError('x'); }");
        let without_new = hash_first("function f() { throw TypeError('x'); }");
        assert_eq!(
            with_new, without_new,
            "new TypeError ↔ TypeError must share callee_set"
        );
    }

    #[test]
    fn callee_set_keeps_user_class_under_nc_prefix() {
        let with_new = hash_first("function f() { return new Foo(); }");
        let without_new = hash_first("function f() { return Foo(); }");
        assert_ne!(with_new, without_new, "new Foo ≠ Foo for user classes");
    }
}
