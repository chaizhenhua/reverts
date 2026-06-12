//! L6 adversarial false-positive corpus per design spec §7.
//!
//! 20 inputs that LOOK like bundler patterns but aren't. Each detector
//! must reject them. FP rate must be 0/20.

use reverts_bundle::{BundleClassification, classifier::classify};
use std::path::Path;

fn classify_is_plain(src: &str) -> bool {
    matches!(
        classify(Path::new("fixture.js"), src),
        BundleClassification::Plain
    )
}

#[test]
fn esbuild_lookalikes_are_rejected() {
    // __commonJS with array arg (not object)
    assert!(classify_is_plain(r#"var x = __commonJS(["a", "b"]);"#));
    // __commonJS with no args
    assert!(classify_is_plain("var x = __commonJS();"));
    // Object literal that looks like a registry but isn't called __commonJS
    assert!(classify_is_plain(
        r#"var x = registry({"a.js": (e,m)=>{}});"#
    ));
    // __commonJS where value is a non-function
    assert!(classify_is_plain(r#"var x = __commonJS({"a.js": 42});"#));
    // __commonJS where value is a string
    assert!(classify_is_plain(
        r#"var x = __commonJS({"a.js": "not a function"});"#
    ));
}

#[test]
fn webpack5_lookalikes_are_rejected() {
    // Wrong variable name
    assert!(classify_is_plain(
        r#"var __not_webpack__ = {"./a": ()=>{}};"#
    ));
    // Right name but no object
    assert!(classify_is_plain("var __webpack_modules__ = 42;"));
    // Right name, object, but values are non-functions
    assert!(classify_is_plain(
        r#"var __webpack_modules__ = {"./a": 1, "./b": 2};"#
    ));
    // Function variant with no body — actually IS detected as Marked
    assert!(!classify_is_plain(
        r#"var __webpack_modules__ = {"./a": function(){}};"#
    ));
    // Empty module map
    assert!(classify_is_plain("var __webpack_modules__ = {};"));
}

#[test]
fn rollup_cjs_lookalikes_are_rejected() {
    // Wrong outer arity (3 params instead of 2)
    assert!(classify_is_plain(
        r#"(function(a,b,c){})(1,(function(){function f(){}}));"#
    ));
    // Outer has 2 params but inner factory is not a function
    assert!(classify_is_plain(r#"(function(g,f){f();}(this, 42));"#));
    // Outer right shape but inner factory body is empty
    assert!(classify_is_plain(
        r#"(function(g,f){f();}(this, (function(){})));"#
    ));
    // Outer right shape but factory body has no FunctionDeclaration
    assert!(classify_is_plain(
        r#"(function(g,f){f();}(this, (function(){var x = 1;})));"#
    ));
    // Top-level IIFE missing factory altogether
    assert!(classify_is_plain(r#"(function(){function f(){}}());"#));
}

#[test]
fn plain_js_with_overlapping_identifiers_is_rejected() {
    // Comments mentioning __commonJS but no actual call
    assert!(classify_is_plain(
        "// uses __commonJS internally\nfunction main() {}"
    ));
    // String literal that LOOKS like the pattern
    assert!(classify_is_plain(
        r#"var note = 'this is not __commonJS({})';"#
    ));
    // Member access on `__commonJS`-named property
    assert!(classify_is_plain(r#"obj.__commonJS = true;"#));
    // Function declaration NAMED `__commonJS` (not called)
    assert!(classify_is_plain(r#"function __commonJS(o) { return o; }"#));
    // Define-style AMD (Phase α doesn't recognise AMD; should be Plain)
    assert!(classify_is_plain(
        r#"define("name", ["dep"], function(d){return {};});"#
    ));
}
