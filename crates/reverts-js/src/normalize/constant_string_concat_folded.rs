use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Expression, Program};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_expression;
use oxc_span::SPAN;
use oxc_syntax::operator::BinaryOperator;
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `ConstantStringConcatFolded` folds adjacent `StringLiteral + ... +
/// StringLiteral` chains into a single `StringLiteral` whose value is
/// the concatenation of all the operand strings.
///
/// Per ECMA-262 §13.10.1 (`ApplyStringOrNumericBinaryOperator` for
/// `+`), when both operands resolve to strings the result is the
/// concatenation of their values. For literal-only operand pairs no
/// `ToPrimitive` / `ToString` user callback can run, so the result is
/// fully deterministic and the rewrite is **strictly spec-equivalent**
/// — no identifier dispatch, no shadowing concerns.
///
/// The fold runs bottom-up, so nested chains collapse correctly:
/// `"a" + ("b" + "c")` → `"a" + "bc"` → `"abc"`.
///
/// The fold deliberately does NOT touch:
///
/// * `"a" + x` where `x` is a non-literal expression — `x` could be
///   any value with a custom `Symbol.toPrimitive`.
/// * `1 + 2` (numeric addition) or `"a" + 1` (mixed) — defer to a
///   future pass; floating-point arithmetic on literals has its own
///   set of edge cases (`NaN`, `-0`, denormals, precision) worth
///   handling in a dedicated module.
pub struct ConstantStringConcatFolded;

impl NormalizationPass for ConstantStringConcatFolded {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::ConstantStringConcatFolded
    }
    fn version(&self) -> u32 {
        1
    }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut visitor = Folder {
            builder: AstBuilder::new(alloc),
        };
        visitor.visit_program(program);
    }
}

struct Folder<'a> {
    builder: AstBuilder<'a>,
}

impl<'a> VisitMut<'a> for Folder<'a> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        // Recurse first so nested concatenations fold bottom-up.
        walk_expression(self, expr);
        if !matches_string_concat(expr) {
            return;
        }
        let placeholder = self.builder.expression_null_literal(SPAN);
        let owned = mem::replace(expr, placeholder);
        let Expression::BinaryExpression(bin_box) = owned else {
            unreachable!()
        };
        let bin = bin_box.unbox();
        let left = strip_parens(bin.left);
        let right = strip_parens(bin.right);
        let (Expression::StringLiteral(left), Expression::StringLiteral(right)) = (left, right)
        else {
            unreachable!("matches_string_concat already verified both sides are StringLiteral")
        };
        let combined = format!("{}{}", left.value, right.value);
        *expr = self.builder.expression_string_literal(SPAN, combined, None);
    }
}

fn strip_parens<'a>(expr: Expression<'a>) -> Expression<'a> {
    match expr {
        Expression::ParenthesizedExpression(p) => strip_parens(p.unbox().expression),
        other => other,
    }
}

fn strip_parens_ref<'a, 'b>(expr: &'b Expression<'a>) -> &'b Expression<'a> {
    match expr {
        Expression::ParenthesizedExpression(p) => strip_parens_ref(&p.expression),
        other => other,
    }
}

fn matches_string_concat(expr: &Expression<'_>) -> bool {
    let Expression::BinaryExpression(bin) = expr else {
        return false;
    };
    if !matches!(bin.operator, BinaryOperator::Addition) {
        return false;
    }
    matches!(strip_parens_ref(&bin.left), Expression::StringLiteral(_))
        && matches!(strip_parens_ref(&bin.right), Expression::StringLiteral(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn folds_two_string_literals() {
        let out = apply_to_source(
            &ConstantStringConcatFolded,
            r#"function f() { return "foo" + "bar"; }"#,
        )
        .expect("parse");
        assert!(out.contains(r#""foobar""#), "got: {out}");
        assert!(!out.contains(r#""foo""#), "got: {out}");
    }

    #[test]
    fn folds_three_string_literals_via_bottom_up_walk() {
        // `"a" + "b" + "c"` parses as `("a" + "b") + "c"`. Walking
        // bottom-up folds the inner `("a" + "b")` first into `"ab"`,
        // then the outer `"ab" + "c"` into `"abc"`.
        let out = apply_to_source(
            &ConstantStringConcatFolded,
            r#"function f() { return "a" + "b" + "c"; }"#,
        )
        .expect("parse");
        assert!(out.contains(r#""abc""#), "got: {out}");
    }

    #[test]
    fn folds_chain_with_paren_grouping() {
        let out = apply_to_source(
            &ConstantStringConcatFolded,
            r#"function f() { return "a" + ("b" + "c"); }"#,
        )
        .expect("parse");
        assert!(out.contains(r#""abc""#), "got: {out}");
    }

    #[test]
    fn folds_empty_strings_preserved() {
        let out = apply_to_source(
            &ConstantStringConcatFolded,
            r#"function f() { return "" + "value"; }"#,
        )
        .expect("parse");
        assert!(out.contains(r#""value""#), "got: {out}");
    }

    #[test]
    fn leaves_string_plus_identifier_alone() {
        let src = r#"function f(x) { return "prefix-" + x; }"#;
        let out = apply_to_source(&ConstantStringConcatFolded, src).expect("parse");
        assert!(out.contains(r#""prefix-" + x"#), "got: {out}");
    }

    #[test]
    fn leaves_string_plus_number_alone() {
        // Mixed-type folding deferred — see pass docstring.
        let src = r#"function f() { return "v" + 1; }"#;
        let out = apply_to_source(&ConstantStringConcatFolded, src).expect("parse");
        assert!(out.contains(r#""v" + 1"#), "got: {out}");
    }

    #[test]
    fn leaves_other_operators_alone() {
        let src = r#"function f() { return "a".repeat(3); }"#;
        let out = apply_to_source(&ConstantStringConcatFolded, src).expect("parse");
        assert!(out.contains("repeat"), "got: {out}");
    }

    #[test]
    fn idempotent_on_already_folded() {
        let src = r#"function f() { return "abc"; }"#;
        let first = apply_to_source(&ConstantStringConcatFolded, src).expect("first");
        let second = apply_to_source(&ConstantStringConcatFolded, &first).expect("second");
        assert_eq!(first, second);
    }
}
