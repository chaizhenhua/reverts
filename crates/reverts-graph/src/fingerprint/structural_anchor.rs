use oxc_ast::Visit;
use oxc_ast::ast::{BindingPatternKind, Expression, FormalParameters, FunctionBody, Statement};
use reverts_ir::hash::{FNV_OFFSET_BASIS, update_fnv1a};

#[derive(Debug)]
struct Counts {
    param_destructure_depth: u32,
    await_count: u32,
    yield_count: u32,
    throw_count: u32,
    try_handler_count: u32,
    for_count: u32,
    for_in_count: u32,
    for_of_count: u32,
    while_count: u32,
    do_while_count: u32,
    return_value_count: u32,
    return_void_count: u32,
    switch_case_count: u32,
}

impl Counts {
    fn new(params: &FormalParameters<'_>) -> Self {
        Self {
            param_destructure_depth: max_destructure_depth(params),
            await_count: 0,
            yield_count: 0,
            throw_count: 0,
            try_handler_count: 0,
            for_count: 0,
            for_in_count: 0,
            for_of_count: 0,
            while_count: 0,
            do_while_count: 0,
            return_value_count: 0,
            return_void_count: 0,
            switch_case_count: 0,
        }
    }
}

#[must_use]
pub fn compute(params: &FormalParameters<'_>, body: &FunctionBody<'_>) -> u64 {
    let mut counts = Counts::new(params);
    let mut visitor = Visitor {
        counts: &mut counts,
    };
    for stmt in &body.statements {
        visitor.visit_statement(stmt);
    }

    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, b"structural|");
    macro_rules! mix {
        ($name:literal, $field:expr) => {
            update_fnv1a(&mut hash, $name);
            update_fnv1a(&mut hash, b"=");
            update_fnv1a(&mut hash, $field.to_string().as_bytes());
            update_fnv1a(&mut hash, b"|");
        };
    }
    mix!(b"pdd", counts.param_destructure_depth);
    mix!(b"aw", counts.await_count);
    mix!(b"yi", counts.yield_count);
    mix!(b"th", counts.throw_count);
    mix!(b"tr", counts.try_handler_count);
    mix!(b"for", counts.for_count);
    mix!(b"fin", counts.for_in_count);
    mix!(b"fof", counts.for_of_count);
    mix!(b"wh", counts.while_count);
    mix!(b"dw", counts.do_while_count);
    mix!(b"rv", counts.return_value_count);
    mix!(b"r0", counts.return_void_count);
    mix!(b"sw", counts.switch_case_count);
    hash
}

fn max_destructure_depth(params: &FormalParameters<'_>) -> u32 {
    let mut max = 0;
    for param in &params.items {
        max = max.max(pattern_depth(&param.pattern.kind, 0));
    }
    max
}

fn pattern_depth(kind: &BindingPatternKind<'_>, depth: u32) -> u32 {
    match kind {
        BindingPatternKind::ObjectPattern(o) => {
            let mut d = depth + 1;
            for prop in &o.properties {
                d = d.max(pattern_depth(&prop.value.kind, depth + 1));
            }
            d
        }
        BindingPatternKind::ArrayPattern(a) => {
            let mut d = depth + 1;
            for e in (&a.elements).into_iter().flatten() {
                d = d.max(pattern_depth(&e.kind, depth + 1));
            }
            d
        }
        _ => depth,
    }
}

struct Visitor<'c> {
    counts: &'c mut Counts,
}

impl<'a> Visit<'a> for Visitor<'_> {
    fn visit_statement(&mut self, stmt: &Statement<'a>) {
        match stmt {
            Statement::ReturnStatement(r) => {
                if r.argument.is_some() {
                    self.counts.return_value_count += 1;
                } else {
                    self.counts.return_void_count += 1;
                }
            }
            Statement::ThrowStatement(_) => self.counts.throw_count += 1,
            Statement::ForStatement(_) => self.counts.for_count += 1,
            Statement::ForInStatement(_) => self.counts.for_in_count += 1,
            Statement::ForOfStatement(_) => self.counts.for_of_count += 1,
            Statement::WhileStatement(_) => self.counts.while_count += 1,
            Statement::DoWhileStatement(_) => self.counts.do_while_count += 1,
            Statement::SwitchStatement(s) => {
                self.counts.switch_case_count += s.cases.len() as u32;
            }
            Statement::TryStatement(t) => {
                if t.handler.is_some() {
                    self.counts.try_handler_count += 1;
                }
            }
            _ => {}
        }
        oxc_ast::visit::walk::walk_statement(self, stmt);
    }

    fn visit_expression(&mut self, expr: &Expression<'a>) {
        if matches!(expr, Expression::AwaitExpression(_)) {
            self.counts.await_count += 1;
        }
        if matches!(expr, Expression::YieldExpression(_)) {
            self.counts.yield_count += 1;
        }
        oxc_ast::visit::walk::walk_expression(self, expr);
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
        compute(&func.params, func.body.as_ref().expect("body"))
    }

    #[test]
    fn structural_anchor_distinguishes_loop_kinds() {
        let f = hash_first("function f(xs) { for (let x of xs) {} }");
        let w = hash_first("function f(xs) { while (xs.shift()) {} }");
        assert_ne!(f, w);
    }

    #[test]
    fn structural_anchor_collides_after_identifier_rename() {
        let a = hash_first("function f(a) { try { return a; } catch(e) { throw e; } }");
        let b = hash_first("function g(x) { try { return x; } catch(z) { throw z; } }");
        assert_eq!(a, b);
    }

    #[test]
    fn structural_anchor_counts_destructure_depth() {
        let flat = hash_first("function f(a) { return a; }");
        let deep = hash_first("function f({ a: { b } }) { return b; }");
        assert_ne!(flat, deep);
    }
}
