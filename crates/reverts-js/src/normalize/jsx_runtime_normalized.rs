use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::Program;
use oxc_semantic::SemanticBuilder;
use oxc_transformer::{JsxOptions, JsxRuntime, TransformOptions, Transformer};
use reverts_ir::NormalizationPassId;

use super::NormalizationPass;

/// `JsxRuntimeNormalized` rewrites JSX elements/fragments to automatic-runtime
/// `_jsx(elementName, propsObj, ...)` call expressions so that JSX source and
/// already-lowered call-form source share the same downstream `ast_hash`.
///
/// On a JSX-free program this pass is a structural no-op: the transformer's
/// JSX plugin only fires when JSX nodes are present, so re-emitting the AST
/// reproduces the input verbatim.
pub struct JsxRuntimeNormalized;

impl NormalizationPass for JsxRuntimeNormalized {
    fn id(&self) -> NormalizationPassId {
        NormalizationPassId::JsxRuntimeNormalized
    }

    fn version(&self) -> u32 {
        1
    }

    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let options = TransformOptions {
            jsx: JsxOptions {
                runtime: JsxRuntime::Automatic,
                development: false,
                ..JsxOptions::default()
            },
            ..TransformOptions::default()
        };

        // The transformer needs an up-to-date `SymbolTable` + `ScopeTree`.
        // Build them fresh from the post-T5 (TS-erased) AST.
        let (symbols, scopes) = SemanticBuilder::new()
            .build(program)
            .semantic
            .into_symbol_table_and_scope_tree();

        // `source_path` is informational only for our use; an empty path is fine.
        let _ = Transformer::new(alloc, Path::new(""), &options)
            .build_with_symbols_and_scopes(symbols, scopes, program);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn jsx_runtime_normalized_rewrites_simple_element() {
        let src = "let v = <div className=\"x\">hi</div>;";
        let out = apply_to_source(&JsxRuntimeNormalized, src).expect("parses");
        assert!(out.contains("_jsx("), "expected _jsx call, got: {out}");
        assert!(
            out.contains("\"div\""),
            "expected element name 'div', got: {out}"
        );
    }

    #[test]
    fn jsx_runtime_normalized_is_idempotent_on_call_form() {
        let src = "let v = _jsx(\"div\", { className: \"x\" }, \"hi\");";
        let first = apply_to_source(&JsxRuntimeNormalized, src).expect("parses");
        let second = apply_to_source(&JsxRuntimeNormalized, &first).expect("parses");
        assert_eq!(first, second);
    }
}
