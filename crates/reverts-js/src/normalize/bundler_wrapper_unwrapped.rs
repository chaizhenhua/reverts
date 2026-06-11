use oxc_allocator::Allocator;
use oxc_ast::AstBuilder;
use oxc_ast::ast::{Argument, Expression, Program};
use oxc_ast::visit::VisitMut;
use oxc_ast::visit::walk_mut::walk_expression;
use oxc_span::SPAN;
use reverts_ir::NormalizationPassId;

use super::NormalizationPass;

/// Names of esbuild's bundler-emitted helper wrappers that the unwrap pass
/// strips. The pass identifies a call expression whose callee is a bare
/// identifier matching one of these names and replaces the call with the
/// inner expression of its first argument.
///
/// The list intentionally covers esbuild's CommonJS/ESM interop and property
/// definition helpers — they wrap source-level values without changing their
/// runtime identity in ways that matter for downstream `ast_hash` parity.
pub const ESBUILD_WRAPPER_NAMES: &[&str] = &[
    "__toESM",
    "__toCommonJS",
    "__commonJS",
    "__esm",
    "__defProp",
    "__defProps",
    "__export",
    "__exportStar",
    "__reExport",
    "__copyProps",
];

/// `BundlerWrapperUnwrapped` removes esbuild-style helper calls so that wrapped
/// exports align with their unwrapped source. Nested wrappers are unwound in a
/// single pass via bottom-up traversal: when we encounter
/// `__toCommonJS(__toESM(x))`, the inner call collapses to `x` first, then the
/// outer call collapses to `x` as well.
pub struct BundlerWrapperUnwrapped;

impl NormalizationPass for BundlerWrapperUnwrapped {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::BundlerWrapperUnwrapped
    }

    fn version(&self) -> u32 {
        1
    }

    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut visitor = Unwrapper {
            builder: AstBuilder::new(alloc),
        };
        visitor.visit_program(program);
    }
}

struct Unwrapper<'a> {
    builder: AstBuilder<'a>,
}

impl<'a> Unwrapper<'a> {
    /// Returns true when `expr` is a call to one of the esbuild wrapper
    /// identifiers and its first argument is a plain expression (not a spread).
    fn is_unwrappable(expr: &Expression<'a>) -> bool {
        if let Expression::CallExpression(call) = expr
            && let Expression::Identifier(id) = &call.callee
            && ESBUILD_WRAPPER_NAMES.contains(&id.name.as_str())
            && let Some(first) = call.arguments.first()
            && !matches!(first, Argument::SpreadElement(_))
        {
            return true;
        }
        false
    }
}

impl<'a> VisitMut<'a> for Unwrapper<'a> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        // Bottom-up: recurse into children first so nested wrappers collapse
        // before we inspect the current expression.
        walk_expression(self, expr);

        while Self::is_unwrappable(expr) {
            // Swap out the call expression so we can move ownership of its
            // first argument. The dummy null literal is dropped on the next
            // line; the arena owns the underlying storage.
            let dummy = self.builder.expression_null_literal(SPAN);
            let taken = std::mem::replace(expr, dummy);
            let Expression::CallExpression(call) = taken else {
                // Restore — should be unreachable given `is_unwrappable`.
                break;
            };
            let call = call.unbox();
            let mut args = call.arguments;
            let Some(first) = args.drain(..).next() else {
                break;
            };
            if first.is_spread() {
                // Defensive: `is_unwrappable` rejects spread, so this branch
                // should be unreachable.
                break;
            }
            *expr = first.into_expression();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn bundler_wrapper_unwraps_to_esm_call() {
        let src = "const m = __toESM(require(\"foo\"));";
        let out = apply_to_source(&BundlerWrapperUnwrapped, src).expect("parses");
        assert!(
            !out.contains("__toESM"),
            "expected wrapper removed, got: {out}"
        );
        assert!(
            out.contains("require"),
            "expected require preserved, got: {out}"
        );
    }

    #[test]
    fn bundler_wrapper_unwraps_nested() {
        let src = "let v = __toCommonJS(__toESM(x));";
        let out = apply_to_source(&BundlerWrapperUnwrapped, src).expect("parses");
        assert!(!out.contains("__toCommonJS"));
        assert!(!out.contains("__toESM"));
        assert!(out.contains("x"));
    }

    #[test]
    fn bundler_wrapper_is_idempotent_on_already_unwrapped_code() {
        let src = "let v = x;\n";
        let first = apply_to_source(&BundlerWrapperUnwrapped, src).expect("parses");
        let second = apply_to_source(&BundlerWrapperUnwrapped, &first).expect("parses");
        assert_eq!(first, second);
    }
}
