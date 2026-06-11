use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Argument, BindingPatternKind, Expression, FunctionBody, Program, Statement};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_expression;
use oxc_span::SPAN;
use reverts_ir::NormalizationPassId;
use std::collections::BTreeSet;

use super::NormalizationPass;

/// `HelperIdentityInlined` collapses top-level identity-shaped helper functions
/// at their call sites. A function is considered an identity helper when its
/// body is exactly `return <param>;` — i.e. it unconditionally returns its
/// single parameter unchanged. Calls like `_id(42)` are rewritten to `42`.
pub struct HelperIdentityInlined;

impl NormalizationPass for HelperIdentityInlined {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::HelperIdentityInlined
    }

    fn version(&self) -> u32 {
        1
    }

    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let helpers = collect_identity_helpers(program);
        if helpers.is_empty() {
            return;
        }
        let mut inliner = Inliner {
            helpers: &helpers,
            builder: AstBuilder::new(alloc),
        };
        inliner.visit_program(program);
    }
}

/// Scan top-level function declarations and collect names of those whose body
/// is exactly `return <param>;`, confirming the returned identifier matches the
/// sole parameter binding.
fn collect_identity_helpers(program: &Program<'_>) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for stmt in &program.body {
        if let Statement::FunctionDeclaration(func) = stmt
            && let Some(id) = &func.id
            && func.params.items.len() == 1
            && let Some(body) = &func.body
            && is_identity_body(body, &func.params.items[0])
        {
            found.insert(id.name.as_str().to_string());
        }
    }
    found
}

fn is_identity_body<'a>(
    body: &FunctionBody<'a>,
    param: &oxc_ast::ast::FormalParameter<'a>,
) -> bool {
    if body.statements.len() != 1 {
        return false;
    }
    let Statement::ReturnStatement(ret) = &body.statements[0] else {
        return false;
    };
    let Some(Expression::Identifier(ret_id)) = &ret.argument else {
        return false;
    };
    let BindingPatternKind::BindingIdentifier(param_id) = &param.pattern.kind else {
        return false;
    };
    ret_id.name.as_str() == param_id.name.as_str()
}

struct Inliner<'a, 'h> {
    helpers: &'h BTreeSet<String>,
    builder: AstBuilder<'a>,
}

impl<'a> VisitMut<'a> for Inliner<'a, '_> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        // Bottom-up: recurse into children before inspecting the current node.
        walk_expression(self, expr);

        let is_inlinable = matches!(
            expr,
            Expression::CallExpression(call)
                if call.arguments.len() == 1
                    && !matches!(call.arguments[0], Argument::SpreadElement(_))
                    && matches!(
                        &call.callee,
                        Expression::Identifier(id) if self.helpers.contains(id.name.as_str())
                    )
        );
        if !is_inlinable {
            return;
        }

        // Swap out the call with a dummy so we can take ownership and extract
        // the single argument by value.
        let dummy = self.builder.expression_null_literal(SPAN);
        let taken = std::mem::replace(expr, dummy);
        if let Expression::CallExpression(call) = taken {
            let mut call = call.unbox();
            if let Some(first_arg) = call.arguments.drain(..).next() {
                // `into_expression` consumes the Argument and yields its inner
                // Expression (panics on SpreadElement, but we checked above).
                *expr = first_arg.into_expression();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn identity_helper_call_is_inlined() {
        let src = "function _id(x) { return x; }\nlet v = _id(42);";
        let out = apply_to_source(&HelperIdentityInlined, src).expect("parses");
        assert!(out.contains("v = 42") || out.contains("v=42"), "got: {out}");
    }

    #[test]
    fn non_identity_helper_is_left_alone() {
        let src = "function adds(x) { return x + 1; }\nlet v = adds(2);";
        let out = apply_to_source(&HelperIdentityInlined, src).unwrap();
        assert!(out.contains("adds("));
    }
}
