use oxc_allocator::Allocator;
use oxc_ast::ast::{Expression, Program};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_expression;
use oxc_syntax::operator::BinaryOperator;
use reverts_ir::NormalizationPassId;
use std::mem;

use super::NormalizationPass;

/// `RelationalOperandCanonicalised` moves a literal operand of a commutative or
/// relational binary comparison to the RIGHT, flipping the relational operator
/// when it swaps — the classic "de-Yoda" canonicalization, generalized:
///
/// * `5 === x`  → `x === 5`          (commutative: operator unchanged)
/// * `2 * x`    → `x * 2`            (commutative)
/// * `0xff & f` → `f & 0xff`         (commutative)
/// * `5 < x`    → `x > 5`            (relational: operator flipped)
/// * `1 <= n`   → `n >= 1`
///
/// Why this helps fingerprint matching: minifiers and authors freely vary
/// operand order across builds, and the AST hash is order-sensitive (`5 === x`
/// and `x === 5` hash differently). Canonicalizing the literal to one side
/// collapses that variance so the two builds' functions match.
///
/// Only rewrites when EXACTLY ONE operand is a literal (no side effects to
/// reorder), so it is value-preserving — including the relational flips, which
/// are spec-equivalent for all operands (`5 < x` ≡ `x > 5`, even for `NaN`,
/// where both are `false`). `+` is deliberately excluded: it is not commutative
/// for strings (`"a" + x` ≠ `x + "a"`).
pub struct RelationalOperandCanonicalised;

impl NormalizationPass for RelationalOperandCanonicalised {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::RelationalOperandCanonicalised
    }

    fn version(&self) -> u32 {
        1
    }

    fn apply<'a>(&self, _alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut visitor = Rewriter;
        visitor.visit_program(program);
    }
}

struct Rewriter;

fn is_literal(expr: &Expression<'_>) -> bool {
    matches!(
        expr,
        Expression::NumericLiteral(_)
            | Expression::StringLiteral(_)
            | Expression::BooleanLiteral(_)
            | Expression::NullLiteral(_)
            | Expression::BigIntLiteral(_)
    )
}

/// Commutative operators whose operands may be reordered without changing the
/// result (excludes `+`, which concatenates strings).
fn is_commutative(operator: BinaryOperator) -> bool {
    matches!(
        operator,
        BinaryOperator::Equality
            | BinaryOperator::Inequality
            | BinaryOperator::StrictEquality
            | BinaryOperator::StrictInequality
            | BinaryOperator::Multiplication
            | BinaryOperator::BitwiseAnd
            | BinaryOperator::BitwiseOR
            | BinaryOperator::BitwiseXOR
    )
}

/// Relational operator obtained by swapping operands (`a < b` ≡ `b > a`).
fn swapped_relational(operator: BinaryOperator) -> Option<BinaryOperator> {
    Some(match operator {
        BinaryOperator::LessThan => BinaryOperator::GreaterThan,
        BinaryOperator::GreaterThan => BinaryOperator::LessThan,
        BinaryOperator::LessEqualThan => BinaryOperator::GreaterEqualThan,
        BinaryOperator::GreaterEqualThan => BinaryOperator::LessEqualThan,
        _ => return None,
    })
}

impl<'a> VisitMut<'a> for Rewriter {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        walk_expression(self, expr);
        let Expression::BinaryExpression(binary) = expr else {
            return;
        };
        let binary = binary.as_mut();
        // Only canonicalize when the literal is on the LEFT and the right is not
        // a literal (so `5 === x` -> `x === 5`, but `1 === 2` stays put).
        if !is_literal(&binary.left) || is_literal(&binary.right) {
            return;
        }
        if is_commutative(binary.operator) {
            mem::swap(&mut binary.left, &mut binary.right);
        } else if let Some(flipped) = swapped_relational(binary.operator) {
            mem::swap(&mut binary.left, &mut binary.right);
            binary.operator = flipped;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RelationalOperandCanonicalised;
    use crate::normalize::apply_to_source;

    fn run(src: &str) -> String {
        apply_to_source(&RelationalOperandCanonicalised, src).expect("apply")
    }

    #[test]
    fn moves_literal_to_right_for_commutative_ops() {
        assert!(run("let a = 5 === x;").contains("x === 5"));
        assert!(run("let a = 2 * y;").contains("y * 2"));
    }

    #[test]
    fn flips_relational_operator_when_swapping() {
        assert!(run("let a = 5 < x;").contains("x > 5"));
        assert!(run("let a = 1 <= n;").contains("n >= 1"));
    }

    #[test]
    fn leaves_addition_and_two_literals_alone() {
        // `+` is not commutative for strings.
        assert!(run("let a = 5 + x;").contains("5 + x"));
        // Both literals: nothing to canonicalize.
        assert!(run("let a = 1 === 2;").contains("1 === 2"));
    }
}
