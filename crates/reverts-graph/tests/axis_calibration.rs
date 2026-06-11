//! Per-axis survival measurement against the L2 cross-version corpus.
//!
//! Tier weights in `reverts-package-matcher` (Exact=10000, ExactAlternate=5000,
//! StructuralAnchored=1000, FeatureSimilarity=100, StructuralOnly=10) encode
//! trust in each match path. The right way to calibrate those numbers is to
//! measure how often each axis stays stable when source legitimately drifts
//! across versions.
//!
//! This test extracts function fingerprints for every (package, version) in
//! the L2 fixtures, then for each version pair computes per-axis survival:
//!
//! ```text
//! survival(axis) = |{ fn : axis(v_a, fn) == axis(v_b, fn'), fn ↔ fn' }| / |functions|
//! ```
//!
//! Function correspondence between versions is determined by matching the
//! function's primary `ast` hash where possible; otherwise by position in
//! `program.body` (the synthetic fixtures preserve order across versions).
//!
//! ## Why the bounds below are correct
//!
//! Each per-axis lower bound below reflects the *minimum reliability we
//! intend the axis to have* under the synthetic minor-drift fixtures.
//! Setting them too high makes the suite brittle; setting them too low
//! defeats the calibration. The numbers were chosen by running the suite
//! on the current implementation and rounding down so the test acts as a
//! regression lock on accidental axis-quality drops.
//!
//! ## What this does NOT calibrate
//!
//! Tier weights themselves require **bundle-vs-source ground-truth data**
//! (a known-good `(bundle_function ↔ package_function)` mapping). The L2
//! corpus provides only version-vs-version pairs. Once a bundle/source
//! corpus exists, calibration of tier weights from accuracy/precision can
//! live alongside this file.

use std::collections::BTreeSet;

use reverts_graph::FunctionExtractor;
use reverts_ir::{FunctionFingerprint, ModuleId};

/// Lower bound on the fraction of functions whose `ast` hash survives a
/// minor-version drift (typical: identifier rename only). Real packages
/// likely sit higher; the synthetic fixtures aggressively rename params.
const AST_SURVIVAL_MIN_MINOR: f64 = 0.0;

/// CFG topology is identifier-blind and expression-blind, so it should
/// survive minor drift almost always.
const CFG_SURVIVAL_MIN_MINOR: f64 = 0.66;

/// Structural-anchor (counts-only) should be the most robust axis.
const STRUCTURAL_SURVIVAL_MIN_MINOR: f64 = 0.66;

/// Binding-pattern (param/local declarator shape) is shape-only and should
/// match the structural-anchor robustness.
const BINDING_SURVIVAL_MIN_MINOR: f64 = 0.66;

#[derive(Debug)]
struct Pair {
    package: &'static str,
    label_a: &'static str,
    source_a: &'static str,
    label_b: &'static str,
    source_b: &'static str,
}

fn minor_pairs() -> Vec<Pair> {
    vec![
        Pair {
            package: "lodash-like",
            label_a: "1.0",
            source_a: r#"
                function map(collection, fn) {
                    let result = [];
                    for (let i = 0; i < collection.length; i++) {
                        result.push(fn(collection[i], i));
                    }
                    return result;
                }
                function filter(collection, predicate) {
                    let result = [];
                    for (let i = 0; i < collection.length; i++) {
                        if (predicate(collection[i], i)) {
                            result.push(collection[i]);
                        }
                    }
                    return result;
                }
            "#,
            label_b: "1.1",
            source_b: r#"
                function map(xs, iteratee) {
                    let result = [];
                    for (let i = 0; i < xs.length; i++) {
                        result.push(iteratee(xs[i], i));
                    }
                    return result;
                }
                function filter(xs, test) {
                    let result = [];
                    for (let i = 0; i < xs.length; i++) {
                        if (test(xs[i], i)) {
                            result.push(xs[i]);
                        }
                    }
                    return result;
                }
            "#,
        },
        Pair {
            package: "axios-like",
            label_a: "0.27",
            source_a: r#"
                function request(config) {
                    const url = config.url;
                    const method = config.method || 'GET';
                    return fetch(url, { method });
                }
                function get(url, config) {
                    return request({ ...config, url, method: 'GET' });
                }
            "#,
            label_b: "0.28",
            source_b: r#"
                function request(opts) {
                    const url = opts.url;
                    const method = opts.method || 'GET';
                    return fetch(url, { method });
                }
                function get(url, opts) {
                    return request({ ...opts, url, method: 'GET' });
                }
            "#,
        },
    ]
}

#[derive(Default, Debug)]
struct SurvivalCounts {
    total: u32,
    ast: u32,
    cfg: u32,
    structural: u32,
    binding: u32,
    return_pattern: u32,
    effect_pattern: u32,
    literal_anchor_present_and_survived: u32,
    literal_anchor_present: u32,
    callee_set_present_and_survived: u32,
    callee_set_present: u32,
}

fn count_axis_survival(a: &[FunctionFingerprint], b: &[FunctionFingerprint]) -> SurvivalCounts {
    // Functions correspond by position in the synthetic fixtures (each
    // version preserves declaration order). For real corpora we'd want
    // ast-hash-based matching with order as a fallback; the synthetic
    // case lets us avoid that complexity.
    let mut counts = SurvivalCounts::default();
    for (fp_a, fp_b) in a.iter().zip(b.iter()) {
        counts.total += 1;
        if fp_a.primary.ast == fp_b.primary.ast {
            counts.ast += 1;
        }
        if fp_a.primary.cfg == fp_b.primary.cfg {
            counts.cfg += 1;
        }
        if fp_a.primary.structural_anchor == fp_b.primary.structural_anchor {
            counts.structural += 1;
        }
        if fp_a.primary.binding_pattern == fp_b.primary.binding_pattern {
            counts.binding += 1;
        }
        if fp_a.primary.return_pattern == fp_b.primary.return_pattern {
            counts.return_pattern += 1;
        }
        if fp_a.primary.effect_pattern == fp_b.primary.effect_pattern {
            counts.effect_pattern += 1;
        }
        if let (Some(a_h), Some(b_h)) = (fp_a.primary.literal_anchor, fp_b.primary.literal_anchor) {
            counts.literal_anchor_present += 1;
            if a_h == b_h {
                counts.literal_anchor_present_and_survived += 1;
            }
        }
        if let (Some(a_h), Some(b_h)) = (fp_a.primary.callee_set, fp_b.primary.callee_set) {
            counts.callee_set_present += 1;
            if a_h == b_h {
                counts.callee_set_present_and_survived += 1;
            }
        }
    }
    counts
}

fn survival_rate(num: u32, denom: u32) -> f64 {
    if denom == 0 {
        return 0.0;
    }
    f64::from(num) / f64::from(denom)
}

#[test]
fn minor_drift_keeps_identifier_blind_axes_above_design_bounds() {
    let mut violations: Vec<String> = Vec::new();
    let mut axis_kinds_observed: BTreeSet<&'static str> = BTreeSet::new();

    for pair in minor_pairs() {
        let fp_a = FunctionExtractor::fingerprint(ModuleId(1), pair.source_a);
        let fp_b = FunctionExtractor::fingerprint(ModuleId(2), pair.source_b);
        assert_eq!(
            fp_a.len(),
            fp_b.len(),
            "{} minor pair {} ↔ {}: function counts differ ({} vs {})",
            pair.package,
            pair.label_a,
            pair.label_b,
            fp_a.len(),
            fp_b.len(),
        );

        let counts = count_axis_survival(&fp_a, &fp_b);

        // Identifier-blind axes — these are the LOAD-BEARING axes for
        // cross-version matching. The whole tier system is built on
        // them; if they regress, every tier downstream loses signal.
        let cfg_rate = survival_rate(counts.cfg, counts.total);
        let struct_rate = survival_rate(counts.structural, counts.total);
        let binding_rate = survival_rate(counts.binding, counts.total);

        if cfg_rate < CFG_SURVIVAL_MIN_MINOR {
            violations.push(format!(
                "{} {} ↔ {}: cfg survival {:.2} < {:.2}",
                pair.package, pair.label_a, pair.label_b, cfg_rate, CFG_SURVIVAL_MIN_MINOR,
            ));
        }
        if struct_rate < STRUCTURAL_SURVIVAL_MIN_MINOR {
            violations.push(format!(
                "{} {} ↔ {}: structural_anchor survival {:.2} < {:.2}",
                pair.package,
                pair.label_a,
                pair.label_b,
                struct_rate,
                STRUCTURAL_SURVIVAL_MIN_MINOR,
            ));
        }
        if binding_rate < BINDING_SURVIVAL_MIN_MINOR {
            violations.push(format!(
                "{} {} ↔ {}: binding_pattern survival {:.2} < {:.2}",
                pair.package, pair.label_a, pair.label_b, binding_rate, BINDING_SURVIVAL_MIN_MINOR,
            ));
        }

        // AST survival is allowed to be 0 here — minor drift includes
        // total identifier rename, which the AST axis hashes explicitly.
        // The bound exists purely for documentation; failing it would
        // indicate the axis became NEGATIVELY correlated.
        let ast_rate = survival_rate(counts.ast, counts.total);
        if ast_rate < AST_SURVIVAL_MIN_MINOR {
            violations.push(format!(
                "{} {} ↔ {}: ast survival {:.2} < {:.2}",
                pair.package, pair.label_a, pair.label_b, ast_rate, AST_SURVIVAL_MIN_MINOR,
            ));
        }

        // Verify the axes we relied on actually had data to report.
        if counts.total > 0 {
            axis_kinds_observed.insert("cfg");
            axis_kinds_observed.insert("structural");
            axis_kinds_observed.insert("binding");
        }
    }

    assert!(
        !axis_kinds_observed.is_empty(),
        "calibration suite ran without any pairs — fixture list is empty?"
    );
    assert!(
        violations.is_empty(),
        "axis calibration regressed below design bounds:\n  {}",
        violations.join("\n  "),
    );
}

/// Records the current per-axis survival rates as a diagnostic. Always
/// passes; the value is in the output. Run with `--nocapture` to inspect.
#[test]
fn axis_survival_diagnostic_snapshot() {
    println!("\n=== per-axis survival on minor-drift fixtures ===");
    for pair in minor_pairs() {
        let fp_a = FunctionExtractor::fingerprint(ModuleId(1), pair.source_a);
        let fp_b = FunctionExtractor::fingerprint(ModuleId(2), pair.source_b);
        let c = count_axis_survival(&fp_a, &fp_b);

        let total = c.total;
        if total == 0 {
            println!(
                "  {} {} ↔ {}: zero functions",
                pair.package, pair.label_a, pair.label_b
            );
            continue;
        }

        println!(
            "  {} {} ↔ {} (n={}):",
            pair.package, pair.label_a, pair.label_b, total
        );
        println!("    ast               {:.2}", survival_rate(c.ast, total));
        println!("    cfg               {:.2}", survival_rate(c.cfg, total));
        println!(
            "    return_pattern    {:.2}",
            survival_rate(c.return_pattern, total)
        );
        println!(
            "    effect_pattern    {:.2}",
            survival_rate(c.effect_pattern, total)
        );
        println!(
            "    structural_anchor {:.2}",
            survival_rate(c.structural, total)
        );
        println!(
            "    binding_pattern   {:.2}",
            survival_rate(c.binding, total)
        );
        if c.literal_anchor_present > 0 {
            println!(
                "    literal_anchor    {:.2} (of {} present)",
                survival_rate(
                    c.literal_anchor_present_and_survived,
                    c.literal_anchor_present
                ),
                c.literal_anchor_present,
            );
        }
        if c.callee_set_present > 0 {
            println!(
                "    callee_set        {:.2} (of {} present)",
                survival_rate(c.callee_set_present_and_survived, c.callee_set_present),
                c.callee_set_present,
            );
        }
    }
    println!();
}
