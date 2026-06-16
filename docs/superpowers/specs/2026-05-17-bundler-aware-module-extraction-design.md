# Bundler-Aware Module Extraction — Design

**Status:** approved 2026-05-17; not committed to repo (`docs/superpowers/` is gitignored).

**Implements:** ADR 0004 — bundler-aware module extraction as a dedicated stage.

## 1. Problem

A production JavaScript bundle is a single source file whose interior structure
is determined by the bundler that produced it. The cascade matcher and the
function-fingerprint pipeline require per-module byte spans whose bodies are
**parseable program units** — not mid-expression fragments. The verification
captured in earlier turns showed:

- Calling `FunctionExtractor::fingerprint(...)` on a 16 MB esbuild CLI bundle
  yields 9,781 functions, but **zero** of them fall inside any of the 30
  legacy-ground-truthed module spans extracted from the same DB. Legacy stores
  outer wrapper spans; our extractor records inner function spans; they do not
  align.
- Slicing the bundle by the legacy span and re-parsing the slice produces 0
  functions because the slice is a mid-expression fragment.

Without bundler-aware extraction the matcher cannot attribute functions
back to source modules, the L4/L7 adversarial fixtures cannot exercise the
cascade end-to-end on a real bundle, and the `cascade_attributions` count
returned by `match-packages` stays at 0 for every bundled input.

## 2. Goals

- **G1.** Convert a raw bundle source into a `Vec<InnerModule>` where each
  inner module's `body_span` slices a parseable program unit.
- **G2.** Reconcile the extracted inner modules with existing
  `ModuleInput.source_span` values from upstream DB loaders so legacy
  metadata (module name, package attribution) survives.
- **G3.** Cover the bundler families that account for ≥95 % of real
  production bundles measured by WoBaC 2023: esbuild, webpack 4/5, Rollup
  CJS/ESM, UMD, Browserify, AMD.
- **G4.** Support monolithic vendored bundles (cm6, jQuery UMD, lodash UMD)
  via call-graph SCC + string-anchor partitioning — the path that
  open-source de-bundlers omit and that the existing
  `~/.reverts/.reverts.db` corpus contains.

## 3. Non-goals (v1)

- **NG1.** Sourcemap-based extraction. Production bundles routinely strip
  `//# sourceMappingURL=`; verification must succeed without that fallback.
- **NG2.** Flow-based recovery (JSAnalyzer style — tracking
  `__webpack_exports__.X = …` writes back from `__webpack_require__("X")`
  callsites). Defer to v2; templates are easier to debug.
- **NG3.** Emit-side concerns: `public_surface` extraction
  (`globalThis.X = {…}` shim generation) and `submodule_ingester` (re-export
  attribution). Both belong to a later code-emission stage.
- **NG4.** Bundle-level fingerprint registry lookup
  (`engine::package_matcher::vendored_bundle_registry`). Wire bundle
  fingerprints into the existing function-fingerprint cascade instead.

## 4. Architecture

```
reverts-cli
  └─→ reverts-bundle (NEW)
       ├── classifier      — Plain | Marked | Iife trichotomy
       ├── structure_detector — per-bundler module extractors
       ├── cluster         — IIFE call-graph SCC + string-anchor partition
       └── merge           — reconcile with ModuleInput.source_span
  └─→ reverts-graph         — consumes resolved spans
  └─→ reverts-analyze       — depended on by reverts-bundle for CompilerKind
```

`reverts-bundle` depends on `reverts-js`, `reverts-analyze`, `reverts-graph`,
`reverts-input`, `reverts-ir`. Nothing in the workspace depends on
`reverts-bundle` except `reverts-cli`, which calls it before invoking the
cascade matcher.

### 4.1 Three-way classification

```rust
pub enum BundleClassification {
    /// No bundler pattern recognised. Source flows through as a single
    /// module with no inner subdivision.
    Plain,
    /// Bundler pattern recognised; inner modules are recoverable via
    /// template detection. Carries the discovered InnerModule list.
    Marked(MarkedMetadata),
    /// Monolithic IIFE-style vendored bundle (cm6, jQuery UMD, lodash UMD).
    /// Inner structure is recoverable only via call-graph clustering.
    Iife(IifeMetadata),
}
```

Classification is gating — downstream behaviour branches on the variant. A
file that could not be matched against any template returns `Plain`
(deliberately, not as a silent fallback: the classifier records the reason
in the `MarkedMetadata` audit so future cases can be diagnosed).

### 4.2 Per-bundler templates

One file per bundler in `crates/reverts-bundle/src/detectors/`:

| File | Pattern recognised |
|---|---|
| `esbuild.rs` | `__commonJS({"path": (exports, module) => {…}})` map; `__esm({"path": () => {…}})` map; `var X = __toCommonJS(…)` re-exports |
| `webpack5.rs` | `var __webpack_modules__ = {"./path": (m, e, r) => {…}, …};` |
| `webpack4.rs` | `(function (modules) {…})([fn, fn, …]);` and `webpackJsonp([id], {…})` chunks |
| `rollup_cjs.rs` | `(function (global, factory) {…})(this, (function () {…}))` UMD-CJS hybrid; sequential IIFE-init chain |
| `rollup_esm.rs` | ES module re-exports + IIFE-init helpers |
| `umd.rs` | `(function (root, factory) { if (typeof define === 'function' && define.amd) … })(this, function () {…})` |
| `browserify.rs` | `(function () { function r(e,n,t) {…} })({…})` plus the bundler-fingerprint hash |
| `amd.rs` | `define("name", ["dep"], function () {…})` registration |

Each detector exposes:

```rust
pub fn detect(program: &Program<'_>, source: &str) -> Vec<InnerModule>;
```

The detector returns an empty `Vec` when its template does not match. The
classifier sequences detectors by `CompilerKind` priority but tries every
detector in catch-all mode — multiple bundlers can leave traces in the same
file (e.g., a webpack chunk emitted via esbuild's runtime).

### 4.3 IIFE cluster recovery

When classification yields `Iife`, the bundle is monolithic and no template
identifies inner modules. `cluster.rs` partitions the IIFE body into inner
clusters by:

1. Building a call graph over named top-level declarations inside the IIFE
   body (functions, top-level `var X = function (…) {…}` declarations).
2. Computing strongly-connected components on that graph.
3. Partitioning each SCC further by **string-anchor cohesion**: two functions
   in the same SCC are split into different clusters when they share no
   string-literal anchor (a string literal ≥3 characters long).

Output: `Vec<InnerCluster>` where each cluster has a synthetic id
(`"iife:0"`, `"iife:1"`, …), a body byte range covering the cluster's
declarations, and the list of string anchors used to form it.

### 4.4 InnerModule data model

```rust
pub struct InnerModule {
    /// Stable identifier within the parent bundle. For esbuild
    /// __commonJS, the string key; for webpack, the module id;
    /// otherwise "<bundler>:<index>" synthesised.
    pub virtual_id: String,
    /// Byte range of the body inside the parent bundle's source —
    /// always a parseable program unit (Function/Arrow body, IIFE
    /// inner block, or registered factory body).
    pub body_span: ByteRange,
    /// Bundler shape this inner came from. Drives downstream
    /// interpretation of the body and helps with the cascade tier
    /// metadata.
    pub bundler: BundlerKind,
    /// Optional upstream source path when the bundler embedded it as
    /// the inner-module key (esbuild __commonJS), else None.
    pub source_path_hint: Option<String>,
    /// Parent module id (the file containing this inner).
    pub parent_module_id: ModuleId,
}
```

`BundlerKind` mirrors `reverts-analyze::CompilerKind` but adds `Umd`,
`Browserify`, and `Amd` variants that compiler-profile detection does not
distinguish. The two enums are deliberately separate: `CompilerKind`
classifies the producing toolchain; `BundlerKind` classifies the wrapper
shape encountered. A single bundle file can have both `CompilerKind::Webpack`
(detected from `__webpack_require__`) and `BundlerKind::Amd` (detected from a
`define()` inside).

### 4.5 Merge with existing ModuleInput.source_span

When `ModuleInput.source_span` is `Some(span)` from upstream (typically a DB
loader populating from the legacy reverts modules table), the extractor's
output is merged rather than overwritten:

- For each upstream `ModuleInput`, find the extractor `InnerModule` whose
  `body_span` overlaps the upstream span. Replace the span with the
  extractor's parseable body span. Keep the upstream `original_name`,
  `package_name`, `package_version`, and `source_file_id`. When more than
  one extractor `InnerModule` overlaps the upstream span, pick the inner
  whose body covers the largest share of the upstream range (`overlap /
  upstream_width`); ties on share resolve by smaller `byte_start`. The
  runner-up inners on the same upstream span are emitted as new modules
  (third bullet below) with a `BundleDetectorAmbiguous` audit finding.
- For each upstream `ModuleInput` with no overlapping `InnerModule`, mark
  the row's `source_span` as **unparseable** (concretely: leave the
  upstream span on the row but emit a `MissingParseableBody` audit
  finding so the matcher skips it without silent error).
- For each `InnerModule` with no matching upstream `ModuleInput`, emit a
  new `ModuleInput` with synthetic `ModuleId`, `kind = ModuleKind::Package`
  when the bundler reports a `source_path_hint` matching a known
  `node_modules/…` shape, else `ModuleKind::Application`. Name derived
  from `virtual_id`.

The merge runs after extraction and before `reverts-graph::build_graph`. It
is the only operation that produces or modifies `ModuleInput.source_span`.

## 5. Reuse mapping

### 5.1 Direct reuse — no change required

| Source | Reused as |
|---|---|
| `reverts-js::{Parser, source_type_candidates, parse_options_for}` | OXC parser setup |
| `reverts-js::{JsError, ParseError, parse_error_message}` | Classifier parse-failure model |
| `reverts-analyze::CompilerKind`, `detect_compiler_profile`, `looks_minified` | Classifier bundler-kind detection |
| `reverts-analyze::WEBPACK/ESBUILD/ROLLUP_RUNTIME_IDENTIFIERS` | Detector pattern constants |
| `reverts-ir::{ByteRange, FunctionId, ModuleId, hash::*}` | Span types, fingerprint helpers |
| `reverts-input::{ModuleInput, ModuleKind, SourceSpan}` | Merge target |

### 5.2 Refactor before use — promote to a single source

These must move **before** `reverts-bundle` consumes them, in their own
single-purpose commits:

| Current location | Target |
|---|---|
| `reverts-js::normalize::bundler_wrapper_unwrapped::ESBUILD_WRAPPER_NAMES` (private) | `reverts-analyze::esbuild_runtime` (with the existing `ESBUILD_RUNTIME_IDENTIFIERS`); both `reverts-js` and `reverts-bundle` import from there |
| `reverts-graph::iife_kind` (private fn) | Make `pub`. `reverts-bundle` imports it. |
| `reverts-graph::AstWrapperKind` (already `pub`) | Used directly; no change |

These refactors are strictly mechanical and add no new behaviour. They land
before any new bundler code so two-track implementations never exist.

### 5.3 Deliberate non-reuse

| Looks reusable | Why not reused |
|---|---|
| `reverts-js::normalize::BundlerWrapperUnwrapped` pass | Strips wrappers for hash collision; detector needs to recognise wrappers for slicing. Same NAMES list, different operation. Share the constant, not the pass. |
| `reverts-graph::AstFact::WrapperRegion` graph facts | Would require chicken-and-egg: graph needs spans, extractor produces spans. Two-pass architecture more complex than letting extractor parse independently. |
| Legacy `submodule_ingester` and `public_surface` | Emit-side; not consumed by the matcher. Out of scope per NG3. |

## 6. Pipeline integration

The CLI driver runs the extractor as a new step between input loading and
graph construction:

```rust
fn run_decompile(connection: &Connection, args: &DecompileArgs) -> Result<…> {
    let mut input = load_project_rows_from_connection(connection, args.project_id)?;

    // NEW: bundler-aware module extraction for source files that look like
    // bundles. Detect classification per source_file, run extractors, merge
    // into input.modules.
    let extraction = reverts_bundle::extract(&input)?;
    extraction.merge_into(&mut input);

    let bundle = InputBundle::from_rows(input)?;
    let graph = reverts_graph::build_graph(&bundle)?;
    let enriched = reverts_analyze::enrich_program(ProgramModel::from_input(bundle));

    // …matcher continues as today, but now sees real per-module spans
}
```

`extract` is the single public entry point:

```rust
pub fn extract(input: &InputRows) -> Result<BundleExtraction, BundleExtractError>;

pub struct BundleExtraction {
    pub classifications: BTreeMap<u32, BundleClassification>,
    pub new_modules: Vec<ModuleInput>,
    pub audit: AuditReport,
}

impl BundleExtraction {
    pub fn merge_into(self, input: &mut InputRows) { … }
}
```

No other crate calls into `reverts-bundle` directly. `reverts-graph`,
`reverts-analyze`, and `reverts-package-matcher` see a normal `InputBundle`
and have no awareness that some module spans came from the extractor.

## 7. Validation strategy

Six layers, mirroring the existing pipeline's validation conventions
(ADR 0003 — self-contained tests):

### L1 — per-detector unit tests (PR gate, self-contained)

Each detector under `crates/reverts-bundle/src/detectors/` has its own
test module with hand-authored synthetic bundle fragments demonstrating:
- Positive: a canonical bundler pattern is recognised, body span is
  exactly the expected byte range.
- Negative: a near-miss pattern (e.g., `__commonJS` with wrong arity) is
  rejected.
- Boundary: an empty registration map and a single-module map both work.

No network, no Node, no real bundle assets. Each detector ships 8–15
fixture pairs.

### L2 — classifier-level tests (PR gate, self-contained)

`classifier.rs` tests assert that representative inputs land in the right
`BundleClassification` variant:
- A handful of bytes of plain ES code → `Plain`.
- An esbuild fragment with one `__commonJS({…})` map → `Marked` with one
  inner.
- A monolithic IIFE `var Cm6 = (function () { … })()` → `Iife`.
- A degenerate "looks vendored but isn't" file → `Plain` with audit
  finding explaining why.

### L3 — merge correctness (PR gate, self-contained)

Verify the merge rules from §4.5 with `InputRows` fixtures:
- Upstream `ModuleInput` + matching extractor `InnerModule` → upstream
  metadata preserved, span replaced.
- Upstream `ModuleInput` + no matching `InnerModule` → audit
  `MissingParseableBody`.
- Extractor `InnerModule` + no matching upstream → new synthetic
  `ModuleInput` with derived name.
- Overlap conflicts (two `InnerModule` spans both inside one upstream
  span) → preferred match is the wrapper whose body covers the larger
  share of the upstream span; the runner-up is dropped with an audit
  finding.

### L4 — end-to-end against legacy DB (nightly, opt-in)

Read the existing `~/.reverts/.reverts.db` for projects where legacy
already attributed bundle modules to source counterparts. For each pair:
- Run `reverts_bundle::extract` on the bundle file.
- Compare against legacy's `modules` table for the same `project_id`.
- Assert: extracted-vs-legacy module count agreement ≥80 %, span overlap
  median ≥90 % of byte coverage.

This is a regression lock that ensures we are not silently regressing
against legacy's known-good output. The test is gated behind a feature
flag because it requires the local DB.

### L5 — cascade integration (PR gate, self-contained)

A synthetic two-module esbuild bundle wrapping two known function
shapes against synthetic package sources containing the same shapes:
- Run extract → merge → graph → cascade matcher.
- Assert: cascade emits 2 attribution rows, both at the `Exact` tier,
  function spans inside the extractor-produced bodies.

This is the integration test that the entire pipeline still works
end-to-end after extraction lands.

### L6 — adversarial false-positive (PR gate, self-contained)

20 synthetic "bundle-shaped but not actually" fixtures:
- An object literal that looks like `__webpack_modules__` but isn't.
- A function call that resembles `__commonJS` but with wrong shape.
- A `define()` call that is not an AMD module.

Each must classify as `Plain`. The detector for each bundler emits zero
inner modules. FP rate must be 0/20.

## 8. Phased rollout

The implementation lands in three phases. Each phase is independently
shippable; downstream stages tolerate `BundleClassification::Plain` for any
bundler not yet supported.

### Phase α — ≈75 % bundle coverage (per WoBaC 2023)

- Workspace plumbing: `reverts-bundle` crate, dependencies, empty modules.
- Refactor commits: `ESBUILD_WRAPPER_NAMES` lifted, `iife_kind` made `pub`.
- Classifier with path heuristics + `CompilerKind` dispatch.
- Detectors: `esbuild`, `webpack5`, `rollup_cjs`.
- Merge implementation.
- CLI integration: `reverts_bundle::extract` runs before graph build.
- L1, L2, L3, L5, L6 test layers.
- End-to-end verification on the existing CC bundle:
  `function_match_count` for the bundle ≥ the legacy DB's `module_matches`
  count for the same project, with 0 audit errors.

### Phase β — +10 % coverage

- Detectors: `webpack4`, `rollup_esm`.
- Additional L1 / L6 fixtures for these patterns.
- L4 nightly comparison enabled (requires DB).

### Phase γ — +10 % coverage; legacy parity

- Detectors: `umd`, `browserify`, `amd`.
- IIFE cluster recovery (`cluster.rs`) — call-graph SCC + string-anchor
  partition. Test against monolithic vendored bundles (cm6, jQuery UMD).
- Final L4 alignment: ≥80 % module-count agreement with legacy across the
  whole `.reverts.db` corpus.

After Phase γ, the legacy `engine::decompile::bundle::vendored_bundle_registry`
is consulted by the cascade matcher's bundle-level fingerprint axis (out of
scope for v1; spec'd separately).

## 9. Error handling and audits

New audit codes in `reverts-observe::FindingCode`:

| Code | Severity | Meaning |
|---|---|---|
| `BundlerKindUnrecognised` | Info | Path matches "vendored" heuristics but no detector matched. `Plain` classification chosen. |
| `BundleDetectorAmbiguous` | Warning | Two or more detectors matched the same byte range with disjoint inner-module results. The detector with higher confidence wins, runner-up dropped. |
| `MissingParseableBody` | Error | Upstream `ModuleInput.source_span` does not overlap any extractor body. Module is unparseable; matcher will skip. |
| `IifeClusterDegenerate` | Info | Iife classification ran cluster recovery and produced only one cluster (no internal structure detected). |

Parse errors during extraction emit the existing
`AstFactExtractionFailed`/`UnparseablePackageSource` codes from
`reverts-observe`; no new code is needed for that case.

## 10. Open questions

- **CompilerProfile minor-bundler detection.** `reverts-analyze` does not
  currently distinguish UMD from Rollup or Browserify. Phase γ either
  extends `CompilerKind` with new variants or has detectors run
  unconditionally. Defer until Phase γ planning.
- **Module-id namespacing for newly discovered modules.** Synthetic
  `ModuleId`s must not collide with upstream-loaded ids. Reserve a
  high-bit range (e.g. `0x8000_0000..`) for extractor-discovered modules;
  spec the exact scheme during Phase α.
- **Performance budget on 16 MB bundles.** Current full-bundle parse takes
  ~28 s on the verification corpus. Extraction must parse only once and
  walk the AST a single time. Confirm via L5 latency assertion.

## 11. References

- ADR 0004 — Bundler-aware module extraction as a dedicated stage
  (the architecture principles this spec implements).
- Legacy `~/Codes/reverts/reverts/src/engine/decompile/bundle/`
  (`classifier.rs`, `structure_detector.rs`, `cluster.rs`,
  `inner_module.rs` — referenced for algorithm parity).
- Open-source: webcrack, wakaru, Pollux (NDSS 2022), JSAnalyzer
  (USENIX 2020), WoBaC (IEEE Access 2023).
