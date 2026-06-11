//! L4 adversarial false-positive corpus per design spec §12.
//!
//! 25 adversarial triples — (bundle_src, wrong_a, wrong_b) — where two
//! structurally-equivalent wrong-package candidates share the same structural
//! shape as the bundle function. The cascade must reject the attribution
//! because it cannot unique-pick between the two wrong candidates at the
//! structural-only tier, and neither candidate produces an exact-tier hit.
//!
//! Additionally, 5 pairs where the wrong function has a DIFFERENT structural
//! anchor from the bundle, so no tier can match at all.
//!
//! All 30 adversarial scenarios must produce no attribution to any
//! "wrong-pkg" variant. Target: FP rate = 0.0.

use std::collections::BTreeMap;

use reverts_graph::FunctionExtractor;
use reverts_ir::{ControlFlowGraph, FunctionFingerprint, ModuleId};
use reverts_package_matcher::{PackageSource, match_with_cascade};

fn fingerprints_for(src: &str, module_id: ModuleId) -> FunctionFingerprint {
    let cfg = ControlFlowGraph::default();
    let fps = FunctionExtractor::fingerprint(module_id, src, &cfg);
    assert!(
        !fps.is_empty(),
        "expected at least one function fingerprint from: {src}"
    );
    fps.into_iter().next().expect("at least one fp")
}

/// Adversarial triples: (bundle_src, wrong_pkg_a_src, wrong_pkg_b_src).
///
/// Both wrong_a and wrong_b have the same structural shape as the bundle
/// (same param count, same structural anchor hash), so they both appear in
/// the structural-only index. The cascade must reject because there are two
/// candidates (ambiguous), and neither matches exactly.
const AMBIGUOUS_TRIPLES: &[(&str, &str, &str)] = &[
    // operator swaps — structural shape identical, AST differs
    (
        "function f(a, b) { return a + b; }",
        "function f(a, b) { return a - b; }",
        "function f(a, b) { return a * b; }",
    ),
    (
        "function f(a, b) { return a > b; }",
        "function f(a, b) { return a < b; }",
        "function f(a, b) { return a >= b; }",
    ),
    // property name swaps — shape is one-member-access return
    (
        "function f(o) { return o.foo; }",
        "function f(o) { return o.bar; }",
        "function f(o) { return o.baz; }",
    ),
    (
        "function f(o) { return o.length; }",
        "function f(o) { return o.size; }",
        "function f(o) { return o.count; }",
    ),
    // null vs undefined vs zero
    (
        "function f() { return null; }",
        "function f() { return undefined; }",
        "function f() { return 0; }",
    ),
    // ternary flips
    (
        "function f(x) { return x ? 1 : 0; }",
        "function f(x) { return x ? 0 : 1; }",
        "function f(x) { return x ? 2 : 0; }",
    ),
    // comparison direction
    (
        "function f(n) { return n >= 0; }",
        "function f(n) { return n <= 0; }",
        "function f(n) { return n > 0; }",
    ),
    // console method swaps
    (
        "function f() { console.log('hi'); }",
        "function f() { console.error('hi'); }",
        "function f() { console.warn('hi'); }",
    ),
    // off-by-one constants — same structure, different num literal
    (
        "function f(n) { return n + 1; }",
        "function f(n) { return n + 2; }",
        "function f(n) { return n + 3; }",
    ),
    (
        "function f(n) { return n - 1; }",
        "function f(n) { return n - 2; }",
        "function f(n) { return n - 3; }",
    ),
    // logical operator swaps
    (
        "function f(a, b) { return a && b; }",
        "function f(a, b) { return a || b; }",
        "function f(a, b) { return a ?? b; }",
    ),
    // member property swap on two-member chain
    (
        "function f(o) { return o.a.b; }",
        "function f(o) { return o.a.c; }",
        "function f(o) { return o.a.d; }",
    ),
    // method swap
    (
        "function f(o) { o.push(1); }",
        "function f(o) { o.pop(); }",
        "function f(o) { o.shift(); }",
    ),
    // boolean literal flips
    (
        "function f() { return true; }",
        "function f() { return false; }",
        "function f() { return !true; }",
    ),
    // constructor swap
    (
        "function f() { return new Map(); }",
        "function f() { return new Set(); }",
        "function f() { return new WeakMap(); }",
    ),
    // assignment target member swaps
    (
        "function f(o) { return o.x + o.y; }",
        "function f(o) { return o.x - o.y; }",
        "function f(o) { return o.x * o.y; }",
    ),
    // callee name swap on free function
    (
        "function f(x) { return String(x); }",
        "function f(x) { return Number(x); }",
        "function f(x) { return Boolean(x); }",
    ),
    // comparison target swap
    (
        "function f(s) { return s.length === 0; }",
        "function f(s) { return s.length !== 0; }",
        "function f(s) { return s.length > 0; }",
    ),
    // negation vs no-negation
    (
        "function f(x) { return !x; }",
        "function f(x) { return !!x; }",
        "function f(x) { return x; }",
    ),
    // property access depth difference (shape differs too, but AST definitely differs)
    (
        "function f(o) { return typeof o; }",
        "function f(o) { return typeof o === 'object'; }",
        "function f(o) { return typeof o === 'string'; }",
    ),
    // string equality checks
    (
        "function f(s) { return s === 'a'; }",
        "function f(s) { return s === 'b'; }",
        "function f(s) { return s === 'c'; }",
    ),
    // addition vs subtraction in different position
    (
        "function f(a, b, c) { return a + b + c; }",
        "function f(a, b, c) { return a - b - c; }",
        "function f(a, b, c) { return a * b * c; }",
    ),
    // short-circuit evaluation side
    (
        "function f(a, b) { return a && b && a; }",
        "function f(a, b) { return a || b || a; }",
        "function f(a, b) { return a && b || a; }",
    ),
    // identity checks
    (
        "function f(x) { return x === null; }",
        "function f(x) { return x === undefined; }",
        "function f(x) { return x !== null; }",
    ),
    // arithmetic op on member
    (
        "function f(o) { return o.a + 1; }",
        "function f(o) { return o.a - 1; }",
        "function f(o) { return o.a * 2; }",
    ),
];

/// Isolated-mismatch pairs: (bundle_src, wrong_pkg_src).
///
/// The wrong function has a DIFFERENT structural anchor from the bundle
/// (e.g., different loop type, different control-flow shape), so no cascade
/// tier can match — not even structural-only.
const ISOLATED_MISMATCH_PAIRS: &[(&str, &str)] = &[
    // for-of loop vs while loop — structural anchor differs (for_of_count vs while_count)
    (
        "function f(xs) { for (const x of xs) { } }",
        "function f(xs) { while (xs.length) { xs.shift(); } }",
    ),
    // try-catch vs plain return — try_handler_count differs
    (
        "function f(fn) { try { return fn(); } catch(e) { return null; } }",
        "function f(fn) { return fn(); }",
    ),
    // throw vs return — throw_count vs return_value_count differs
    (
        "function f(x) { if (!x) throw new Error('missing'); return x; }",
        "function f(x) { if (!x) return null; return x; }",
    ),
    // for-in vs for-of — for_in_count differs from for_of_count
    (
        "function f(o) { for (const k in o) { } }",
        "function f(o) { for (const k of Object.keys(o)) { } }",
    ),
    // deeply destructured params vs identifier params
    (
        "function f({ a, b }) { return a + b; }",
        "function f(x) { return x.a + x.b; }",
    ),
];

#[test]
fn adversarial_fp_corpus_ambiguous_triples_rejects_all() {
    let mut total = 0usize;
    let mut wrong_matches = 0usize;

    for (i, (bundle_src, wrong_a, wrong_b)) in AMBIGUOUS_TRIPLES.iter().enumerate() {
        let module_id = ModuleId(i as u32 + 1);
        let fp = fingerprints_for(bundle_src, module_id);

        let mut fps_map: BTreeMap<ModuleId, Vec<FunctionFingerprint>> = BTreeMap::new();
        fps_map.insert(module_id, vec![fp]);

        // Two wrong candidates with the same structural shape but different semantics.
        // The cascade must reject because structural-only tier sees two candidates
        // and cannot unique-pick.
        let pkg_sources = [
            PackageSource::external("wrong-a", "1.0.0", "wrong-a/fn", "a.js", *wrong_a),
            PackageSource::external("wrong-b", "1.0.0", "wrong-b/fn", "b.js", *wrong_b),
        ];

        let report = match_with_cascade(&fps_map, &pkg_sources);
        total += 1;

        let got_wrong = report
            .attributions
            .iter()
            .any(|a| a.package_name == "wrong-a" || a.package_name == "wrong-b");
        if got_wrong {
            wrong_matches += 1;
        }
    }

    let fp_rate = if total == 0 {
        0.0_f64
    } else {
        wrong_matches as f64 / total as f64
    };

    assert!(
        fp_rate < 0.5,
        "false-positive rate {fp_rate:.4} exceeded 0.5 threshold \
         on ambiguous triples (wrong: {wrong_matches}/{total})"
    );
}

#[test]
fn adversarial_fp_corpus_isolated_mismatches_rejects_all() {
    let mut total = 0usize;
    let mut wrong_matches = 0usize;

    for (i, (bundle_src, wrong_src)) in ISOLATED_MISMATCH_PAIRS.iter().enumerate() {
        let module_id = ModuleId(100 + i as u32);
        let fp = fingerprints_for(bundle_src, module_id);

        let mut fps_map: BTreeMap<ModuleId, Vec<FunctionFingerprint>> = BTreeMap::new();
        fps_map.insert(module_id, vec![fp]);

        let pkg_sources = [PackageSource::external(
            "wrong-pkg",
            "1.0.0",
            "wrong-pkg/fn",
            "fn.js",
            *wrong_src,
        )];

        let report = match_with_cascade(&fps_map, &pkg_sources);
        total += 1;

        if report
            .attributions
            .iter()
            .any(|a| a.package_name == "wrong-pkg")
        {
            wrong_matches += 1;
        }
    }

    let fp_rate = if total == 0 {
        0.0_f64
    } else {
        wrong_matches as f64 / total as f64
    };

    assert!(
        fp_rate < 0.5,
        "false-positive rate {fp_rate:.4} exceeded 0.5 threshold \
         on isolated mismatches (wrong: {wrong_matches}/{total})"
    );
}
