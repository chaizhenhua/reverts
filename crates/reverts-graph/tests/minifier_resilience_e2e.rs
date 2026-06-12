//! End-to-end validation that the full normalization pipeline aligns
//! minifier-emitted and source-form versions of the same function.
//!
//! Each test pair is a `(minified, source)` couple chosen to exercise
//! several passes in combination. Both halves are run through every
//! `stable_passes()` pass in sequence; the output of each pass should
//! converge to the same canonical text. We then compute the AST and
//! CFG axis hashes on the un-normalized **and** fully-normalized
//! forms — the normalized hashes must match across the pair.

use reverts_graph::FunctionExtractor;
use reverts_ir::ModuleId;
use reverts_js::normalize::{apply_to_source, stable_passes};

fn apply_all_passes(src: &str) -> String {
    let mut current = src.to_string();
    for pass in stable_passes() {
        match apply_to_source(pass.as_ref(), &current) {
            Ok(out) => current = out,
            Err(_) => return current, // bail at first parse error
        }
    }
    current
}

#[test]
fn full_pipeline_is_idempotent_on_plain_source() {
    let src = r#"
        function add(a, b) {
            return a + b;
        }
    "#;
    let first = apply_all_passes(src);
    let second = apply_all_passes(&first);
    assert_eq!(first, second, "full pipeline must be idempotent");
}

#[test]
fn pipeline_aligns_short_circuit_minified_with_source() {
    let minified = "function f(x) { x && doInit(); }";
    let source = "function f(x) { if (x) doInit(); }";
    let n_mini = apply_all_passes(minified);
    let n_src = apply_all_passes(source);
    assert_eq!(
        n_mini.trim(),
        n_src.trim(),
        "&&-form and if-form must converge after pipeline\nminified:\n{n_mini}\nsource:\n{n_src}"
    );
}

#[test]
fn pipeline_aligns_ternary_minified_with_source() {
    let minified = "function f(x) { x ? log('y') : log('n'); }";
    let source = "function f(x) { if (x) log('y'); else log('n'); }";
    let n_mini = apply_all_passes(minified);
    let n_src = apply_all_passes(source);
    assert_eq!(n_mini.trim(), n_src.trim());
}

#[test]
fn pipeline_aligns_void_zero_with_undefined() {
    let minified = "function f(x) { return x === void 0; }";
    let source = "function f(x) { return x === undefined; }";
    let n_mini = apply_all_passes(minified);
    let n_src = apply_all_passes(source);
    assert_eq!(n_mini.trim(), n_src.trim());
}

#[test]
fn pipeline_aligns_bang_zero_with_true() {
    let minified = "function f() { return !0; }";
    let source = "function f() { return true; }";
    let n_mini = apply_all_passes(minified);
    let n_src = apply_all_passes(source);
    assert_eq!(n_mini.trim(), n_src.trim());
}

#[test]
fn pipeline_aligns_throw_call_typeerror_with_throw_new_typeerror() {
    let minified = "function f(x) { if (!x) throw TypeError('bad'); }";
    let source = "function f(x) { if (!x) throw new TypeError('bad'); }";
    let n_mini = apply_all_passes(minified);
    let n_src = apply_all_passes(source);
    assert_eq!(n_mini.trim(), n_src.trim());
}

#[test]
fn pipeline_aligns_multi_declarator_with_separate() {
    let minified = "function f(x) { let a = 1, b = 2; return a + b + x; }";
    let source = "function f(x) { let a = 1; let b = 2; return a + b + x; }";
    let n_mini = apply_all_passes(minified);
    let n_src = apply_all_passes(source);
    assert_eq!(n_mini.trim(), n_src.trim());
}

#[test]
fn pipeline_aligns_object_assign_with_spread() {
    let minified = "function f(opts) { return Object.assign({a: 1, b: 2}, opts); }";
    let source = "function f(opts) { return {a: 1, b: 2, ...opts}; }";
    let n_mini = apply_all_passes(minified);
    let n_src = apply_all_passes(source);
    assert_eq!(n_mini.trim(), n_src.trim());
}

#[test]
fn pipeline_aligns_sequence_in_stmt_with_separate_calls() {
    let minified = "function f() { a(); (b(), c(), d()); }";
    let source = "function f() { a(); b(); c(); d(); }";
    let n_mini = apply_all_passes(minified);
    let n_src = apply_all_passes(source);
    assert_eq!(n_mini.trim(), n_src.trim());
}

#[test]
fn fingerprint_ast_hash_matches_after_full_pipeline_for_minified_pair() {
    // The big one: a pretty-bytes-flavoured function with ALL the
    // minifier rewrites in play simultaneously.
    let minified = r#"
        function pb(A, Q) {
            if (!Number.isFinite(A)) throw TypeError("bad");
            Q = Object.assign({bits: !1, binary: !1}, Q);
            let B = Q.bits ? UnitsB : UnitsT;
            let G = A < 0, Z = G ? "-" : "";
            G && (A = -A);
            return A < 1 ? Z + A + B[0] : Z + A + B[1];
        }
    "#;
    let source = r#"
        function pb(A, Q) {
            if (!Number.isFinite(A)) {
                throw new TypeError("bad");
            }
            Q = {bits: false, binary: false, ...Q};
            let B = Q.bits ? UnitsB : UnitsT;
            let G = A < 0;
            let Z = G ? "-" : "";
            if (G) {
                A = -A;
            }
            if (A < 1) return Z + A + B[0];
            return Z + A + B[1];
        }
    "#;

    let n_mini = apply_all_passes(minified);
    let n_src = apply_all_passes(source);

    // The codegen step doesn't try to make whitespace identical (it
    // preserves brace-wrappers around single statements when the
    // source had them explicitly, and omits them when the source
    // didn't), so textual equality is too strict a target here. The
    // real contract is that the *fingerprint axes* converge —
    // single-statement block unwrap is handled at hash time, so
    // `if (G) { A = -A; }` and `if (G) A = -A;` produce the same
    // AST/CFG hashes.
    let fp_mini = FunctionExtractor::fingerprint(ModuleId(1), &n_mini);
    let fp_src = FunctionExtractor::fingerprint(ModuleId(2), &n_src);
    assert_eq!(fp_mini.len(), 1);
    assert_eq!(fp_src.len(), 1);
    assert_eq!(
        fp_mini[0].primary.ast, fp_src[0].primary.ast,
        "primary AST hash should match after full normalization\nminified normalised:\n{n_mini}\nsource normalised:\n{n_src}"
    );
    assert_eq!(
        fp_mini[0].primary.cfg, fp_src[0].primary.cfg,
        "primary CFG hash should match after full normalization"
    );
}

#[test]
fn axes_match_across_all_dimensions_after_full_pipeline() {
    let minified = "function f(x) { x && doA(); x || doB(); }";
    let source = "function f(x) { if (x) doA(); if (!x) doB(); }";
    let n_mini = apply_all_passes(minified);
    let n_src = apply_all_passes(source);
    let fp_mini = FunctionExtractor::fingerprint(ModuleId(1), &n_mini);
    let fp_src = FunctionExtractor::fingerprint(ModuleId(2), &n_src);
    assert_eq!(fp_mini.len(), 1);
    assert_eq!(fp_src.len(), 1);
    assert_eq!(fp_mini[0].primary.ast, fp_src[0].primary.ast);
    assert_eq!(fp_mini[0].primary.cfg, fp_src[0].primary.cfg);
    assert_eq!(fp_mini[0].statement_count, fp_src[0].statement_count);
}
