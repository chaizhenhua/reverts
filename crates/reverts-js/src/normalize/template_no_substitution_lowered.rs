use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Expression, Program};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_expression;
use oxc_span::SPAN;
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `TemplateNoSubstitutionLowered` rewrites an untagged no-substitution
/// template literal `` `foo` `` into a regular string literal `"foo"`.
///
/// Per ECMA-262 §13.2.8 (`Evaluation of TemplateLiteral`), the value
/// of an untagged template with no substitutions is the cooked string
/// of its single template element — bitwise identical to the string
/// literal carrying the same value. There is no `ToPrimitive` callback
/// path, no observable side effect, and no identifier dispatch
/// involved. The rewrite is **strictly spec-equivalent**.
///
/// Tagged templates (`` tag`foo` ``) are deliberately not rewritten:
/// their evaluation passes the cooked array, the raw array, and any
/// substitutions to the `tag` function (§13.2.8.6), which is an
/// entirely different call shape from `tag("foo")`. The pass only
/// matches `Expression::TemplateLiteral`; the inner quasi of a
/// `TaggedTemplateExpression` is exposed via a different field and is
/// not visited as an expression.
///
/// Templates containing any substitution are left alone — `` `${x}` ``
/// performs `ToString(x)` and concatenation; the rewrite for those
/// belongs in a follow-up pass that can reason about operand types.
pub struct TemplateNoSubstitutionLowered;

impl NormalizationPass for TemplateNoSubstitutionLowered {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::TemplateNoSubstitutionLowered
    }
    fn version(&self) -> u32 {
        1
    }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut visitor = Lowerer {
            builder: AstBuilder::new(alloc),
        };
        visitor.visit_program(program);
    }
}

struct Lowerer<'a> {
    builder: AstBuilder<'a>,
}

impl<'a> VisitMut<'a> for Lowerer<'a> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        walk_expression(self, expr);
        let Expression::TemplateLiteral(tpl) = expr else {
            return;
        };
        if !tpl.expressions.is_empty() {
            return;
        }
        let Some(quasi) = tpl.quasis.first() else {
            return;
        };
        // Untagged template literals never carry invalid escape
        // sequences, so `cooked` is required here.
        let value = quasi
            .value
            .cooked
            .as_deref()
            .expect("untagged no-substitution template literal must have cooked text")
            .to_owned();
        let placeholder = self.builder.expression_null_literal(SPAN);
        let _ = mem::replace(expr, placeholder);
        *expr = self.builder.expression_string_literal(SPAN, value, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn lowers_no_substitution_template_to_string() {
        let out = apply_to_source(
            &TemplateNoSubstitutionLowered,
            "function f() { return `hello`; }",
        )
        .expect("parse");
        assert!(out.contains(r#""hello""#), "got: {out}");
        assert!(!out.contains('`'), "got: {out}");
    }

    #[test]
    fn lowers_empty_template_to_empty_string() {
        let out = apply_to_source(
            &TemplateNoSubstitutionLowered,
            "function f() { return ``; }",
        )
        .expect("parse");
        assert!(out.contains(r#""""#), "got: {out}");
        assert!(!out.contains('`'), "got: {out}");
    }

    #[test]
    fn leaves_template_with_substitution_alone() {
        let src = "function f(x) { return `hello ${x}`; }";
        let out = apply_to_source(&TemplateNoSubstitutionLowered, src).expect("parse");
        assert!(out.contains('`'), "got: {out}");
        assert!(out.contains("${x}"), "got: {out}");
    }

    #[test]
    fn leaves_tagged_no_substitution_template_alone() {
        // `tag\`foo\`` calls tag with ([\"foo\"], cooked=[\"foo\"], raw=[\"foo\"]).
        // Rewriting the inner quasi to a string literal would change
        // the call signature to `tag(\"foo\")` — observably different.
        let src = "function f(tag) { return tag`foo`; }";
        let out = apply_to_source(&TemplateNoSubstitutionLowered, src).expect("parse");
        assert!(out.contains('`'), "got: {out}");
        assert!(out.contains("tag`foo`"), "got: {out}");
    }

    #[test]
    fn preserves_escape_sequences_in_cooked_value() {
        // The cooked value of `\\n` is a single newline char.
        let out = apply_to_source(
            &TemplateNoSubstitutionLowered,
            r"function f() { return `a\nb`; }",
        )
        .expect("parse");
        // After lowering, codegen will re-escape the newline as needed
        // for the string literal. The content should still be
        // observable as the same printed character sequence.
        assert!(!out.contains('`'), "got: {out}");
        assert!(out.contains("a\\nb") || out.contains("a\nb"), "got: {out}");
    }

    #[test]
    fn lowers_template_inside_argument_list() {
        let out = apply_to_source(
            &TemplateNoSubstitutionLowered,
            "function f() { console.log(`hi`); }",
        )
        .expect("parse");
        assert!(out.contains(r#""hi""#), "got: {out}");
    }

    #[test]
    fn idempotent_when_no_match() {
        let src = r#"function f() { return "already a string"; }"#;
        let first = apply_to_source(&TemplateNoSubstitutionLowered, src).expect("first");
        let second = apply_to_source(&TemplateNoSubstitutionLowered, &first).expect("second");
        assert_eq!(first, second);
    }
}
