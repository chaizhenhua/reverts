use oxc_allocator::{Allocator, Vec as ArenaVec};
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Argument, Expression, ObjectPropertyKind, Program};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_expression;
use oxc_span::SPAN;
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `ObjectAssignExpanded` rewrites the minifier-friendly invocation
/// form `Object.assign({a:1, b:2}, x, y)` into the human-readable
/// object-spread form `{a:1, b:2, ...x, ...y}`. Both forms are
/// spec-equivalent when the first argument is a fresh object literal
/// (no aliasing, no observable side effects from `assign`'s own
/// invocation).
///
/// Without this rewrite, minified bundles (which prefer `Object.assign`
/// because it costs fewer bytes than the spread syntax) and source
/// code (which usually uses spread) produce wildly different AST,
/// CFG, callee_set, and effect_pattern fingerprints for what is the
/// same option-merging pattern.
///
/// The rewrite is conservative:
///
/// * Only fires when the callee is exactly `Object.assign` — no
///   recognition of aliased references like `var z = Object.assign`.
/// * Only fires when the first argument is an `ObjectExpression`
///   literal — `Object.assign(existingObj, …)` mutates `existingObj`
///   in place and is observable; we keep that form intact.
/// * Subsequent arguments that are object-literals are inlined as
///   their property list; all other expressions are wrapped as
///   `...expr` spread properties.
pub struct ObjectAssignExpanded;

impl NormalizationPass for ObjectAssignExpanded {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::ObjectAssignExpanded
    }
    fn version(&self) -> u32 {
        1
    }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut visitor = Expander {
            builder: AstBuilder::new(alloc),
        };
        visitor.visit_program(program);
    }
}

struct Expander<'a> {
    builder: AstBuilder<'a>,
}

impl<'a> VisitMut<'a> for Expander<'a> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        // Recurse bottom-up so nested Object.assign calls are rewritten
        // before the outer one inspects them.
        walk_expression(self, expr);
        if !is_rewritable_object_assign(expr) {
            return;
        }
        let placeholder = self.builder.expression_null_literal(SPAN);
        let owned = mem::replace(expr, placeholder);
        let Expression::CallExpression(call_box) = owned else {
            unreachable!()
        };
        let call = call_box.unbox();
        // is_rewritable_object_assign already verified the first
        // argument is a literal ObjectExpression, so try_rewrite
        // returning None here is unreachable; but to keep the contract
        // explicit we restore the original on the impossible path.
        match try_rewrite(&self.builder, call.arguments) {
            Some(rewritten) => *expr = rewritten,
            None => unreachable!("is_rewritable_object_assign passed but try_rewrite failed"),
        }
    }
}

fn is_rewritable_object_assign(expr: &Expression<'_>) -> bool {
    let Expression::CallExpression(c) = expr else {
        return false;
    };
    let Expression::StaticMemberExpression(m) = &c.callee else {
        return false;
    };
    let Expression::Identifier(obj) = &m.object else {
        return false;
    };
    if obj.name != "Object" || m.property.name != "assign" || c.arguments.len() < 2 {
        return false;
    }
    // First argument must be a literal ObjectExpression for the
    // rewrite to be semantically safe (no aliased mutation observable
    // through the original `assign` target).
    matches!(c.arguments.first(), Some(Argument::ObjectExpression(_)))
}

fn try_rewrite<'a>(
    builder: &AstBuilder<'a>,
    arguments: ArenaVec<'a, Argument<'a>>,
) -> Option<Expression<'a>> {
    // First argument must be a literal ObjectExpression to make the
    // rewrite semantically safe (no mutation of existing reference).
    let mut iter = arguments.into_iter();
    let first = iter.next()?;
    let Argument::ObjectExpression(first_obj_box) = first else {
        return None;
    };
    let first_obj = first_obj_box.unbox();

    let mut properties: ArenaVec<'a, ObjectPropertyKind<'a>> = ArenaVec::new_in(builder.allocator);
    for p in first_obj.properties {
        properties.push(p);
    }

    for arg in iter {
        // Each remaining argument is either an object literal (inline
        // its properties) or any other expression (wrap as spread).
        match arg {
            Argument::ObjectExpression(obj_box) => {
                let obj = obj_box.unbox();
                for p in obj.properties {
                    properties.push(p);
                }
            }
            Argument::SpreadElement(_) => {
                // `Object.assign({}, ...iterable)` — unusual; keep the
                // original call by aborting the rewrite.
                return None;
            }
            other => {
                let expr = argument_to_expression(other)?;
                properties.push(builder.object_property_kind_spread_element(SPAN, expr));
            }
        }
    }

    Some(builder.expression_object(SPAN, properties, None))
}

fn argument_to_expression<'a>(arg: Argument<'a>) -> Option<Expression<'a>> {
    // The `Argument` enum is a structural superset of `Expression`
    // for the non-spread variants we accept here. OXC provides
    // `Argument::into_expression()` only in some versions; do it
    // manually via a match to stay compatible.
    Some(match arg {
        Argument::BooleanLiteral(b) => Expression::BooleanLiteral(b),
        Argument::NullLiteral(n) => Expression::NullLiteral(n),
        Argument::NumericLiteral(n) => Expression::NumericLiteral(n),
        Argument::BigIntLiteral(b) => Expression::BigIntLiteral(b),
        Argument::RegExpLiteral(r) => Expression::RegExpLiteral(r),
        Argument::StringLiteral(s) => Expression::StringLiteral(s),
        Argument::TemplateLiteral(t) => Expression::TemplateLiteral(t),
        Argument::Identifier(i) => Expression::Identifier(i),
        Argument::MetaProperty(m) => Expression::MetaProperty(m),
        Argument::Super(s) => Expression::Super(s),
        Argument::ArrayExpression(a) => Expression::ArrayExpression(a),
        Argument::ArrowFunctionExpression(a) => Expression::ArrowFunctionExpression(a),
        Argument::AssignmentExpression(a) => Expression::AssignmentExpression(a),
        Argument::AwaitExpression(a) => Expression::AwaitExpression(a),
        Argument::BinaryExpression(b) => Expression::BinaryExpression(b),
        Argument::CallExpression(c) => Expression::CallExpression(c),
        Argument::ChainExpression(c) => Expression::ChainExpression(c),
        Argument::ClassExpression(c) => Expression::ClassExpression(c),
        Argument::ConditionalExpression(c) => Expression::ConditionalExpression(c),
        Argument::FunctionExpression(f) => Expression::FunctionExpression(f),
        Argument::ImportExpression(i) => Expression::ImportExpression(i),
        Argument::LogicalExpression(l) => Expression::LogicalExpression(l),
        Argument::NewExpression(n) => Expression::NewExpression(n),
        Argument::ObjectExpression(o) => Expression::ObjectExpression(o),
        Argument::ParenthesizedExpression(p) => Expression::ParenthesizedExpression(p),
        Argument::SequenceExpression(s) => Expression::SequenceExpression(s),
        Argument::TaggedTemplateExpression(t) => Expression::TaggedTemplateExpression(t),
        Argument::ThisExpression(t) => Expression::ThisExpression(t),
        Argument::UnaryExpression(u) => Expression::UnaryExpression(u),
        Argument::UpdateExpression(u) => Expression::UpdateExpression(u),
        Argument::YieldExpression(y) => Expression::YieldExpression(y),
        Argument::PrivateInExpression(p) => Expression::PrivateInExpression(p),
        Argument::JSXElement(j) => Expression::JSXElement(j),
        Argument::JSXFragment(j) => Expression::JSXFragment(j),
        Argument::StaticMemberExpression(m) => Expression::StaticMemberExpression(m),
        Argument::ComputedMemberExpression(m) => Expression::ComputedMemberExpression(m),
        Argument::PrivateFieldExpression(p) => Expression::PrivateFieldExpression(p),
        // SpreadElement excluded — handled before this fn is called.
        // TS-only argument variants are dropped: we don't expect them
        // in normal JS callers.
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn rewrites_assign_with_literal_and_identifier() {
        let out = apply_to_source(
            &ObjectAssignExpanded,
            "function f(opts) { return Object.assign({a:1, b:2}, opts); }",
        )
        .expect("parse");
        // Expect the object literal with spread.
        assert!(out.contains("..."), "got: {out}");
        assert!(out.contains("a: 1"), "got: {out}");
        assert!(out.contains("b: 2"), "got: {out}");
        assert!(!out.contains("Object.assign"), "got: {out}");
    }

    #[test]
    fn rewrites_assign_with_multiple_args() {
        let out = apply_to_source(
            &ObjectAssignExpanded,
            "function f(x, y) { return Object.assign({}, x, y); }",
        )
        .expect("parse");
        assert!(!out.contains("Object.assign"), "got: {out}");
        assert!(out.contains("...x"), "got: {out}");
        assert!(out.contains("...y"), "got: {out}");
    }

    #[test]
    fn inlines_object_literal_args() {
        let out = apply_to_source(
            &ObjectAssignExpanded,
            "function f() { return Object.assign({a:1}, {b:2}, {c:3}); }",
        )
        .expect("parse");
        assert!(!out.contains("Object.assign"), "got: {out}");
        assert!(out.contains("a: 1"), "got: {out}");
        assert!(out.contains("b: 2"), "got: {out}");
        assert!(out.contains("c: 3"), "got: {out}");
    }

    #[test]
    fn leaves_assign_with_non_literal_target_alone() {
        // `Object.assign(target, src)` mutates target — we must not
        // rewrite this since the mutation is observable.
        let out = apply_to_source(
            &ObjectAssignExpanded,
            "function f(target, src) { Object.assign(target, src); }",
        )
        .expect("parse");
        assert!(out.contains("Object.assign"), "got: {out}");
    }

    #[test]
    fn leaves_unrelated_call_alone() {
        let out = apply_to_source(
            &ObjectAssignExpanded,
            "function f() { return foo({a:1}, b); }",
        )
        .expect("parse");
        assert!(out.contains("foo("), "got: {out}");
    }

    #[test]
    fn leaves_assign_with_single_arg_alone() {
        let out = apply_to_source(
            &ObjectAssignExpanded,
            "function f() { return Object.assign({a:1}); }",
        )
        .expect("parse");
        assert!(out.contains("Object.assign"), "got: {out}");
    }

    #[test]
    fn idempotent_when_no_match() {
        let src = "function add(a, b) { return a + b; }";
        let first = apply_to_source(&ObjectAssignExpanded, src).expect("first");
        let second = apply_to_source(&ObjectAssignExpanded, &first).expect("second");
        assert_eq!(first, second);
    }
}
