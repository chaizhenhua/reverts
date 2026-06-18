use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Expression, Program, SimpleAssignmentTarget};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::{walk_expression, walk_simple_assignment_target};
use oxc_span::SPAN;
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;
use crate::is_valid_static_member_property_name;

/// `ComputedToStaticMember` rewrites `obj["validIdent"]` into
/// `obj.validIdent` when the bracketed key is a string literal that
/// happens to be a valid JavaScript identifier and is not a reserved
/// word. The two forms are semantically identical: both look up the
/// same property on the same object.
///
/// Minifiers prefer the static-member form (one less byte per access),
/// while hand-written source sometimes uses the bracket form for
/// stylistic reasons (e.g. when the property name might shadow a
/// keyword, or to match dictionary-access semantics). The cascade
/// matcher's `access` and `callee_set` axes distinguish
/// `StaticMemberExpression` from `ComputedMemberExpression`: the
/// static form preserves the property name in the axis hash, the
/// computed form does not. Without this rewrite, identical code that
/// happens to spell the same access two different ways produces
/// divergent fingerprints.
pub struct ComputedToStaticMember;

impl NormalizationPass for ComputedToStaticMember {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::ComputedToStaticMember
    }
    fn version(&self) -> u32 {
        2
    }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut visitor = Rewriter {
            builder: AstBuilder::new(alloc),
        };
        visitor.visit_program(program);
    }
}

struct Rewriter<'a> {
    builder: AstBuilder<'a>,
}

impl<'a> VisitMut<'a> for Rewriter<'a> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        // Descend bottom-up so nested expressions normalise first.
        walk_expression(self, expr);
        if !is_rewritable_expr(expr) {
            return;
        }
        let placeholder = self.builder.expression_null_literal(SPAN);
        let owned = mem::replace(expr, placeholder);
        let Expression::ComputedMemberExpression(cme_box) = owned else {
            unreachable!()
        };
        let cme = cme_box.unbox();
        let key = match cme.expression {
            Expression::StringLiteral(s) => s,
            _ => unreachable!("is_rewritable_expr already verified key is a StringLiteral"),
        };
        let ident_name = self.builder.identifier_name(SPAN, key.value.as_str());
        *expr = Expression::StaticMemberExpression(
            self.builder.alloc(self.builder.static_member_expression(
                SPAN,
                cme.object,
                ident_name,
                cme.optional,
            )),
        );
    }

    fn visit_simple_assignment_target(&mut self, t: &mut SimpleAssignmentTarget<'a>) {
        walk_simple_assignment_target(self, t);
        if !is_rewritable_target(t) {
            return;
        }
        let placeholder = self
            .builder
            .simple_assignment_target_identifier_reference(SPAN, "__cme_placeholder__");
        let owned = mem::replace(t, placeholder);
        let SimpleAssignmentTarget::ComputedMemberExpression(cme_box) = owned else {
            unreachable!()
        };
        let cme = cme_box.unbox();
        let key = match cme.expression {
            Expression::StringLiteral(s) => s,
            _ => unreachable!("is_rewritable_target already verified key is a StringLiteral"),
        };
        let ident_name = self.builder.identifier_name(SPAN, key.value.as_str());
        *t = SimpleAssignmentTarget::StaticMemberExpression(
            self.builder.alloc(self.builder.static_member_expression(
                SPAN,
                cme.object,
                ident_name,
                cme.optional,
            )),
        );
    }
}

fn is_rewritable_expr(expr: &Expression<'_>) -> bool {
    let Expression::ComputedMemberExpression(cme) = expr else {
        return false;
    };
    let Expression::StringLiteral(s) = &cme.expression else {
        return false;
    };
    is_valid_static_member_property_name(s.value.as_str())
}

fn is_rewritable_target(t: &SimpleAssignmentTarget<'_>) -> bool {
    let SimpleAssignmentTarget::ComputedMemberExpression(cme) = t else {
        return false;
    };
    let Expression::StringLiteral(s) = &cme.expression else {
        return false;
    };
    is_valid_static_member_property_name(s.value.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn rewrites_bracket_string_to_dot_access() {
        let out = apply_to_source(
            &ComputedToStaticMember,
            r#"function f(obj) { return obj["foo"]; }"#,
        )
        .expect("parse");
        assert!(out.contains("obj.foo"), "got: {out}");
        assert!(!out.contains("obj["), "got: {out}");
    }

    #[test]
    fn rewrites_chained_brackets() {
        let out = apply_to_source(
            &ComputedToStaticMember,
            r#"function f(o) { return o["a"]["b"]["c"]; }"#,
        )
        .expect("parse");
        assert!(out.contains("o.a.b.c"), "got: {out}");
    }

    #[test]
    fn rewrites_bracket_call() {
        let out = apply_to_source(
            &ComputedToStaticMember,
            r#"function f(o) { return o["push"](1); }"#,
        )
        .expect("parse");
        assert!(out.contains("o.push(1)"), "got: {out}");
    }

    #[test]
    fn leaves_non_identifier_keys_alone() {
        let out = apply_to_source(
            &ComputedToStaticMember,
            r#"function f(o) { return o["with-dash"]; }"#,
        )
        .expect("parse");
        assert!(out.contains("\"with-dash\""), "got: {out}");
    }

    #[test]
    fn leaves_numeric_keys_alone() {
        let out = apply_to_source(&ComputedToStaticMember, r#"function f(o) { return o[0]; }"#)
            .expect("parse");
        assert!(out.contains("[0]"), "got: {out}");
    }

    #[test]
    fn leaves_reserved_words_alone() {
        // `obj.class` is technically legal post-ES5 but minifiers
        // historically left these as `obj["class"]` for compatibility,
        // and the safe move is to keep them bracketed.
        let out = apply_to_source(
            &ComputedToStaticMember,
            r#"function f(o) { return o["class"]; }"#,
        )
        .expect("parse");
        assert!(out.contains("\"class\""), "got: {out}");
    }

    #[test]
    fn leaves_dynamic_key_alone() {
        let src = r#"function f(o, k) { return o[k]; }"#;
        let out = apply_to_source(&ComputedToStaticMember, src).expect("parse");
        assert!(out.contains("o[k]"), "got: {out}");
    }

    #[test]
    fn rewrites_assignment_target_bracket_to_dot() {
        let out = apply_to_source(
            &ComputedToStaticMember,
            r#"function f(obj) { obj["foo"] = 1; }"#,
        )
        .expect("parse");
        assert!(out.contains("obj.foo = 1"), "got: {out}");
        assert!(!out.contains("\"foo\""), "got: {out}");
    }

    #[test]
    fn rewrites_compound_assignment_target() {
        let out = apply_to_source(
            &ComputedToStaticMember,
            r#"function f(obj) { obj["count"] += 1; }"#,
        )
        .expect("parse");
        assert!(out.contains("obj.count"), "got: {out}");
        assert!(!out.contains("\"count\""), "got: {out}");
    }

    #[test]
    fn leaves_dynamic_assignment_target_alone() {
        let src = r#"function f(obj, k) { obj[k] = 1; }"#;
        let out = apply_to_source(&ComputedToStaticMember, src).expect("parse");
        assert!(out.contains("obj[k]"), "got: {out}");
    }

    #[test]
    fn leaves_non_identifier_key_assignment_alone() {
        let src = r#"function f(obj) { obj["with-dash"] = 1; }"#;
        let out = apply_to_source(&ComputedToStaticMember, src).expect("parse");
        assert!(out.contains("\"with-dash\""), "got: {out}");
    }

    #[test]
    fn idempotent_on_already_static() {
        let src = "function f(o) { return o.foo.bar; }";
        let first = apply_to_source(&ComputedToStaticMember, src).expect("first");
        let second = apply_to_source(&ComputedToStaticMember, &first).expect("second");
        assert_eq!(first, second);
    }
}
