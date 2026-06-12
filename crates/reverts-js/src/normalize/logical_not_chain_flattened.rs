use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Expression, Program};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_expression;
use oxc_span::SPAN;
use oxc_syntax::operator::UnaryOperator;
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `LogicalNotChainFlattened` collapses chains of three-or-more
/// adjacent logical-not operators down to one or two `!`s, depending
/// on parity:
///
/// ```text
/// !!!x   → !x       (3 nots = 1 net negation)
/// !!!!x  → !!x      (4 nots = ToBoolean)
/// !!!!!x → !x
/// !!!!!!x → !!x
/// ```
///
/// This rewrite is **strictly spec-equivalent**: per ECMA-262 §13.5.7,
/// `!x` evaluates `ToBoolean(x)` exactly once and returns the negated
/// boolean. The first `!` calls `ToBoolean(x)` on whatever expression
/// `x` is; every subsequent `!` operates on a pure boolean, where
/// `ToBoolean(bool)` is the identity. Therefore any chain of `N` nots
/// applies `ToBoolean` exactly once, then bit-flips `N` times — and a
/// `N`-bit-flip of a boolean is the same as a `(N mod 2)`-bit-flip.
///
/// No identifier dispatch, no shadowing concerns.
///
/// Two-not chains (`!!x`, the canonical `ToBoolean` idiom) and
/// single-not chains (`!x`, the canonical negation) are left alone.
/// Parenthesised wrappers anywhere in the chain are transparent.
pub struct LogicalNotChainFlattened;

impl NormalizationPass for LogicalNotChainFlattened {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::LogicalNotChainFlattened
    }
    fn version(&self) -> u32 {
        1
    }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut visitor = Flattener {
            builder: AstBuilder::new(alloc),
        };
        visitor.visit_program(program);
    }
}

struct Flattener<'a> {
    builder: AstBuilder<'a>,
}

impl<'a> VisitMut<'a> for Flattener<'a> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        walk_expression(self, expr);
        let depth = count_leading_nots(expr);
        if depth < 3 {
            return;
        }
        // Take ownership and drill down to the non-`!` core.
        let placeholder = self.builder.expression_null_literal(SPAN);
        let owned = mem::replace(expr, placeholder);
        let mut core = owned;
        for _ in 0..depth {
            let next = match strip_parens_owned(core) {
                Expression::UnaryExpression(u_box) => {
                    let u = u_box.unbox();
                    debug_assert!(matches!(u.operator, UnaryOperator::LogicalNot));
                    u.argument
                }
                other => {
                    core = other;
                    break;
                }
            };
            core = next;
        }
        // Reapply `(depth mod 2 == 0) ? 2 : 1` `!`s onto the core.
        let parity = depth % 2;
        let n = if parity == 0 { 2 } else { 1 };
        let mut result = core;
        for _ in 0..n {
            result = self
                .builder
                .expression_unary(SPAN, UnaryOperator::LogicalNot, result);
        }
        *expr = result;
    }
}

/// How many adjacent `!` operators are at the head of `expr` (after
/// stripping parens at every level).
fn count_leading_nots(expr: &Expression<'_>) -> usize {
    let mut count = 0;
    let mut cur = strip_parens_ref(expr);
    while let Expression::UnaryExpression(u) = cur
        && matches!(u.operator, UnaryOperator::LogicalNot)
    {
        count += 1;
        cur = strip_parens_ref(&u.argument);
    }
    count
}

fn strip_parens_owned<'a>(expr: Expression<'a>) -> Expression<'a> {
    match expr {
        Expression::ParenthesizedExpression(p) => strip_parens_owned(p.unbox().expression),
        other => other,
    }
}

fn strip_parens_ref<'a, 'b>(expr: &'b Expression<'a>) -> &'b Expression<'a> {
    match expr {
        Expression::ParenthesizedExpression(p) => strip_parens_ref(&p.expression),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn flattens_three_nots_to_one() {
        let out = apply_to_source(&LogicalNotChainFlattened, "function f(x) { return !!!x; }")
            .expect("parse");
        assert!(out.contains("!x"), "got: {out}");
        // No `!!!` left.
        assert!(!out.contains("!!!"), "got: {out}");
    }

    #[test]
    fn flattens_four_nots_to_two() {
        let out = apply_to_source(&LogicalNotChainFlattened, "function f(x) { return !!!!x; }")
            .expect("parse");
        assert!(out.contains("!!x"), "got: {out}");
        assert!(!out.contains("!!!!"), "got: {out}");
    }

    #[test]
    fn flattens_five_nots_to_one() {
        let out = apply_to_source(
            &LogicalNotChainFlattened,
            "function f(x) { return !!!!!x; }",
        )
        .expect("parse");
        assert!(out.contains("!x"), "got: {out}");
        assert!(!out.contains("!!"), "got: {out}");
    }

    #[test]
    fn leaves_single_not_alone() {
        let src = "function f(x) { return !x; }";
        let out = apply_to_source(&LogicalNotChainFlattened, src).expect("parse");
        assert!(out.contains("!x"), "got: {out}");
    }

    #[test]
    fn leaves_double_not_alone() {
        // `!!x` is the canonical ToBoolean idiom; we leave it.
        let src = "function f(x) { return !!x; }";
        let out = apply_to_source(&LogicalNotChainFlattened, src).expect("parse");
        assert!(out.contains("!!x"), "got: {out}");
        assert!(!out.contains("!!!"), "got: {out}");
    }

    #[test]
    fn handles_parens_inside_chain() {
        let out = apply_to_source(
            &LogicalNotChainFlattened,
            "function f(x) { return !(!(!(x))); }",
        )
        .expect("parse");
        // 3 nots through parens → 1 not on x.
        assert!(out.contains("!x"), "got: {out}");
    }

    #[test]
    fn handles_chain_around_call() {
        let out = apply_to_source(
            &LogicalNotChainFlattened,
            "function f() { return !!!getValue(); }",
        )
        .expect("parse");
        assert!(out.contains("!getValue()"), "got: {out}");
    }

    #[test]
    fn idempotent_on_already_short_chain() {
        let src = "function f(x) { return !x && !!x; }";
        let first = apply_to_source(&LogicalNotChainFlattened, src).expect("first");
        let second = apply_to_source(&LogicalNotChainFlattened, &first).expect("second");
        assert_eq!(first, second);
    }
}
