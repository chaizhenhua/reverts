//! L1 paired-fixture suite per design spec §12.
//!
//! For each of the 12 fingerprint axes, verify pair-wise that semantically
//! equivalent inputs collide and semantically distinct inputs differ.

use oxc_allocator::Allocator;
use oxc_ast::ast::Statement;
use oxc_parser::Parser;
use oxc_span::SourceType;
use reverts_graph::fingerprint::{
    access, ast, binding_pattern, callee_set, cfg, effect_pattern, literal_anchor, literal_shape,
    return_pattern, structural_anchor, throw_set,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Hash the FunctionBody of the first function declaration in `src` using the
/// provided closure. The closure receives `(params, body)` references tied to
/// the lifetime of the allocator, which is created and owned on the stack by
/// this helper.
///
/// A macro is used so that the `Allocator` lives in the caller's stack frame,
/// keeping the AST references valid for the duration of the closure.
macro_rules! with_first_fn {
    ($alloc:ident, $src:expr, |$params:ident, $body:ident| $block:expr) => {{
        let $alloc = Allocator::default();
        let parsed = Parser::new(&$alloc, $src, SourceType::default()).parse();
        assert!(
            parsed.errors.is_empty(),
            "parse errors: {:?}",
            parsed.errors
        );
        let func = parsed
            .program
            .body
            .iter()
            .find_map(|s| {
                if let Statement::FunctionDeclaration(f) = s {
                    Some(f)
                } else {
                    None
                }
            })
            .expect("no function found in source");
        let $params = &func.params;
        let $body = func.body.as_deref().expect("function must have body");
        $block
    }};
}

// ---------------------------------------------------------------------------
// AST axis (12 total → 4 covered here; unit tests in ast.rs cover the rest)
// ---------------------------------------------------------------------------

#[test]
fn axis_ast_collides_for_alpha_renamed_pair() {
    let h1 = with_first_fn!(a, "function f(a, b) { return a + b; }", |_p, b| {
        ast::compute(b)
    });
    let h2 = with_first_fn!(a, "function g(x, y) { return x + y; }", |_p, b| {
        ast::compute(b)
    });
    assert_eq!(h1, h2);
}

#[test]
fn axis_ast_differs_for_different_operator() {
    let h1 = with_first_fn!(a, "function f(a, b) { return a + b; }", |_p, b| {
        ast::compute(b)
    });
    let h2 = with_first_fn!(a, "function f(a, b) { return a - b; }", |_p, b| {
        ast::compute(b)
    });
    assert_ne!(h1, h2);
}

#[test]
fn axis_ast_differs_for_for_loop_vs_while_loop() {
    let h1 = with_first_fn!(a, "function f(xs) { for (let x of xs) { } }", |_p, b| {
        ast::compute(b)
    });
    let h2 = with_first_fn!(a, "function f(xs) { while (xs.length) {} }", |_p, b| {
        ast::compute(b)
    });
    assert_ne!(h1, h2);
}

#[test]
fn axis_ast_collides_for_only_rename_of_arg() {
    let h1 = with_first_fn!(
        a,
        "function f(arg) { let s = arg + 1; return s; }",
        |_p, b| { ast::compute(b) }
    );
    let h2 = with_first_fn!(
        a,
        "function g(qux) { let s = qux + 1; return s; }",
        |_p, b| { ast::compute(b) }
    );
    assert_eq!(h1, h2);
}

// ---------------------------------------------------------------------------
// CFG axis (function-level topology; expression-blind, identifier-blind)
// ---------------------------------------------------------------------------

#[test]
fn axis_cfg_collides_when_only_expressions_differ() {
    let h1 = with_first_fn!(
        a1,
        "function f(x) { if (x > 0) return 1; return 2; }",
        |_p, b| { cfg::compute(b) }
    );
    let h2 = with_first_fn!(
        a2,
        "function f(x) { if (x.foo()) return 'a'; return 'b'; }",
        |_p, b| { cfg::compute(b) }
    );
    assert_eq!(h1, h2);
}

#[test]
fn axis_cfg_distinguishes_if_with_else_from_if_without_else() {
    let with_else = with_first_fn!(
        a1,
        "function f(x) { if (x) return 1; else return 2; }",
        |_p, b| { cfg::compute(b) }
    );
    let no_else = with_first_fn!(
        a2,
        "function f(x) { if (x) return 1; return 2; }",
        |_p, b| { cfg::compute(b) }
    );
    assert_ne!(with_else, no_else);
}

#[test]
fn axis_cfg_distinguishes_loop_kinds() {
    let for_of = with_first_fn!(a1, "function f(xs) { for (let x of xs) {} }", |_p, b| {
        cfg::compute(b)
    });
    let while_loop = with_first_fn!(a2, "function f(xs) { while (xs.length) {} }", |_p, b| {
        cfg::compute(b)
    });
    assert_ne!(for_of, while_loop);
}

#[test]
fn axis_cfg_collides_under_identifier_rename() {
    let h1 = with_first_fn!(
        a1,
        "function f(a, b) { try { return a + b; } catch (err) { throw err; } }",
        |_p, b| { cfg::compute(b) }
    );
    let h2 = with_first_fn!(
        a2,
        "function g(x, y) { try { return x + y; } catch (z) { throw z; } }",
        |_p, b| { cfg::compute(b) }
    );
    assert_eq!(h1, h2);
}

// ---------------------------------------------------------------------------
// Return pattern axis
// ---------------------------------------------------------------------------

#[test]
fn axis_return_pattern_distinguishes_void_from_value() {
    let h1 = with_first_fn!(a, "function f() { return; }", |_p, b| {
        return_pattern::compute(b)
    });
    let h2 = with_first_fn!(a, "function f() { return 1; }", |_p, b| {
        return_pattern::compute(b)
    });
    assert_ne!(h1, h2);
}

#[test]
fn axis_return_pattern_collides_same_member_chain_bucket() {
    let h1 = with_first_fn!(a, "function f(a) { return a.x.y; }", |_p, b| {
        return_pattern::compute(b)
    });
    let h2 = with_first_fn!(a, "function f(z) { return z.q.r; }", |_p, b| {
        return_pattern::compute(b)
    });
    assert_eq!(h1, h2);
}

#[test]
fn axis_return_pattern_distinguishes_literal_from_identifier() {
    let h1 = with_first_fn!(a, "function f() { return 42; }", |_p, b| {
        return_pattern::compute(b)
    });
    let h2 = with_first_fn!(a, "function f(x) { return x; }", |_p, b| {
        return_pattern::compute(b)
    });
    assert_ne!(h1, h2);
}

#[test]
fn axis_return_pattern_collides_for_same_literal_class() {
    let h1 = with_first_fn!(a, "function f() { return 'hello'; }", |_p, b| {
        return_pattern::compute(b)
    });
    let h2 = with_first_fn!(a, "function f() { return 'world'; }", |_p, b| {
        return_pattern::compute(b)
    });
    assert_eq!(h1, h2);
}

// ---------------------------------------------------------------------------
// Effect pattern axis
// ---------------------------------------------------------------------------

#[test]
fn axis_effect_pattern_distinguishes_call_vs_pure() {
    let h1 = with_first_fn!(a, "function f() { console.log(1); }", |_p, b| {
        effect_pattern::compute(b)
    });
    let h2 = with_first_fn!(a, "function f(a, b) { return a + b; }", |_p, b| {
        effect_pattern::compute(b)
    });
    assert_ne!(h1, h2);
}

#[test]
fn axis_effect_pattern_collides_under_rename() {
    let h1 = with_first_fn!(
        a,
        "function f(a) { console.log(a); throw new Error('x'); }",
        |_p, b| { effect_pattern::compute(b) }
    );
    let h2 = with_first_fn!(
        a,
        "function g(z) { console.log(z); throw new Error('y'); }",
        |_p, b| { effect_pattern::compute(b) }
    );
    assert_eq!(h1, h2);
}

#[test]
fn axis_effect_pattern_distinguishes_await_from_sync() {
    let h1 = with_first_fn!(a, "async function f(p) { return await p; }", |_p, b| {
        effect_pattern::compute(b)
    });
    let h2 = with_first_fn!(a, "function g(p) { return p; }", |_p, b| {
        effect_pattern::compute(b)
    });
    assert_ne!(h1, h2);
}

#[test]
fn axis_effect_pattern_collides_different_throw_messages() {
    let h1 = with_first_fn!(a, "function f() { throw new Error('a'); }", |_p, b| {
        effect_pattern::compute(b)
    });
    let h2 = with_first_fn!(a, "function g() { throw new Error('b'); }", |_p, b| {
        effect_pattern::compute(b)
    });
    assert_eq!(h1, h2);
}

// ---------------------------------------------------------------------------
// Literal anchor axis
// ---------------------------------------------------------------------------

#[test]
fn axis_literal_anchor_none_without_literals() {
    let h = with_first_fn!(a, "function f(a) { return a + 1; }", |_p, b| {
        literal_anchor::compute(b)
    });
    assert!(h.is_none());
}

#[test]
fn axis_literal_anchor_present_with_long_string() {
    let h = with_first_fn!(
        a,
        "function f() { throw new Error('something long'); }",
        |_p, b| { literal_anchor::compute(b) }
    );
    assert!(h.is_some());
}

#[test]
fn axis_literal_anchor_absent_for_short_string() {
    let h = with_first_fn!(a, "function f() { return 'ab'; }", |_p, b| {
        literal_anchor::compute(b)
    });
    assert!(h.is_none(), "string shorter than 3 chars must not anchor");
}

#[test]
fn axis_literal_anchor_differs_for_different_long_strings() {
    let h1 = with_first_fn!(
        a,
        "function f() { throw new Error('first long anchor'); }",
        |_p, b| { literal_anchor::compute(b) }
    );
    let h2 = with_first_fn!(
        a,
        "function f() { throw new Error('second different anchor'); }",
        |_p, b| { literal_anchor::compute(b) }
    );
    assert_ne!(h1, h2);
}

// ---------------------------------------------------------------------------
// Access (pattern + shape) axis
// ---------------------------------------------------------------------------

#[test]
fn axis_access_pattern_differs_when_property_name_differs() {
    let (p1, _) = with_first_fn!(a, "function f(o) { return o.foo; }", |_p, b| {
        access::compute(b)
    });
    let (p2, _) = with_first_fn!(a, "function f(o) { return o.bar; }", |_p, b| {
        access::compute(b)
    });
    assert_ne!(p1, p2);
}

#[test]
fn axis_access_shape_collides_when_only_property_name_differs() {
    let (_, s1) = with_first_fn!(a, "function f(o) { return o.foo; }", |_p, b| {
        access::compute(b)
    });
    let (_, s2) = with_first_fn!(a, "function f(o) { return o.bar; }", |_p, b| {
        access::compute(b)
    });
    assert_eq!(s1, s2);
}

#[test]
fn axis_access_both_none_when_no_member_access() {
    let (p, s) = with_first_fn!(a, "function f(a, b) { return a + b; }", |_p, b| {
        access::compute(b)
    });
    assert!(p.is_none());
    assert!(s.is_none());
}

#[test]
fn axis_access_pattern_includes_depth_signal() {
    let (p1, s1) = with_first_fn!(a, "function f(o) { return o.a; }", |_p, b| {
        access::compute(b)
    });
    let (p2, s2) = with_first_fn!(a, "function f(o) { return o.a.b; }", |_p, b| {
        access::compute(b)
    });
    // Both property name and chain depth differ → both pattern and shape differ.
    assert_ne!(s1, s2, "shape must encode depth");
    assert_ne!(p1, p2, "pattern must encode name + depth");
}

// ---------------------------------------------------------------------------
// Structural anchor axis
// ---------------------------------------------------------------------------

#[test]
fn axis_structural_anchor_distinguishes_loop_kinds() {
    let h1 = with_first_fn!(a, "function f(xs) { for (let x of xs) {} }", |p, b| {
        structural_anchor::compute(p, b)
    });
    let h2 = with_first_fn!(a, "function f(xs) { while (xs.length) {} }", |p, b| {
        structural_anchor::compute(p, b)
    });
    assert_ne!(h1, h2);
}

#[test]
fn axis_structural_anchor_collides_after_rename() {
    let h1 = with_first_fn!(
        a,
        "function f(a) { try { return a; } catch(e) { throw e; } }",
        |p, b| { structural_anchor::compute(p, b) }
    );
    let h2 = with_first_fn!(
        a,
        "function g(x) { try { return x; } catch(z) { throw z; } }",
        |p, b| { structural_anchor::compute(p, b) }
    );
    assert_eq!(h1, h2);
}

#[test]
fn axis_structural_anchor_distinguishes_for_from_for_in() {
    let h1 = with_first_fn!(a, "function f(o) { for (const k in o) { } }", |p, b| {
        structural_anchor::compute(p, b)
    });
    let h2 = with_first_fn!(
        a,
        "function f(o) { for (const k of Object.keys(o)) { } }",
        |p, b| { structural_anchor::compute(p, b) }
    );
    assert_ne!(h1, h2);
}

#[test]
fn axis_structural_anchor_distinguishes_destructure_depth() {
    let h1 = with_first_fn!(a, "function f(a) { return a; }", |p, b| {
        structural_anchor::compute(p, b)
    });
    let h2 = with_first_fn!(a, "function f({ a, b }) { return a + b; }", |p, b| {
        structural_anchor::compute(p, b)
    });
    assert_ne!(h1, h2);
}

// ---------------------------------------------------------------------------
// Literal shape axis
// ---------------------------------------------------------------------------

#[test]
fn axis_literal_shape_collides_same_bucket_different_content() {
    let h1 = with_first_fn!(a, "function f() { return 'foo'; }", |_p, b| {
        literal_shape::compute(b)
    });
    let h2 = with_first_fn!(a, "function f() { return 'bar'; }", |_p, b| {
        literal_shape::compute(b)
    });
    assert_eq!(h1, h2);
}

#[test]
fn axis_literal_shape_distinguishes_string_from_number() {
    let h1 = with_first_fn!(a, "function f() { return 'foo'; }", |_p, b| {
        literal_shape::compute(b)
    });
    let h2 = with_first_fn!(a, "function f() { return 42; }", |_p, b| {
        literal_shape::compute(b)
    });
    assert_ne!(h1, h2);
}

#[test]
fn axis_literal_shape_none_when_no_literals() {
    let h = with_first_fn!(a, "function f(a) { return a; }", |_p, b| {
        literal_shape::compute(b)
    });
    assert!(h.is_none());
}

#[test]
fn axis_literal_shape_distinguishes_short_from_long_string() {
    let h1 = with_first_fn!(a, "function f() { return 'hi'; }", |_p, b| {
        literal_shape::compute(b)
    });
    let h2 = with_first_fn!(
        a,
        "function f() { return 'a-much-longer-string-value'; }",
        |_p, b| { literal_shape::compute(b) }
    );
    assert_ne!(h1, h2);
}

// ---------------------------------------------------------------------------
// Callee set axis
// ---------------------------------------------------------------------------

#[test]
fn axis_callee_set_distinguishes_static_member_names() {
    let h1 = with_first_fn!(a, "function f(o) { o.toString(); }", |_p, b| {
        callee_set::compute(b)
    });
    let h2 = with_first_fn!(a, "function f(o) { o.toJSON(); }", |_p, b| {
        callee_set::compute(b)
    });
    assert_ne!(h1, h2);
}

#[test]
fn axis_callee_set_collides_under_receiver_rename() {
    let h1 = with_first_fn!(a, "function f(o) { o.push(1); }", |_p, b| {
        callee_set::compute(b)
    });
    let h2 = with_first_fn!(a, "function f(x) { x.push(1); }", |_p, b| {
        callee_set::compute(b)
    });
    assert_eq!(h1, h2);
}

#[test]
fn axis_callee_set_none_for_no_calls() {
    let h = with_first_fn!(a, "function f(a, b) { return a + b; }", |_p, b| {
        callee_set::compute(b)
    });
    assert!(h.is_none());
}

#[test]
fn axis_callee_set_distinguishes_free_fn_from_method() {
    let h1 = with_first_fn!(a, "function f() { foo(); }", |_p, b| {
        callee_set::compute(b)
    });
    let h2 = with_first_fn!(a, "function f(o) { o.foo(); }", |_p, b| {
        callee_set::compute(b)
    });
    assert_ne!(h1, h2);
}

// ---------------------------------------------------------------------------
// Binding pattern axis
// ---------------------------------------------------------------------------

#[test]
fn axis_binding_pattern_distinguishes_destructure_from_identifier() {
    let h1 = with_first_fn!(a, "function f(a) { return a; }", |p, b| {
        binding_pattern::compute(p, b)
    });
    let h2 = with_first_fn!(a, "function f({ a, b }) { return a + b; }", |p, b| {
        binding_pattern::compute(p, b)
    });
    assert_ne!(h1, h2);
}

#[test]
fn axis_binding_pattern_collides_under_rename() {
    let h1 = with_first_fn!(
        a,
        "function f(a, b) { let x = 1; return a + b + x; }",
        |p, b| { binding_pattern::compute(p, b) }
    );
    let h2 = with_first_fn!(
        a,
        "function g(p, q) { let y = 1; return p + q + y; }",
        |p, b| { binding_pattern::compute(p, b) }
    );
    assert_eq!(h1, h2);
}

#[test]
fn axis_binding_pattern_distinguishes_ident_from_array_destructure() {
    // Identifier param produces "p:i"; array-destructure produces "p:a[2]".
    let h1 = with_first_fn!(a, "function f(a) { return a; }", |p, b| {
        binding_pattern::compute(p, b)
    });
    let h2 = with_first_fn!(a, "function f([x, y]) { return x + y; }", |p, b| {
        binding_pattern::compute(p, b)
    });
    assert_ne!(h1, h2);
}

#[test]
fn axis_binding_pattern_collides_two_ident_params_under_rename() {
    let h1 = with_first_fn!(a, "function f(x, y) { return x * y; }", |p, b| {
        binding_pattern::compute(p, b)
    });
    let h2 = with_first_fn!(a, "function g(m, n) { return m * n; }", |p, b| {
        binding_pattern::compute(p, b)
    });
    assert_eq!(h1, h2);
}

// ---------------------------------------------------------------------------
// Throw set axis
// ---------------------------------------------------------------------------

#[test]
fn axis_throw_set_distinguishes_constructor_names() {
    let h1 = with_first_fn!(a, "function f() { throw new TypeError('x'); }", |_p, b| {
        throw_set::compute(b)
    });
    let h2 = with_first_fn!(a, "function f() { throw new RangeError('x'); }", |_p, b| {
        throw_set::compute(b)
    });
    assert_ne!(h1, h2);
}

#[test]
fn axis_throw_set_collides_under_rename() {
    let h1 = with_first_fn!(a, "function f(e) { throw new Error('a'); }", |_p, b| {
        throw_set::compute(b)
    });
    let h2 = with_first_fn!(a, "function f(x) { throw new Error('b'); }", |_p, b| {
        throw_set::compute(b)
    });
    assert_eq!(h1, h2);
}

#[test]
fn axis_throw_set_none_when_no_throw() {
    let h = with_first_fn!(a, "function f(a) { return a; }", |_p, b| {
        throw_set::compute(b)
    });
    assert!(h.is_none());
}

#[test]
fn axis_throw_set_collides_same_constructor_different_message() {
    let h1 = with_first_fn!(
        a,
        "function f() { throw new RangeError('first'); }",
        |_p, b| { throw_set::compute(b) }
    );
    let h2 = with_first_fn!(
        a,
        "function g() { throw new RangeError('second'); }",
        |_p, b| { throw_set::compute(b) }
    );
    assert_eq!(h1, h2);
}
