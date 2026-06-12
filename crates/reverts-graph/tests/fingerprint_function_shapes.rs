//! Regression: `FunctionExtractor::fingerprint` must visit functions in
//! every common production form — function declarations, function
//! expressions in variable initialisers, function expressions in object
//! property values, arrows — and must handle module-body sources that
//! arrive wrapped in a single outer pair of braces (esbuild-style
//! arrow-body slices).

use reverts_graph::FunctionExtractor;
use reverts_ir::ModuleId;

#[test]
fn extracts_top_level_function_declaration() {
    let fps = FunctionExtractor::fingerprint(ModuleId(1), r#"function f(a) { return a + 1; }"#);
    assert_eq!(fps.len(), 1);
    assert_eq!(fps[0].param_count, 1);
}

#[test]
fn extracts_function_expression_in_var_decl() {
    let fps =
        FunctionExtractor::fingerprint(ModuleId(1), r#"var f = function (a) { return a + 1; };"#);
    assert_eq!(fps.len(), 1);
    assert_eq!(fps[0].param_count, 1);
}

#[test]
fn extracts_function_expression_as_object_property_value() {
    let fps = FunctionExtractor::fingerprint(
        ModuleId(1),
        r#"module.exports = {
            isFunction: function (x) { return typeof x === "function"; }
        };"#,
    );
    assert_eq!(fps.len(), 1);
    assert_eq!(fps[0].param_count, 1);
}

#[test]
fn extracts_functions_inside_outer_block_braces() {
    // esbuild-extracted arrow body slice arrives with a single outer
    // pair of `{ ... }` — the extractor must strip them so block-nested
    // FunctionDeclarations are visible to OXC at the program level.
    let fps = FunctionExtractor::fingerprint(
        ModuleId(1),
        r#"{
            function add(a, b) { return a + b; }
            function mul(a, b) { return a * b; }
        }"#,
    );
    assert_eq!(fps.len(), 2);
}

#[test]
fn extracts_function_in_block_braces_with_module_exports_only() {
    let fps = FunctionExtractor::fingerprint(
        ModuleId(1),
        r#"{
            module.exports = {
                isFunction: function (x) { return typeof x === "function"; }
            };
        }"#,
    );
    assert_eq!(fps.len(), 1);
}

#[test]
fn extracts_arrow_function() {
    let fps = FunctionExtractor::fingerprint(ModuleId(1), r#"const f = (a) => a + 1;"#);
    assert_eq!(fps.len(), 1);
    assert_eq!(fps[0].param_count, 1);
}
