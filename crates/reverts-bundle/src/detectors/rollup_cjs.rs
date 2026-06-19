use oxc_ast::ast::{Argument, CallExpression, Expression, Program, Statement};
use oxc_span::GetSpan;
use reverts_ir::{ByteRange, ModuleId};

use crate::inner_module::{BundlerKind, InnerModule};

/// Recognise Rollup's CJS-UMD output:
/// `(function (global, factory) { … }(this, (function () { /* body */ })));`
///
/// The inner factory function (second argument of the outer call) holds
/// the bundled code. We slice its body as one `InnerModule` per
/// top-level function declaration inside the body.
#[must_use]
pub fn detect(
    _source: &str,
    program: &Program<'_>,
    parent_module_id: ModuleId,
) -> Vec<InnerModule> {
    let mut out = Vec::new();
    for stmt in &program.body {
        let Statement::ExpressionStatement(expr) = stmt else {
            continue;
        };
        match &expr.expression {
            Expression::CallExpression(call) => {
                collect_from_outer_iife(call, parent_module_id, &mut out);
            }
            Expression::ParenthesizedExpression(p) => {
                if let Expression::CallExpression(call) = &p.expression {
                    collect_from_outer_iife(call, parent_module_id, &mut out);
                }
            }
            _ => {}
        }
    }
    out
}

fn collect_from_outer_iife<'a>(
    call: &CallExpression<'a>,
    parent_module_id: ModuleId,
    out: &mut Vec<InnerModule>,
) {
    // Expect callee = a function expression with two formal parameters
    // (`global`, `factory`). Reject if shape doesn't match — defensive.
    let outer_fn = match &call.callee {
        Expression::FunctionExpression(f) if f.params.items.len() == 2 => f,
        Expression::ParenthesizedExpression(p) => {
            if let Expression::FunctionExpression(f) = &p.expression {
                if f.params.items.len() != 2 {
                    return;
                }
                f
            } else {
                return;
            }
        }
        _ => return,
    };
    // Shape-gate confirmed; we don't need to inspect the outer body.
    let _ = outer_fn;

    // Second argument is the inner factory.
    let Some(inner_arg) = call.arguments.get(1) else {
        return;
    };
    let inner_factory = match inner_arg {
        Argument::FunctionExpression(f) => Some(&**f),
        Argument::ParenthesizedExpression(p) => {
            if let Expression::FunctionExpression(f) = &p.expression {
                Some(&**f)
            } else {
                None
            }
        }
        Argument::ArrowFunctionExpression(_) => None, // arrow uncommon in rollup-cjs
        _ => None,
    };
    let Some(inner) = inner_factory else { return };
    let Some(inner_body) = inner.body.as_ref() else {
        return;
    };

    // Walk the inner factory body. Each top-level FunctionDeclaration
    // becomes one inner module. Other statements (var, expression,
    // etc.) flow as part of the surrounding container — they aren't
    // independently matchable.
    let mut seq: usize = 0;
    for stmt in &inner_body.statements {
        if let Statement::FunctionDeclaration(f) = stmt
            && let Some(body) = f.body.as_ref()
        {
            let s = body.span();
            out.push(InnerModule {
                virtual_id: format!("rollup_cjs:{seq}"),
                body_span: ByteRange::new(s.start, s.end),
                bundler: BundlerKind::RollupCjs,
                source_path_hint: None,
                parent_module_id,
                synthetic_source: None,
            });
            seq += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn extract(src: &str) -> Vec<InnerModule> {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, src, SourceType::default()).parse();
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        detect(src, &parsed.program, ModuleId(2))
    }

    #[test]
    fn detect_rollup_cjs_extracts_each_inner_function() {
        let src = r#"
            (function (global, factory) {
                typeof exports === 'object' ? factory(exports) : factory({});
            }(this, (function (exports) {
                function helper() { return 1; }
                function entry() { return helper(); }
            })));
        "#;
        let modules = extract(src);
        assert_eq!(modules.len(), 2);
        for m in &modules {
            assert_eq!(m.bundler, BundlerKind::RollupCjs);
            assert!(m.source_path_hint.is_none());
            assert!(m.virtual_id.starts_with("rollup_cjs:"));
        }
    }

    #[test]
    fn detect_rollup_cjs_ignores_non_matching_outer_shape() {
        let src = "(function () { return 1; })()";
        assert!(extract(src).is_empty());
    }

    #[test]
    fn detect_rollup_cjs_returns_body_spans_for_inner_decls() {
        let src = r#"
            (function (global, factory) { factory(); }(this, (function () {
                function helper() { var y = 42; return y; }
            })));
        "#;
        let modules = extract(src);
        assert_eq!(modules.len(), 1);
        let body_text =
            &src[modules[0].body_span.start as usize..modules[0].body_span.end as usize];
        assert!(body_text.contains("var y = 42"));
    }
}
