# Package Signature Matching — Design

**Status:** approved 2026-05-16; not committed to repo (`docs/superpowers/` is gitignored).

## 1. Problem

Today's `reverts-package-matcher` matches a bundle module to a cached `PackageSource`
only when (a) the bundle module already carries `package_name`, and (b) the cached
source matches by either FNV hash of normalized text or by FNV hash of raw function
source slices plus string anchors ≥ 3 chars. This fails in two ways the project
needs to close:

1. **Cross-version drift** — identifier renames, parameter mangling, formatting drift,
   bundler helper wrapping, and TS/JSX lowering all defeat the current "function
   signature" (which is a hash of raw function-source bytes). Matching collapses to
   `string_anchors` overlap, which gives recall ≤ 30% on real cross-version pairs.
2. **Third-party identification** — modules without a `package_name` (application
   modules absorbing third-party helpers, or `kind=Package` modules with the name
   field missing) cannot be classified at all. The matcher only runs over already-
   labeled rows.

## 2. Goals

- **G1.** Drift-tolerant function fingerprints that survive identifier rename,
  parameter rename, TS→JS lowering, JSX→runtime calls, esbuild/webpack helper
  wrapping, and minification.
- **G2.** Function-level attribution. A bundle module that contains 8 lodash
  helpers + 2 application functions produces 8 lodash-attributed function rows and
  2 unattributed function rows, not a single best-effort module-level row.
- **G3.** Reverse-index lookup. The same fingerprints that score known-name matches
  can identify unknown functions against an open corpus reachable through an
  injectable trait. ADR 0003 forbids network/Node/npm in core tests; the trait is
  the seam.
- **G4.** Per-attribution confidence and explainability. Each attribution carries
  the match tier, the matched axes, the matched alternate pass, and a margin to
  the runner-up.

## 3. Non-goals (v1)

- ML embeddings (Asm2Vec / CodeBERT style) — out of scope; cost/benefit and the
  determinism story do not fit the pipeline. Reconsider in v2 as an optional axis.
- Persistent on-disk signature database. Signatures are values that flow through
  the pipeline; caching is a separate concern living outside core.
- Source-level recovery from match data. ADR 0002 forbids post-write repair; the
  matcher emits attributions only.
- Cross-language packages (Wasm, native addons).

## 4. Architecture

```
reverts-cli
   └─→ reverts-package-index ★new (non-core; async; reqwest allowed)
        ├─ LocalCacheIndex
        └─ RemoteIndex

reverts-package-matcher (core; sync; ADR 0003-compliant)
   └─→ reverts-analyze
        └─→ reverts-graph     — emits FunctionFingerprint
             ├─→ reverts-ir   — FunctionId / AxisHashes types
             ├─→ reverts-js   — NormalizationPass trait + passes
             └─→ reverts-input — PackageAttributionInput.function_span
```

Dependency direction strictly downward, matching
`docs/architecture/module-boundaries.md`. Network only in
`reverts-package-index`. The matcher never knows whether the index is local or
remote.

### 4.1 New crate: `reverts-package-index`

Non-core. Holds:

- `trait PackageFingerprintIndex` implementations.
- `LocalCacheIndex` — backed by the existing `PackageSource` cache shape,
  preloaded into in-memory inverted maps.
- `RemoteIndex` — async; talks to npm/unpkg or a future internal registry; may
  use `reqwest`. Wraps the async surface behind a blocking adapter so the
  synchronous matcher can call it from `reverts-cli`.

### 4.2 Crate change summary

| Crate | Change |
|---|---|
| `reverts-ir` | add `FunctionId`, `AxisHashes`, `FunctionFingerprint`, `NormalizationPassId`, `MatchTier` |
| `reverts-js` | add `trait NormalizationPass` and 6 implementations |
| `reverts-graph` | extract `FunctionFingerprint` per function (primary + alternates) alongside existing AST facts |
| `reverts-input` | add optional `function_span: Option<SourceSpan>` to `PackageAttributionInput` |
| `reverts-package-matcher` | replace today's `ExactPackageMatcher` body with cascade matcher; consume `dyn PackageFingerprintIndex` |
| `reverts-package-index` | new crate |
| `reverts-cli` | inject `RemoteIndex` |
| `reverts-analyze` | thread the fingerprint records into `EnrichedProgram` |

## 5. Data model

```rust
// reverts-ir

pub struct FunctionId {
    pub module_id: ModuleId,
    pub span: SourceSpan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AxisKind {
    Ast, Cfg, ReturnPattern, EffectPattern, LiteralAnchor, AccessPattern,
    StructuralAnchor, LiteralShape, AccessShape, CalleeSet, BindingPattern,
    ThrowSet,
}

pub struct AxisHashes {
    pub ast: u64,
    pub cfg: u64,
    pub return_pattern: u64,
    pub effect_pattern: u64,
    pub literal_anchor: Option<u64>,
    pub access_pattern: Option<u64>,
    pub structural_anchor: u64,         // always present
    pub literal_shape: Option<u64>,
    pub access_shape: Option<u64>,
    pub callee_set: Option<u64>,
    pub binding_pattern: u64,           // always present
    pub throw_set: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NormalizationPassId {
    Primary,
    TsRuntimeErased,
    JsxRuntimeNormalized,
    BundlerWrapperUnwrapped,
    HelperIdentityInlined,
    ExportBoundaryNormalized,
    ClosureBoundaryAligned,
}

pub struct FunctionFingerprint {
    pub id: FunctionId,
    pub param_count: usize,
    pub statement_count: usize,
    pub primary: AxisHashes,
    pub alternates: Vec<(NormalizationPassId, AxisHashes)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub enum MatchTier {
    Exact,             // primary AST hash
    ExactAlternate,    // alternate AST hash
    StructuralAnchored,// CFG + at least one anchor overlap
    FeatureSimilarity, // feature key + Jaccard threshold
    StructuralOnly,    // structural_anchor fallback
}
```

```rust
// reverts-package-matcher

pub struct ExactKey      { pub param_count: usize, pub statement_count: usize, pub ast_hash: u64 }
pub struct CfgKey        { pub param_count: usize, pub cfg_hash: u64 }
pub struct FeatureKey    { pub param_count: usize, pub kind: AxisKind, pub hash: u64 }
pub struct StructuralKey { pub param_count: usize, pub structural_anchor: u64 }

pub struct Candidate {
    pub package: PackageId,                       // (name, version)
    pub variant_path: String,
    pub external_function_id: u64,
    pub matched_axis: AxisKind,
    pub matched_alternate: Option<NormalizationPassId>,
}

pub struct CorpusStats {
    pub axis_hash_frequencies: BTreeMap<(AxisKind, u64), u32>,
}

pub trait PackageFingerprintIndex: Send + Sync {
    fn query_exact(&self, key: ExactKey)             -> Vec<Candidate>;
    fn query_cfg(&self, key: CfgKey)                 -> Vec<Candidate>;
    fn query_feature(&self, key: FeatureKey)         -> Vec<Candidate>;
    fn query_structural(&self, key: StructuralKey)   -> Vec<Candidate>;
    fn corpus_stats(&self)                           -> &CorpusStats;
}
```

`PackageAttributionInput` gains:

```rust
pub function_span: Option<SourceSpan>,    // None = module-level (backward-compatible)
pub confidence: Option<AttributionConfidence>,
```

```rust
pub struct AttributionConfidence {
    pub tier: MatchTier,
    pub matched_axes: SmallVec<[AxisKind; 4]>,
    pub matched_alternate: Option<NormalizationPassId>,
    pub top_score: f64,
    pub runner_up_score: f64,
    pub margin: f64,                  // (top - runner_up) / top, clamped to [0,1]
}
```

## 6. The twelve axes

Each is FNV-1a hash mix that incorporates the pass id and pass version, so a
pass-version bump invalidates downstream caches automatically.

| Axis | What it captures | How it is computed | Failure modes it absorbs |
|---|---|---|---|
| `ast` | normalized AST shape | recursive Merkle hash of `NormalizedNode` | identifier rename |
| `cfg` | branching topology | hash of `model.graph().control_flow()` ordered traversal restricted to the function's span | literal-value drift, identifier rename |
| `return_pattern` | what is returned | new visitor: bucket each return into {void, literal, identifier, member-chain, call, ternary, await, throw-thru} | local name drift |
| `effect_pattern` | side-effect bag | def-use `Call`, `MemberWrite`, await/yield/throw counts | local name drift |
| `literal_anchor` | stable strings/regexes/bigints | new visitor: cooked string ≥ 3 chars, regex source+flags, BigInt literal value | identifier rename, minification |
| `access_pattern` | member access with names | def-use `MemberRead`/`MemberWrite` property names | receiver rename |
| `structural_anchor` | counts-only digest | param-destructure depth, await/yield/throw counts, try-handler count, loop-kind multiset, return arity | identifier + property + literal drift; ~54% of TS source has no other signal |
| `literal_shape` | content-resilient literal digest | length-bucket of strings, regex flag+group count, numeric class (int / float / power-of-2) | constant folding, string consolidation |
| `access_shape` | minify-resilient access digest | member-chain depth, computed-vs-static flag, call arity — no property names | property name rewriting in aggressive minifiers |
| `callee_set` | bag of callees | def-use `Call` targets: `c:NAME`, `cm:.PROP`, `nc:NAME` | receiver rename |
| `binding_pattern` | declarator topology | param/local shapes: `i`, `i+d`, `o[N]`, `a[N]`, `r` | identifier rename |
| `throw_set` | thrown error classes | `throw new NAME(...)` constructor names + `t:expr`/`t:lit` markers | local name drift |

Approximately five axes (`cfg`, `effect_pattern`, `access_pattern`, `callee_set`,
`binding_pattern`'s param part) are derivable from existing graph state and
require no new visitor.

## 7. Normalization passes

```rust
// reverts-js

pub trait NormalizationPass {
    fn id(&self) -> NormalizationPassId;
    fn version(&self) -> u32;
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>);
}
```

v1 passes (each function emits primary + one alternate per applicable pass):

1. **`TsRuntimeErased`** — drop TS-only nodes (type ann, enum lowered to object,
   namespace lowered to IIFE, abstract/private modifiers stripped).
2. **`JsxRuntimeNormalized`** — JSX → `_jsx(elementType, props, ...children)`.
3. **`BundlerWrapperUnwrapped`** — peel `__toESM(x)`, `__toCommonJS(x)`,
   `__defProp(target, key, desc)` wrappers. Helper name list lives in
   `reverts-js::bundler_helpers`.
4. **`HelperIdentityInlined`** — inline calls to identity-shaped helpers within
   the same module (e.g. `_interopRequireDefault(x)` → `x`).
5. **`ExportBoundaryNormalized`** — strip `export` keyword wrapping function
   declarations so `export function f(){}` and `function f(){}` collide.
6. **`ClosureBoundaryAligned`** — when a function's entire body is `return (() =>
   { ... })()` or returns a single nested function, unwrap once so nested and
   top-level definitions of the same shape align.

A pass returns the same `Program` mutated; it must be idempotent (calling twice
produces the same result). The pass version is baked into every alternate hash
mix; bumping a pass version invalidates that alternate but not the primary.

## 8. Match cascade

Each bundle function is queried through the index in this order, stopping at the
first tier that yields a unique candidate (after frequency-penalized scoring):

| Tier | Key | Acceptance |
|---|---|---|
| Exact              | `ExactKey` against primary `ast` | unique candidate ⇒ accept |
| ExactAlternate     | `ExactKey` against alternates' `ast` | unique candidate ⇒ accept; record matched pass |
| StructuralAnchored | `CfgKey` and ≥ 1 anchor overlap (`literal_anchor`, `callee_set`, `throw_set`) | unique among CFG candidates with anchor overlap ≥ τ_anchor=1 |
| FeatureSimilarity  | `FeatureKey` on the most-distinctive axis present (`callee_set` > `throw_set` > `literal_anchor` > `access_pattern`) | Jaccard over remaining axes ≥ τ_feature=0.6 AND unique best |
| StructuralOnly     | `StructuralKey` | unique after frequency penalty AND margin ≥ 0.3 |

**Frequency penalty.** For each candidate, the matched hash's effective weight is
`w_tier / log2(2 + freq)` where `freq` comes from `corpus_stats()`. A `cfg_hash`
that appears in 10,000 functions contributes negligibly; one that appears in 3
contributes nearly full weight. This is the LibRadar lesson made concrete.

**Tier weights** for scoring (variant/version selection): Exact=10000,
ExactAlternate=5000, StructuralAnchored=1000, FeatureSimilarity=100,
StructuralOnly=10.

## 9. Variant and version selection

### 9.1 Variant within a (package, version)

For the multiple cached `PackageSource` entries that share a `(package, version)`
pair (the user confirmed cache holds multiple variants):

```
score(variant) = Σ tier_weight(match.tier) over functions matched in variant
              + α · jaccard(bundle_module.fn_set, variant.fn_set)
α = 100  // calibrated to match Σ tier_weights for ~1-2 ExactAlternate hits
pick = argmax(score)
tie  → prefer (browser > module > main > umd > main-cjs)
```

### 9.2 Version across versions for a package

```
score(version) = best_variant_score(version) × (matched_fn_count / module_fn_count)
```

- Unique best ⇒ `Selected`.
- Tied best within ε=0.05 ⇒ `Ambiguous` audit finding via existing
  `FindingCode::AmbiguousPackageMatch`.
- All zero ⇒ `NoMatch`.
- Best score below τ_v=300 ⇒ `InsufficientEvidence` (reuses existing variant).

### 9.3 Global bundle-wide assignment (Hungarian)

After per-function tier matching across all packages produces a candidate set per
bundle function, build a bipartite graph and solve maximum-weight assignment.

- Left: bundle functions in this run.
- Right: union of all candidate external function ids returned.
- Edge weight: `tier_weight × (1 - 0.1 × rank_within_function)` (rank 0 = top
  candidate from the cascade, rank 1 = runner-up, etc., capped at 4).
- Solve via in-tree Kuhn–Munkres (~120 lines, no new dependency).

The assignment respects one-to-one and gives globally optimal attribution. For
performance: typical bundle has 5,000-50,000 functions; Kuhn-Munkres is O(n³)
which is ~10⁸–10¹¹ ops worst case. We bucket by `param_count` first (no edge
crosses param buckets) which reduces n significantly per solve, then chunk
overlapping packages — typical solve is under 5s end-to-end.

## 10. Acceptance and confidence

| Condition | Output |
|---|---|
| Tier ∈ {Exact, ExactAlternate} and unique candidate | `Accepted` |
| Lower tier, margin ≥ 0.3 | `Accepted` |
| Lower tier, margin ∈ [0.1, 0.3) | `AcceptedWithCaveat` — runner-up recorded |
| Lower tier, margin < 0.1 | `Ambiguous` audit finding; no attribution |
| No tier matched | `NoMatch` — no row emitted; no finding |

`Ambiguous` reuses `FindingCode::AmbiguousPackageMatch`. `AcceptedWithCaveat`
emits the attribution row with `confidence.runner_up_score` set; the planner can
optionally widen later.

## 11. Audit invariants (extends existing)

- Every accepted function-level attribution has a `function_span` that is a
  valid sub-span of the parent `module_id`'s source.
- No two attribution rows share the same `(module_id, function_span)`.
- For an accepted `function_span`, no overlapping `function_span` accepted under
  a different `package_name`.
- Existing module-level invariants (parse, bare-import surface) unchanged.

New finding codes:

- `OverlappingFunctionAttribution` — two rows with overlapping spans.
- `LowConfidenceAttribution` — for `AcceptedWithCaveat`, surfaced as warning.

## 12. Validation strategy

Layered. L1, L4, L7 run on every PR; L2/L5/L8 nightly; L3 weekly; L6 on demand.

### L1 — Per-axis unit tests (PR gate)
Each axis has 6-10 paired fixtures: should-match and should-not-match. Examples:
- `ast` collides for `function f(a, b) { return a + b; }` and
  `function g(x, y) { return x + y; }`.
- `cfg` collides for `if (a) return b; return c;` and
  `return a ? b : c;` (after sugar pass).
- `throw_set` distinguishes `TypeError` vs `RangeError`.

Self-contained per ADR 0003: no network, no Node, no npm.

### L2 — Cross-version regression corpus (nightly)
Curated fixtures: 8 packages × 3-5 versions each, plus minified variants:
- `lodash` 4.0.0, 4.10.0, 4.17.21
- `axios` 0.27.2, 1.0.0, 1.6.5
- `react` 16.14, 17.0, 18.2
- `vue` 2.7.16, 3.0.0, 3.4.0
- `undici` 5.0.0, 5.28.0, 6.0.0
- `rxjs` 6.6.7, 7.5.0, 7.8.0
- `immer` 9.0.0, 10.0.0
- `zod` 3.20, 3.22, 3.23

Assertions per pair `(v_a, v_b)`:
- minor pair: function-fingerprint-set Jaccard ≥ 0.7
- major pair: ≥ 0.3
- per-axis survival rate published as a table; regression bumps fail the suite

This corpus is the empirical foundation. Pass-version bumps must re-run it.

### L3 — Bundle ↔ source roundtrip (weekly)
5-10 small real webpack/esbuild bundles built from known package versions:
- ≥ 95% of package modules correctly attributed at package level
- ≥ 90% of functions within attributed modules attributed at function level
- 0 false package attributions (mis-attributed to a different package)

### L4 — False-positive adversarial corpus (PR gate)
50 paired adversarial cases: same param count + same statement count + same
`cfg_hash` but different semantics (e.g. property-name-only differences). All
should be rejected by anchor disagreement.
- FP rate < 0.5%.

### L5 — Minified mirror of L2 (nightly)
Same packages, minified production builds.
- 10-20 pp recall regression vs L2 is the expected target; deeper regressions fail.

### L6 — Performance budget (on demand)
- Fingerprint generation: < 10ms/function (avg on L2 corpus)
- Single index lookup: < 1ms (in-memory index)
- End-to-end match for 500-module bundle: < 30s

### L7 — Global assignment correctness (PR gate)
Synthetic chunks built by merging the same-named helper from two different
packages (`lodash._arrayMap` + a hand-authored `_arrayMap`).
- Hungarian assignment splits 5/5 (vs. naive greedy giving 10/0).

### L8 — Differential vs legacy (nightly)
Same input bundles run through legacy `reverts` and new matcher.
- `new.accepted ⊇ legacy.accepted` (recall does not regress)
- Newly accepted with margin ≥ 0.3 surfaced for human spot-check

## 13. Migration and rollout

- Phase 0: ship data types + fingerprint extraction in `reverts-graph` behind a
  feature flag (`fingerprints`). No behavior change.
- Phase 1: enable in-memory `LocalCacheIndex`; new matcher runs in parallel with
  existing one; diff reports emitted to `AuditReport` for inspection but no
  attribution rows changed.
- Phase 2: switch attribution to new matcher's output when its confidence
  thresholds clear; legacy path kept for fallback under a config knob.
- Phase 3: remove old matcher body.

## 14. Open questions

- None blocking. Hungarian implementation choice (in-tree Kuhn-Munkres) is
  confirmed; corpus package list is confirmed.

## 15. References

- ATVHunter (ICSE 2021) — coarse-grained CFG + fine-grained opcode two-tier.
  Direct inspiration for the cascade.
- LibScout (CCS 2016) — per-class Merkle hash folded toward package root.
  Inspiration for layered hashing.
- LibRadar (ICSE-C 2016) — corpus frequency penalty for popular but
  uninformative signatures. Section 8 frequency penalty.
- legacy `reverts` repo (`src/engine/function_matcher`,
  `src/engine/equivalence/normalizer.rs`) — concrete reference for the 12-axis
  shape and pass list.
- Schleimer et al. (Winnowing, SIGMOD 2003) — referenced for future token-axis
  work; not in v1.
