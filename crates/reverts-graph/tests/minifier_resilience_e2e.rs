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
fn pipeline_aligns_void_zero_with_undefined_when_not_shadowed() {
    // The `void_zero_to_undefined_guarded` pass rewrites `void 0` to
    // `undefined` only after verifying the program does not bind a
    // local `undefined` and contains no `with` statement. In the
    // common case (neither condition triggers) the rewrite is
    // strictly spec-equivalent and the two forms converge.
    let minified = "function f(x) { return x === void 0; }";
    let source = "function f(x) { return x === undefined; }";
    let fp_m = FunctionExtractor::fingerprint(ModuleId(1), &apply_all_passes(minified));
    let fp_s = FunctionExtractor::fingerprint(ModuleId(2), &apply_all_passes(source));
    assert_eq!(
        fp_m[0].primary.ast, fp_s[0].primary.ast,
        "void 0 ↔ undefined must converge after pipeline when undefined is not shadowed"
    );
}

#[test]
fn pipeline_keeps_void_zero_distinct_when_undefined_is_shadowed() {
    // When the program binds a local `undefined`, the guarded pass
    // bails out and `void 0` stays as `void 0`. The two forms then
    // remain distinct, as they semantically differ.
    let minified = r#"
        function f(x) {
            let undefined = 1;
            return x === void 0;
        }
    "#;
    let source = r#"
        function f(x) {
            let undefined = 1;
            return x === undefined;
        }
    "#;
    let fp_m = FunctionExtractor::fingerprint(ModuleId(1), &apply_all_passes(minified));
    let fp_s = FunctionExtractor::fingerprint(ModuleId(2), &apply_all_passes(source));
    assert_ne!(
        fp_m[0].primary.ast, fp_s[0].primary.ast,
        "guarded pass must NOT collapse void 0 when `undefined` is shadowed"
    );
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
fn pipeline_keeps_throw_call_distinct_from_throw_new() {
    // Same policy: `throw Foo()` and `throw new Foo()` differ under
    // arrow-function shadowing of `Foo` and are kept distinct.
    let minified = "function f(x) { if (!x) throw TypeError('bad'); }";
    let source = "function f(x) { if (!x) throw new TypeError('bad'); }";
    let fp_m = FunctionExtractor::fingerprint(ModuleId(1), &apply_all_passes(minified));
    let fp_s = FunctionExtractor::fingerprint(ModuleId(2), &apply_all_passes(source));
    assert_ne!(fp_m[0].primary.ast, fp_s[0].primary.ast);
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
fn pipeline_keeps_object_assign_distinct_from_spread() {
    // `Object.assign({...}, x)` vs `{...x}` is not strictly
    // equivalent under `Object.assign` shadowing. The pipeline keeps
    // both forms as-is, so their AST hashes differ.
    let minified = "function f(opts) { return Object.assign({a: 1, b: 2}, opts); }";
    let source = "function f(opts) { return {a: 1, b: 2, ...opts}; }";
    let fp_m = FunctionExtractor::fingerprint(ModuleId(1), &apply_all_passes(minified));
    let fp_s = FunctionExtractor::fingerprint(ModuleId(2), &apply_all_passes(source));
    assert_ne!(fp_m[0].primary.ast, fp_s[0].primary.ast);
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
    // A function whose minified vs source forms differ only in rewrites
    // the pipeline applies under strict spec equivalence:
    //   - `!1` → `false`
    //   - `let a = b, c = d;` → `let a = b; let c = d;`
    //   - `a && b;` → `if (a) b;`
    //   - `return cond ? a : b;` → `if (cond) return a; return b;`
    //
    // Rewrites that depend on global identifiers (`undefined`,
    // `Object.assign`, `TypeError`) are **not** applied because the
    // identifiers are shadowable. The pair below avoids those.
    let minified = r#"
        function pb(A, Q) {
            let B = Q.bits ? UnitsB : UnitsT;
            let G = A < 0, Z = G ? "-" : "";
            G && (A = -A);
            return A < 1 ? Z + A + B[0] : Z + A + B[1];
        }
    "#;
    let source = r#"
        function pb(A, Q) {
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
    let r_caller = fp_renamed
        .iter()
        .find(|f| f.param_count == 1)
        .expect("renamed fingerprint should contain a 1-param function");
    let o_caller = fp_original
        .iter()
        .find(|f| f.param_count == 1)
        .expect("original fingerprint should contain a 1-param function");
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
fn pipeline_aligns_arrow_expression_form_with_block_form() {
    let minified = "var f = (a) => a + 1;";
    let source = "var f = (a) => { return a + 1; };";
    let n_m = apply_all_passes(minified);
    let n_s = apply_all_passes(source);
    let fp_m = FunctionExtractor::fingerprint(ModuleId(1), &n_m);
    let fp_s = FunctionExtractor::fingerprint(ModuleId(2), &n_s);
    assert_eq!(fp_m.len(), 1);
    assert_eq!(fp_s.len(), 1);
    assert_eq!(
        fp_m[0].primary.ast, fp_s[0].primary.ast,
        "arrow expression-form and block-form must produce the same AST hash"
    );
    assert_eq!(
        fp_m[0].primary.cfg, fp_s[0].primary.cfg,
        "arrow expression-form and block-form must produce the same CFG hash"
    );
    assert_eq!(
        fp_m[0].statement_count, fp_s[0].statement_count,
        "statement_count must match (both produce 1)"
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
fn closure_scope_filter_aligns_renamed_enclosing_helper() {
    // `inner` calls `helper`, a name bound in the *enclosing* function
    // (`outer`'s scope), not in `inner`'s own scope. The minifier
    // renames `helper` to `K`. Under closure-scope filtering both
    // names are in the universal-locals set and dropped from
    // `inner`'s callee_set — the two fingerprints converge.
    //
    // `inner` is given two statements so we can uniquely identify it
    // by `statement_count == 2` (helper has 1, outer has 3).
    let minified = r#"
        function outer() {
            function K(arg) { return K + arg; }
            function inner(x) {
                const y = x + 1;
                return K(y);
            }
            return inner;
        }
    "#;
    let source = r#"
        function outer() {
            function helper(arg) { return helper + arg; }
            function inner(x) {
                const y = x + 1;
                return helper(y);
            }
            return inner;
        }
    "#;
    let fp_m = FunctionExtractor::fingerprint(ModuleId(1), minified);
    let fp_s = FunctionExtractor::fingerprint(ModuleId(2), source);
    let inner_m = fp_m
        .iter()
        .find(|f| f.statement_count == 2)
        .expect("minified inner");
    let inner_s = fp_s
        .iter()
        .find(|f| f.statement_count == 2)
        .expect("source inner");
    // Both inners call into an enclosing-scope helper. Without
    // closure-scope filtering the minified inner would record `c:K`
    // and the source inner `c:helper`, diverging.
    assert_eq!(
        inner_m.primary.callee_set, inner_s.primary.callee_set,
        "closure-renamed helper must collapse the callee_set hash"
    );
}

#[test]
fn closure_scope_filter_includes_imported_bindings() {
    // Imports introduce renameable bindings the same way `let`/`function`
    // do. When the source uses an imported helper that the bundle
    // inlines under a renamed local symbol, the universal-locals filter
    // must drop both `helper` (the imported name) and `K` (the bundle's
    // renamed name), so their callee_sets converge.
    let source = r#"
        import { helper } from './lib';
        function inner(x) {
            const y = x + 1;
            return helper(y);
        }
    "#;
    let bundle = r#"
        function K(arg) { return arg + 1; }
        function inner(x) {
            const y = x + 1;
            return K(y);
        }
    "#;
    let fp_s = FunctionExtractor::fingerprint(ModuleId(1), source);
    let fp_b = FunctionExtractor::fingerprint(ModuleId(2), bundle);
    let inner_s = fp_s
        .iter()
        .find(|f| f.statement_count == 2)
        .expect("source inner");
    let inner_b = fp_b
        .iter()
        .find(|f| f.statement_count == 2)
        .expect("bundle inner");
    assert_eq!(
        inner_s.primary.callee_set, inner_b.primary.callee_set,
        "import-bound helper must be filtered from callee_set the same way bundle-local helpers are"
    );
}

#[test]
fn closure_scope_filter_preserves_global_callees() {
    // `console` is not bound anywhere in the file, so closure-scope
    // collection must NOT touch it. The fingerprint records the
    // global.
    let src = r#"
        function outer() {
            function inner() { console.log('hi'); }
            return inner;
        }
    "#;
    let fp = FunctionExtractor::fingerprint(ModuleId(1), src);
    let inner = fp.iter().find(|f| f.statement_count == 1).expect("inner");
    assert!(
        inner.primary.callee_set.is_some(),
        "`console.log` is a global method-call and must survive closure filtering"
    );
}

#[test]
fn pipeline_aligns_boolean_call_with_double_not_when_not_shadowed() {
    let minified = "function f(x) { return Boolean(x); }";
    let source = "function f(x) { return !!x; }";
    let fp_m = FunctionExtractor::fingerprint(ModuleId(1), &apply_all_passes(minified));
    let fp_s = FunctionExtractor::fingerprint(ModuleId(2), &apply_all_passes(source));
    assert_eq!(
        fp_m[0].primary.ast, fp_s[0].primary.ast,
        "Boolean(x) and !!x must converge after pipeline when `Boolean` is not shadowed"
    );
}

#[test]
fn pipeline_aligns_number_call_with_unary_plus_when_not_shadowed() {
    let minified = "function f(x) { return Number(x); }";
    let source = "function f(x) { return +x; }";
    let fp_m = FunctionExtractor::fingerprint(ModuleId(1), &apply_all_passes(minified));
    let fp_s = FunctionExtractor::fingerprint(ModuleId(2), &apply_all_passes(source));
    assert_eq!(
        fp_m[0].primary.ast, fp_s[0].primary.ast,
        "Number(x) and +x must converge after pipeline when `Number` is not shadowed"
    );
}

#[test]
fn pipeline_aligns_nullish_chain_with_loose_null_check() {
    // Minified source uses the explicit `=== null || === void 0` form;
    // hand-written source uses the compact `== null`. After the full
    // pipeline they should reach the same canonical form.
    let minified = "function f(a) { return a === null || a === void 0; }";
    let source = "function f(a) { return a == null; }";
    let fp_m = FunctionExtractor::fingerprint(ModuleId(1), &apply_all_passes(minified));
    let fp_s = FunctionExtractor::fingerprint(ModuleId(2), &apply_all_passes(source));
    assert_eq!(
        fp_m[0].primary.ast, fp_s[0].primary.ast,
        "explicit nullish chain and `== null` must converge after pipeline"
    );
}

#[test]
fn pipeline_aligns_combined_guarded_rewrites() {
    // A function that combines multiple guarded patterns. The minified
    // form uses `void 0`, `Boolean(...)`, and the explicit nullish
    // chain. The hand-written source uses `undefined`, `!!`, and
    // `== null`. After the full pipeline (which only applies the
    // guarded rewrites when `undefined`/`Boolean` are proven
    // unshadowed) the two forms must converge.
    let minified = r#"
        function f(a, b) {
            if (a === null || a === void 0) return void 0;
            return Boolean(b);
        }
    "#;
    let source = r#"
        function f(a, b) {
            if (a == null) return undefined;
            return !!b;
        }
    "#;
    let fp_m = FunctionExtractor::fingerprint(ModuleId(1), &apply_all_passes(minified));
    let fp_s = FunctionExtractor::fingerprint(ModuleId(2), &apply_all_passes(source));
    assert_eq!(
        fp_m[0].primary.ast, fp_s[0].primary.ast,
        "combined guarded rewrites must converge after pipeline"
    );
}

#[test]
fn pipeline_aligns_typeof_local_undefined_with_strict_equality() {
    // Minified source uses `typeof x === "undefined"` for a local
    // variable check; hand-written source uses `x === undefined`.
    // The guarded pass proves both preconditions hold (`x` is a
    // local param, `undefined` is unshadowed) and the rewrite
    // converges the two forms.
    let minified = r#"function f(x) { return typeof x === "undefined"; }"#;
    let source = r#"function f(x) { return x === undefined; }"#;
    let fp_m = FunctionExtractor::fingerprint(ModuleId(1), &apply_all_passes(minified));
    let fp_s = FunctionExtractor::fingerprint(ModuleId(2), &apply_all_passes(source));
    assert_eq!(
        fp_m[0].primary.ast, fp_s[0].primary.ast,
        "typeof X === 'undefined' and X === undefined must converge for local X"
    );
}

#[test]
fn pipeline_keeps_typeof_global_distinct_from_strict_equality() {
    // For an *undeclared* identifier the guarded pass bails out —
    // typeof's ReferenceError-safety is observable. The two forms
    // therefore stay distinct.
    let minified = r#"function f() { return typeof globalName === "undefined"; }"#;
    let source = r#"function f() { return globalName === undefined; }"#;
    let fp_m = FunctionExtractor::fingerprint(ModuleId(1), &apply_all_passes(minified));
    let fp_s = FunctionExtractor::fingerprint(ModuleId(2), &apply_all_passes(source));
    assert_ne!(
        fp_m[0].primary.ast, fp_s[0].primary.ast,
        "guarded pass must NOT collapse typeof when X is an undeclared global"
    );
}

#[test]
fn pipeline_aligns_negated_conditional_with_swapped_arms() {
    // `!c ? a : b` and `c ? b : a` are spec-equivalent: both
    // evaluate c via ToBoolean once, return the same value for
    // every operand. The `conditional_negation_flipped` pass
    // canonicalises to the unnegated form so the minified and
    // hand-written versions converge.
    let minified = "function f(c, a, b) { return !c ? a : b; }";
    let source = "function f(c, a, b) { return c ? b : a; }";
    let fp_m = FunctionExtractor::fingerprint(ModuleId(1), &apply_all_passes(minified));
    let fp_s = FunctionExtractor::fingerprint(ModuleId(2), &apply_all_passes(source));
    assert_eq!(
        fp_m[0].primary.ast, fp_s[0].primary.ast,
        "!c ? a : b must converge with c ? b : a after pipeline"
    );
}

#[test]
fn pipeline_aligns_finite_for_with_while_when_no_init_or_update() {
    // `for (; cond; ) body` and `while (cond) body` are spec-equivalent.
    // `for_to_while_finite_canonical` lifts the for to a while so both
    // forms converge on the same AST shape and structural_anchor counts.
    let minified = "function f(n) { for (; n > 0; ) { handle(n); n--; } }";
    let source = "function f(n) { while (n > 0) { handle(n); n--; } }";
    let fp_m = FunctionExtractor::fingerprint(ModuleId(1), &apply_all_passes(minified));
    let fp_s = FunctionExtractor::fingerprint(ModuleId(2), &apply_all_passes(source));
    assert_eq!(
        fp_m[0].primary.ast, fp_s[0].primary.ast,
        "for(;cond;) body must converge with while(cond) body after pipeline"
    );
    assert_eq!(
        fp_m[0].primary.structural_anchor, fp_s[0].primary.structural_anchor,
        "structural_anchor (which tracks loop counts) must also converge"
    );
}

#[test]
fn pipeline_aligns_nullish_assignment_with_explicit_if() {
    // Minified or hand-written source might use either form; the
    // `nullish_assignment_compacted` pass canonicalises both to the
    // compact `??=` so they converge.
    let explicit = "function f(x, val) { if (x == null) x = val; }";
    let compact = "function f(x, val) { x ??= val; }";
    let fp_e = FunctionExtractor::fingerprint(ModuleId(1), &apply_all_passes(explicit));
    let fp_c = FunctionExtractor::fingerprint(ModuleId(2), &apply_all_passes(compact));
    assert_eq!(
        fp_e[0].primary.ast, fp_c[0].primary.ast,
        "if(x==null) x=val and x??=val must converge after pipeline"
    );
}

#[test]
fn pipeline_aligns_nullish_assignment_on_member_with_explicit_if() {
    let explicit = "function f(obj, val) { if (obj.field == null) obj.field = val; }";
    let compact = "function f(obj, val) { obj.field ??= val; }";
    let fp_e = FunctionExtractor::fingerprint(ModuleId(1), &apply_all_passes(explicit));
    let fp_c = FunctionExtractor::fingerprint(ModuleId(2), &apply_all_passes(compact));
    assert_eq!(
        fp_e[0].primary.ast, fp_c[0].primary.ast,
        "if(obj.field==null) obj.field=val must converge with obj.field??=val"
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
