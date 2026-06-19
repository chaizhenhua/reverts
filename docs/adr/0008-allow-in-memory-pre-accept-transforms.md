# ADR 0008: Allow Audited In-Memory Pre-Accept Transforms

## Status

Accepted — 2026-05-27

## Context

[ADR 0001](0001-use-ast-first-output-pipeline.md) requires AST-first emission and
[ADR 0002](0002-reject-post-write-repair.md) rejects post-write repair. In
practice two categories of work do not fit cleanly inside the per-module emitter
yet still must run before output is accepted:

- Whole-project normalizations that need every emitted file to exist first, such
  as canonicalizing `import.meta.url` source locations, rewriting asset
  references to their final relative paths, and folding static template
  literals.
- A small set of edits where AST round-tripping is not yet practical and a
  scoped byte/text edit is the pragmatic option.

Without an explicit decision, these would drift back into exactly the ad hoc
"final sweep" passes ADR 0002 forbids, and the boundary between "legitimate
transform" and "banned repair" would be a matter of taste.

## Decision

Two narrow mechanisms are permitted and are *not* considered post-write repair:

1. **Pre-accept transforms** (`reverts-pipeline::pre_accept`). These are ordered,
   named, in-memory transforms that run on the `EmittedProject` after emission
   and before the acceptance audit. They produce a `PreAcceptProject`, never
   touch the filesystem, and record their names and changed-file counts in a
   `PreAcceptTransformReport`. The current order is
   `canonicalize_source_locations` → `rewrite_asset_references` →
   `fold_static_template_literals`.

2. **Centralized source surgery** (`reverts-planner::source_surgery` and
   `reverts-js` helpers). All remaining byte/text edits go through one shared
   edit applier with parse-aware and delimiter-boundary-aware helpers, not ad
   hoc string replacement.

The distinction from banned post-write repair is strict:

- The edits run **before** acceptance, on in-memory data, so the audit still
  sees the final bytes and can reject them.
- They are **ordered and named**, and report what they changed.
- They do **not** invent missing declarations, patch imports, or mask an
  upstream emitter defect. A correctness bug still gets fixed at its
  input/graph/constraint/planner/emitter source per ADR 0002.

Every pass that still scans source bytes must document why it cannot use
AST-first rewriting and should use `source_surgery`/`byte_lexer` helpers.

## Consequences

- Project-wide normalization has a sanctioned home that the acceptance audit
  still governs; only `AcceptedProject` reaches the writer.
- The remaining byte-level edits are auditable in one place instead of scattered
  string manipulation, and each is registered as debt with a migration note (see
  [module-boundaries.md](../architecture/module-boundaries.md), "Source Surgery").
- New post-emission behavior must be added as a named pre-accept transform or a
  documented `source_surgery` seam, not as an unscoped rewrite.
- This ADR is the explicit exception surface for ADR 0002. Anything outside
  these two mechanisms remains forbidden.

## References

- [ADR 0001](0001-use-ast-first-output-pipeline.md),
  [ADR 0002](0002-reject-post-write-repair.md)
- [ADR 0006](0006-typestate-output-gating.md) — `PreAcceptProject` /
  `AcceptedProject` typestate that gates these transforms.
- [module-boundaries.md](../architecture/module-boundaries.md) — "Pre-Accept
  Output Stage" and "Source Surgery" sections, and the byte-surgery debt
  registry.
