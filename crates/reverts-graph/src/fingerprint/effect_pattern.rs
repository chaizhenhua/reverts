use oxc_ast::Visit;
use oxc_ast::ast::{
    AssignmentExpression, AssignmentTarget, AwaitExpression, CallExpression, FunctionBody,
    NewExpression, Statement, YieldExpression,
};
use reverts_ir::hash::{FNV_OFFSET_BASIS, update_fnv1a};

#[derive(Default)]
struct Counts {
    call: u32,
    member_write: u32,
    awaits: u32,
    yields: u32,
    throws: u32,
}

#[must_use]
pub fn compute(body: &FunctionBody<'_>) -> u64 {
    let mut counts = Counts::default();
    let mut v = V {
        counts: &mut counts,
    };
    for s in &body.statements {
        v.visit_statement(s);
    }
    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, b"effect|");
    update_fnv1a(&mut hash, format!("c={}|", counts.call).as_bytes());
    update_fnv1a(&mut hash, format!("mw={}|", counts.member_write).as_bytes());
    update_fnv1a(&mut hash, format!("aw={}|", counts.awaits).as_bytes());
    update_fnv1a(&mut hash, format!("yi={}|", counts.yields).as_bytes());
    update_fnv1a(&mut hash, format!("th={}|", counts.throws).as_bytes());
    hash
}

struct V<'c> {
    counts: &'c mut Counts,
}

impl<'a> Visit<'a> for V<'_> {
    fn visit_call_expression(&mut self, c: &CallExpression<'a>) {
        self.counts.call += 1;
        oxc_ast::visit::walk::walk_call_expression(self, c);
    }
    fn visit_new_expression(&mut self, n: &NewExpression<'a>) {
        // `new X(...)` is intentionally NOT counted in the `call`
        // bucket. Previously it was, on the rationale that the two
        // forms invoke a function the same way. The collapse was
        // reverted because `Foo(...)` and `new Foo(...)` are not
        // strictly spec-equivalent — when `Foo` is shadowed by an
        // arrow function, `new Foo()` throws TypeError while
        // `Foo()` does not.
        oxc_ast::visit::walk::walk_new_expression(self, n);
    }
    fn visit_assignment_expression(&mut self, e: &AssignmentExpression<'a>) {
        if matches!(
            &e.left,
            AssignmentTarget::StaticMemberExpression(_)
                | AssignmentTarget::ComputedMemberExpression(_)
        ) {
            self.counts.member_write += 1;
        }
        oxc_ast::visit::walk::walk_assignment_expression(self, e);
    }
    fn visit_await_expression(&mut self, a: &AwaitExpression<'a>) {
        self.counts.awaits += 1;
        oxc_ast::visit::walk::walk_await_expression(self, a);
    }
    fn visit_yield_expression(&mut self, y: &YieldExpression<'a>) {
        self.counts.yields += 1;
        oxc_ast::visit::walk::walk_yield_expression(self, y);
    }
    fn visit_statement(&mut self, s: &Statement<'a>) {
        if matches!(s, Statement::ThrowStatement(_)) {
            self.counts.throws += 1;
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
    fn effect_pattern_distinguishes_call_heavy_from_pure() {
        let pure_fn = hash_first("function f(a, b) { return a + b; }");
        let call_fn = hash_first("function f() { console.log(1); console.log(2); }");
        assert_ne!(pure_fn, call_fn);
    }

    #[test]
    fn effect_pattern_collides_under_identifier_rename() {
        let a = hash_first("function f(a) { console.log(a); throw new Error('x'); }");
        let b = hash_first("function g(z) { console.log(z); throw new Error('y'); }");
        assert_eq!(a, b);
    }

    #[test]
    fn effect_pattern_distinguishes_new_versus_call_invocation() {
        // Previously these collided on the assumption `new X` and `X`
        // invoke the same function. The pass policy is now "no
        // transform without full equivalence", and the two are not
        // equivalent under arrow-function shadowing.
        let with_new = hash_first("function f() { throw new TypeError('x'); }");
        let without_new = hash_first("function f() { throw TypeError('x'); }");
        assert_ne!(with_new, without_new);
    }
}
