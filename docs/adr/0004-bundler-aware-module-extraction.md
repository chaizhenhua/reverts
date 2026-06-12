# ADR 0004: Bundler-Aware Module Extraction as a Dedicated Stage

## Status

Accepted

## Context

A production JavaScript bundle is a single source file whose interior structure
is determined by the originating bundler (esbuild, webpack, Rollup, UMD,
Browserify, AMD). Function-level fingerprinting and the cascade matcher both
require the bundle to be split into per-module byte spans whose bodies are
parseable program units; without that, the parser receives mid-expression
fragments and extracts zero functions.

The information needed to recover those spans is bundler-specific. esbuild
emits `__commonJS({"path": fn})` registration maps; webpack emits
`__webpack_modules__` object tables; Rollup wraps modules in IIFE-init chains;
vendored single-file libraries (cm6, jQuery UMD) ship as monolithic IIFE blobs
that no template matches. The legacy `reverts` codebase encodes these in
`engine/decompile/bundle/` across ~3,200 lines. Open-source tools such as
webcrack and wakaru handle a subset with per-bundler templates; academic
systems such as Pollux and JSAnalyzer cover one bundler with high precision.

The reverts-next pipeline currently has no equivalent. Bundle source files
flow through the cascade matcher as a single module, which collapses
function-level attribution and breaks bundle-vs-source verification.

## Decision

Bundler-aware module extraction is a first-class pipeline stage. It lives in a
dedicated `reverts-bundle` crate, sits between `reverts-input` / `reverts-js`
and `reverts-graph`, and produces structured `InnerModule` records consumed by
the rest of the pipeline.

The crate is organised around three architectural commitments:

1. **Classification gates behaviour.** Every bundle source is classified as
   one of `Plain` (no extraction), `Marked` (per-bundler template
   extraction), or `Iife` (monolithic vendored bundle requiring cluster
   recovery). Downstream stages branch on this classification; there is no
   silent fallback that treats an unsupported shape as `Plain`.

2. **Per-bundler templates, dispatched by `CompilerKind`.** Each bundler shape
   has a dedicated detector function fed by `reverts-analyze`'s existing
   compiler-profile detection. Generic heuristics ("anything function-shaped
   is a module") are rejected: they produce false positives that pollute the
   match cascade.

3. **Single-source shared infrastructure.** Parser plumbing, compiler-kind
   detection, runtime-identifier constants, IIFE-shape recognition, and span
   types are reused from existing crates. No bundler-pattern constant lives in
   two crates; no AST shape predicate has two implementations. When a piece of
   logic is needed by both a normalization pass and the extractor, it is
   lifted to `reverts-ir` or `reverts-analyze` before either consumer reads
   it.

## Consequences

- A `reverts-bundle` crate joins the workspace, depending on `reverts-js`,
  `reverts-analyze`, `reverts-graph`, `reverts-ir`, and `reverts-input`.
  Nothing depends on it except the CLI driver; the cascade matcher remains
  bundle-agnostic.
- Implementation work proceeds per-bundler in phases so each phase produces a
  shippable verification: esbuild + webpack 5 + Rollup CJS first (≈75 % of
  real bundles per WoBaC 2023 measurement), then webpack 4 + Rollup ESM, then
  UMD + Browserify + AMD.
- IIFE / vendored-bundle support uses a call-graph SCC over inner bodies plus
  string-anchor partitioning, ported from the legacy `cluster.rs` algorithm.
  Open-source de-bundlers omit this path; vendored libraries (cm6, jQuery UMD,
  lodash UMD) are intentionally supported because they appear in the existing
  `~/.reverts/.reverts.db` corpus.
- Sourcemap-based extraction is deferred to a later iteration. Production
  bundles routinely strip sourcemaps, and verification of the AST-based
  normalisation pipeline must succeed without that fallback before sourcemap
  fast-paths are added.
- Existing `ModuleInput.source_span` values from upstream (DB) loaders are
  reconciled with extractor output by an explicit merge rule: extractor output
  defines the parseable body span, while pre-existing metadata (module name,
  package attribution) is preserved. Spans that the extractor cannot recover
  remain marked as unparseable rather than being treated as if they had been
  parsed.
- Flow-based extraction (tracking `__webpack_exports__` writes back from
  `__webpack_require__` call sites) is a v2 alternative explicitly out of
  scope. Templates are easier to debug and adequate for the v1 corpus.
- The legacy emit-side concerns (`public_surface`, `submodule_ingester`) are
  not ported; they belong to a later code-emission stage and are not needed
  by the matcher.
