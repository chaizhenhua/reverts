use oxc_ast::Visit;
use oxc_ast::ast::{Expression, FunctionBody, Statement};
use reverts_ir::hash::fnv1a_of_string_set;
use std::collections::BTreeSet;

/// Compute the throw_set axis with **scope-aware filtering** — when
/// `throw new X(...)` (or `throw X(...)`) names a local binding listed
/// in `local_names`, the constructor identifier is folded under the
/// generic `n:LOCAL` / `t:LOCAL` bucket. This mirrors `callee_set`:
/// minifiers rename local bindings to short single-letter names, so
/// hashing on the literal name diverges minified vs un-minified output
/// for what is the same throw shape.
///
/// Builtin/global constructor names (`TypeError`, user-imported names,
/// etc.) are recorded under `n:<name>` so distinct error types still
/// hash apart.
///
/// Passing an empty set behaves identically to a name-blind compute:
/// every identifier constructor gets recorded under its literal name.
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
    fnv1a_of_string_set(s.iter().map(String::as_str), b"throw|")
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
    fn visit_statement(&mut self, stmt: &Statement<'a>) {
        if let Statement::ThrowStatement(t) = stmt {
            match &t.argument {
                Expression::NewExpression(n) => {
                    if let Expression::Identifier(i) = &n.callee {
                        let name = i.name.as_str();
                        if self.locals.contains(name) {
                            self.s.insert("n:LOCAL".to_string());
                        } else {
                            self.s.insert(format!("n:{name}"));
                        }
                    } else {
                        self.s.insert("t:expr".to_string());
                    }
                }
                // `throw Foo(...)` (call-form) stays under the
                // generic `t:expr` bucket. Previously it was folded
                // into `n:Foo` for built-in error constructors on the
                // assumption that `Foo()` and `new Foo()` are
                // equivalent for Errors; the collapse was reverted
                // because the built-in name is shadowable and the
                // two forms are not strictly spec-equivalent under
                // arrow-function shadowing.
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

    fn hash_first_with_locals(src: &str, locals: &[&str]) -> Option<u64> {
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
        let set: BTreeSet<&str> = locals.iter().copied().collect();
        compute_with_locals(func.body.as_ref().expect("body"), &set)
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
    fn throw_set_distinguishes_throw_call_builtin_from_throw_new_builtin() {
        // `throw Foo()` (call) and `throw new Foo()` (construct) are
        // not strictly spec-equivalent under arrow-function shadowing
        // of `Foo`. The two forms therefore hash separately.
        let with_new = hash_first("function f() { throw new TypeError('x'); }");
        let without_new = hash_first("function f() { throw TypeError('x'); }");
        assert_ne!(with_new, without_new);
    }

    #[test]
    fn throw_set_collapses_local_constructor_names() {
        // `throw new K(...)` where K is a local param maps to the same
        // bucket regardless of the local's literal name — the
        // alternate name is unstable across minifier output.
        let a = hash_first_with_locals("function f(MyErr) { throw new MyErr('x'); }", &["MyErr"]);
        let b = hash_first_with_locals("function f(K) { throw new K('x'); }", &["K"]);
        assert_eq!(a, b, "throw-new of local param must collapse");
    }

    #[test]
    fn throw_set_keeps_global_constructor_distinct_from_local() {
        // `throw new TypeError(...)` (global) must NOT hash the same as
        // `throw new K(...)` (local) — the two name different things.
        let global = hash_first("function f() { throw new TypeError('x'); }");
        let local = hash_first_with_locals("function f(K) { throw new K('x'); }", &["K"]);
        assert_ne!(global, local);
    }
}
