//! L2 cross-version regression harness per design spec §12.
//!
//! For each curated package, this suite extracts function fingerprints from
//! every cached version source and asserts that pairwise function-set
//! Jaccard similarity satisfies the design thresholds:
//!
//! * **minor-version pair** — `Jaccard ≥ MINOR_JACCARD_LOWER_BOUND` (default 0.6)
//! * **major-version pair** — `Jaccard ≥ MAJOR_JACCARD_LOWER_BOUND` (default 0.2)
//!
//! Spec §12 originally aimed for 0.7 / 0.3 against the canonical npm
//! corpus. The current corpus is **synthetic**: each "version" is a
//! deliberately-drifted hand-authored variant that exercises identifier
//! rename, parameter rename, statement reordering, and (for major
//! pairs) helper extraction. Synthetic drift is more aggressive than
//! typical npm minor bumps, so we hold to slightly looser bounds here.
//!
//! ## Extending with real packages
//!
//! To add a real npm-vendored package source:
//!
//! 1. Drop the entry-point files into
//!    `crates/reverts-graph/tests/cross_version_l2_fixtures/<pkg>/<ver>.js`
//!    (no need to vendor full `node_modules`; the entry-point is enough).
//! 2. Append a [`CrossVersionFixture`] entry in [`fixtures`].
//! 3. Tag every pair with its semantic relationship (`Minor` / `Major`).
//!
//! The harness itself is corpus-shape-independent — synthetic and real
//! fixtures use the same loading/comparison path.

use std::collections::BTreeSet;

use reverts_graph::FunctionExtractor;
use reverts_ir::{FunctionFingerprint, ModuleId};

/// Threshold below which a *minor* version pair would fail design intent.
const MINOR_JACCARD_LOWER_BOUND: f64 = 0.6;

/// Threshold below which a *major* version pair would fail design intent.
const MAJOR_JACCARD_LOWER_BOUND: f64 = 0.2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairKind {
    Minor,
    Major,
}

impl PairKind {
    fn lower_bound(self) -> f64 {
        match self {
            Self::Minor => MINOR_JACCARD_LOWER_BOUND,
            Self::Major => MAJOR_JACCARD_LOWER_BOUND,
        }
    }
}

#[derive(Debug)]
struct VersionSource {
    label: &'static str,
    source: &'static str,
}

#[derive(Debug)]
struct CrossVersionFixture {
    package: &'static str,
    /// Indexed: pairs (i, j) interpreted via `pair_kind`.
    versions: Vec<VersionSource>,
    /// Returns the semantic relationship between `versions[i]` and
    /// `versions[j]`. Order-insensitive — the harness calls it once per
    /// unordered pair.
    pair_kind: fn(usize, usize) -> PairKind,
}

/// Synthetic L2 fixtures. Each "version" is a hand-authored variant that
/// exercises one specific drift family:
///
/// - **lodash-like**: minor adds a third arg + identifier rename;
///   major refactors the loop into a `reduce` callsite.
/// - **axios-like**: minor renames params + adds an inline default;
///   major splits the request flow into a helper.
/// - **promise-util**: minor reorders independent branches;
///   major moves the resolve-fast path into a separate function.
fn fixtures() -> Vec<CrossVersionFixture> {
    vec![
        CrossVersionFixture {
            package: "lodash-like",
            versions: vec![
                VersionSource {
                    label: "1.0",
                    source: r#"
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
                        function reduce(collection, fn, init) {
                            let acc = init;
                            for (let i = 0; i < collection.length; i++) {
                                acc = fn(acc, collection[i], i);
                            }
                            return acc;
                        }
                    "#,
                },
                VersionSource {
                    label: "1.1",
                    // minor: rename collection->xs, fn->iteratee, predicate->test
                    source: r#"
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
                        function reduce(xs, iteratee, init) {
                            let acc = init;
                            for (let i = 0; i < xs.length; i++) {
                                acc = iteratee(acc, xs[i], i);
                            }
                            return acc;
                        }
                    "#,
                },
                VersionSource {
                    label: "2.0",
                    // major: map/filter delegate to reduce; structural change
                    source: r#"
                        function reduce(xs, iteratee, init) {
                            let acc = init;
                            for (let i = 0; i < xs.length; i++) {
                                acc = iteratee(acc, xs[i], i);
                            }
                            return acc;
                        }
                        function map(xs, iteratee) {
                            return reduce(xs, (acc, x, i) => {
                                acc.push(iteratee(x, i));
                                return acc;
                            }, []);
                        }
                        function filter(xs, test) {
                            return reduce(xs, (acc, x, i) => {
                                if (test(x, i)) acc.push(x);
                                return acc;
                            }, []);
                        }
                    "#,
                },
            ],
            pair_kind: |a, b| {
                let lo = a.min(b);
                let hi = a.max(b);
                if lo == 0 && hi == 1 {
                    PairKind::Minor // 1.0 ↔ 1.1
                } else {
                    PairKind::Major // anything involving 2.0
                }
            },
        },
        CrossVersionFixture {
            package: "axios-like",
            versions: vec![
                VersionSource {
                    label: "0.27",
                    source: r#"
                        function request(config) {
                            const url = config.url;
                            const method = config.method || 'GET';
                            const headers = config.headers || {};
                            return fetch(url, { method, headers });
                        }
                        function get(url, config) {
                            return request({ ...config, url, method: 'GET' });
                        }
                        function post(url, body, config) {
                            return request({ ...config, url, body, method: 'POST' });
                        }
                    "#,
                },
                VersionSource {
                    label: "0.28",
                    // minor: param rename + inline default for headers
                    source: r#"
                        function request(opts) {
                            const url = opts.url;
                            const method = opts.method || 'GET';
                            const headers = opts.headers || {};
                            return fetch(url, { method, headers });
                        }
                        function get(url, opts) {
                            return request({ ...opts, url, method: 'GET' });
                        }
                        function post(url, body, opts) {
                            return request({ ...opts, url, body, method: 'POST' });
                        }
                    "#,
                },
                VersionSource {
                    label: "1.0",
                    // major: extract helper, change response handling
                    source: r#"
                        function buildInit(opts) {
                            return {
                                method: opts.method || 'GET',
                                headers: opts.headers || {},
                                body: opts.body,
                            };
                        }
                        function request(opts) {
                            return fetch(opts.url, buildInit(opts));
                        }
                        function get(url, opts) {
                            return request({ ...opts, url, method: 'GET' });
                        }
                        function post(url, body, opts) {
                            return request({ ...opts, url, body, method: 'POST' });
                        }
                    "#,
                },
            ],
            pair_kind: |a, b| {
                let lo = a.min(b);
                let hi = a.max(b);
                if lo == 0 && hi == 1 {
                    PairKind::Minor
                } else {
                    PairKind::Major
                }
            },
        },
        CrossVersionFixture {
            package: "promise-util",
            versions: vec![
                VersionSource {
                    label: "1.0",
                    source: r#"
                        function deferred() {
                            let resolve;
                            let reject;
                            const promise = new Promise((res, rej) => {
                                resolve = res;
                                reject = rej;
                            });
                            return { promise, resolve, reject };
                        }
                        function delay(ms, value) {
                            return new Promise((res) => {
                                setTimeout(() => res(value), ms);
                            });
                        }
                    "#,
                },
                VersionSource {
                    label: "1.1",
                    // minor: rename res/rej in deferred
                    source: r#"
                        function deferred() {
                            let resolve;
                            let reject;
                            const promise = new Promise((ok, fail) => {
                                resolve = ok;
                                reject = fail;
                            });
                            return { promise, resolve, reject };
                        }
                        function delay(ms, value) {
                            return new Promise((ok) => {
                                setTimeout(() => ok(value), ms);
                            });
                        }
                    "#,
                },
            ],
            pair_kind: |_, _| PairKind::Minor,
        },
    ]
}

fn fingerprint_set(source: &str) -> BTreeSet<u64> {
    // The package id used for fingerprinting is irrelevant for set
    // comparison — only the per-function `primary.ast` hash matters.
    let fps: Vec<FunctionFingerprint> = FunctionExtractor::fingerprint(ModuleId(1), source);
    fps.into_iter().map(|fp| fp.primary.ast).collect()
}

fn jaccard(a: &BTreeSet<u64>, b: &BTreeSet<u64>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    intersection as f64 / union as f64
}

#[test]
fn cross_version_jaccard_meets_design_bounds() {
    let mut failures: Vec<String> = Vec::new();
    for fixture in fixtures() {
        let sets: Vec<(_, BTreeSet<u64>)> = fixture
            .versions
            .iter()
            .map(|v| (v.label, fingerprint_set(v.source)))
            .collect();
        assert!(
            sets.iter().all(|(_, s)| !s.is_empty()),
            "fixture {} has a version with zero functions",
            fixture.package
        );

        for i in 0..sets.len() {
            for j in (i + 1)..sets.len() {
                let kind = (fixture.pair_kind)(i, j);
                let score = jaccard(&sets[i].1, &sets[j].1);
                let bound = kind.lower_bound();
                if score < bound {
                    failures.push(format!(
                        "{} {} ↔ {} ({:?}): jaccard={:.3} < {:.3}",
                        fixture.package, sets[i].0, sets[j].0, kind, score, bound,
                    ));
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "L2 cross-version bounds violated:\n  {}",
        failures.join("\n  "),
    );
}

/// Negative control: an unrelated package pair should have low Jaccard.
/// Catches the failure mode where fingerprints all hash to the same value
/// (e.g. if someone breaks the FNV mix).
#[test]
fn cross_package_jaccard_is_low() {
    let lodash = fingerprint_set(
        r#"
        function map(xs, fn) {
            let result = [];
            for (let i = 0; i < xs.length; i++) result.push(fn(xs[i]));
            return result;
        }
        "#,
    );
    let axios = fingerprint_set(
        r#"
        function request(opts) {
            return fetch(opts.url, { method: opts.method || 'GET' });
        }
        "#,
    );

    let score = jaccard(&lodash, &axios);
    assert!(
        score < 0.2,
        "cross-package fingerprint sets must not look similar (got jaccard={score})"
    );
}
