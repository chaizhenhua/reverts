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
fn pipeline_aligns_computed_member_with_dot_member() {
    let bracketed = r#"function f(o) { return o["push"](o["length"]); }"#;
    let dotted = r#"function f(o) { return o.push(o.length); }"#;
    let n_b = apply_all_passes(bracketed);
    let n_d = apply_all_passes(dotted);
    let fp_b = FunctionExtractor::fingerprint(ModuleId(1), &n_b);
    let fp_d = FunctionExtractor::fingerprint(ModuleId(2), &n_d);
    assert_eq!(fp_b.len(), 1);
    assert_eq!(fp_d.len(), 1);
    assert_eq!(
        fp_b[0].primary.ast, fp_d[0].primary.ast,
        "computed and dot access must produce the same AST hash"
    );
    // The callee_set axis was the actual blocker — it tags
    // `cm:.push` for static-member calls but loses the name for
    // computed-member calls. After this pass both forms produce the
    // same callee_set.
    assert_eq!(
        fp_b[0].primary.callee_set, fp_d[0].primary.callee_set,
        "callee_set must agree after computed→static rewrite"
    );
}

#[test]
fn scope_aware_callee_filter_aligns_renamed_local_helpers() {
    // The whole point: when a helper at module top level is renamed
    // by a minifier (`toLocaleString` → `K92`), call sites of the
    // helper inside other module functions should produce the SAME
    // callee_set hash as the un-minified version — because the
    // renamed local is filtered out of the callee_set.
    let renamed = r#"
        function K92(A, Q, B) { return A.toLocaleString(Q, B); }
        function caller(n) { return K92(n, 'en', {}); }
    "#;
    let original = r#"
        function toLocaleString(A, Q, B) { return A.toLocaleString(Q, B); }
        function caller(n) { return toLocaleString(n, 'en', {}); }
    "#;
    let fp_renamed = FunctionExtractor::fingerprint(ModuleId(1), renamed);
    let fp_original = FunctionExtractor::fingerprint(ModuleId(2), original);
    // We expect 2 functions per source (the helper + the caller).
    assert_eq!(fp_renamed.len(), 2);
    assert_eq!(fp_original.len(), 2);

    // The `caller` function: param_count=1, stmts=1. Its callee_set
    // hashes should match between renamed and original — the call to
    // `K92` (a top-level local) is filtered out, leaving only the
    // method call `.toLocaleString` which IS recorded.
    // Wait — caller doesn't have a method call. It just calls K92.
    // After scope filtering, callee_set is empty (None) on both
    // sides. Same emptiness = same hash.
    let r_caller = fp_renamed.iter().find(|f| f.param_count == 1).unwrap();
    let o_caller = fp_original.iter().find(|f| f.param_count == 1).unwrap();
    assert_eq!(
        r_caller.primary.callee_set, o_caller.primary.callee_set,
        "scope-aware callee_set should ignore renamed local helper"
    );
}

#[test]
fn function_scope_callee_filter_treats_param_callees_as_unstable() {
    // `caller(K)` calls K, which is a PARAMETER (not a top-level
    // binding). Function-scope filtering should drop `c:K` from the
    // callee_set. Two functions with the same shape but different
    // param names should produce the same callee_set hash.
    let a = r#"
        function caller(K) { K(1); K(2); }
    "#;
    let b = r#"
        function caller(helper) { helper(1); helper(2); }
    "#;
    let fp_a = FunctionExtractor::fingerprint(ModuleId(1), a);
    let fp_b = FunctionExtractor::fingerprint(ModuleId(2), b);
    assert_eq!(fp_a.len(), 1);
    assert_eq!(fp_b.len(), 1);
    assert_eq!(
        fp_a[0].primary.callee_set, fp_b[0].primary.callee_set,
        "callee invoking a param identifier must not leak the param name"
    );
}

#[test]
fn function_scope_callee_filter_treats_let_body_local_as_unstable() {
    let renamed = r#"
        function caller(x) {
            let K = makeHelper();
            return K(x);
        }
    "#;
    let original = r#"
        function caller(x) {
            let helper = makeHelper();
            return helper(x);
        }
    "#;
    let r = FunctionExtractor::fingerprint(ModuleId(1), renamed);
    let o = FunctionExtractor::fingerprint(ModuleId(2), original);
    assert_eq!(r.len(), 1);
    assert_eq!(o.len(), 1);
    assert_eq!(
        r[0].primary.callee_set, o[0].primary.callee_set,
        "callee invoking a let-bound body local must not leak the local name"
    );
}

#[test]
fn function_scope_callee_filter_treats_block_scope_local_as_unstable() {
    // `let helper` declared inside an if-block is still local to the
    // function (block-scoped). Calling it inside the same block
    // should still be filtered out of callee_set.
    let renamed = r#"
        function caller(x) {
            if (x) {
                let K = makeHelper();
                K(x);
            }
        }
    "#;
    let original = r#"
        function caller(x) {
            if (x) {
                let helper = makeHelper();
                helper(x);
            }
        }
    "#;
    let r = FunctionExtractor::fingerprint(ModuleId(1), renamed);
    let o = FunctionExtractor::fingerprint(ModuleId(2), original);
    assert_eq!(r.len(), 1);
    assert_eq!(o.len(), 1);
    assert_eq!(
        r[0].primary.callee_set, o[0].primary.callee_set,
        "callee invoking a block-scope let-bound local must not leak the local name"
    );
}

#[test]
fn function_scope_callee_filter_treats_catch_param_as_unstable() {
    let renamed = r#"
        function safe(fn) {
            try { return fn(); } catch (E) { E.report(); }
        }
    "#;
    let original = r#"
        function safe(fn) {
            try { return fn(); } catch (err) { err.report(); }
        }
    "#;
    let r = FunctionExtractor::fingerprint(ModuleId(1), renamed);
    let o = FunctionExtractor::fingerprint(ModuleId(2), original);
    assert_eq!(r.len(), 1);
    assert_eq!(o.len(), 1);
    // The fingerprint AST + CFG must align; callee_set may also align.
    assert_eq!(
        r[0].primary.callee_set, o[0].primary.callee_set,
        "catch-clause param name should not leak via callee_set"
    );
}

#[test]
fn function_scope_callee_filter_treats_for_of_binding_as_unstable() {
    let renamed = r#"
        function loop(xs) {
            for (let K of xs) { K(); }
        }
    "#;
    let original = r#"
        function loop(xs) {
            for (let item of xs) { item(); }
        }
    "#;
    let r = FunctionExtractor::fingerprint(ModuleId(1), renamed);
    let o = FunctionExtractor::fingerprint(ModuleId(2), original);
    assert_eq!(r.len(), 1);
    assert_eq!(o.len(), 1);
    assert_eq!(
        r[0].primary.callee_set, o[0].primary.callee_set,
        "for-of binding identifier must be filtered as local"
    );
}

#[test]
fn function_scope_callee_filter_still_records_globals() {
    // `Number.isFinite` is a global that survives minification. Even
    // with scope filtering, it must remain in callee_set.
    let src = r#"
        function check(x) { return Number.isFinite(x); }
    "#;
    let fp = FunctionExtractor::fingerprint(ModuleId(1), src);
    assert_eq!(fp.len(), 1);
    assert!(
        fp[0].primary.callee_set.is_some(),
        "method call .isFinite must keep callee_set non-empty"
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
