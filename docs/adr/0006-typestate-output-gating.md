# ADR 0006: Gate Output With Typestate

## Status

Accepted — 2026-05-27

## Context

The output pipeline has several points where data is only safe to use once it
has passed a validation step: a plan must be structurally validated before the
emitter runs, and emitted bytes must pass the acceptance audit before they reach
the filesystem. If these stages share a single type, nothing stops a caller from
handing unvalidated data to the next stage, which is exactly how the previous
output path let invalid output reach disk.

## Decision

Each validation boundary is encoded as a distinct type, so the type system — not
caller discipline — enforces the ordering:

- `EmitPlan` → `ValidatedEmitPlan` / `ValidatedPlannedFile`. Validation rejects
  duplicate output paths, empty file paths, duplicate planned imports, duplicate
  generated exports, empty import namespaces, generated exports without a
  declaration/import, and synthetic planned bindings. The emitter only accepts
  `ValidatedEmitPlan`.
- `EmittedProject` → `PreAcceptProject` → `AcceptedProject`. Pre-accept
  transforms (see [ADR 0008](0008-allow-in-memory-pre-accept-transforms.md))
  produce a `PreAcceptProject`. Only audit-clean output becomes an
  `AcceptedProject`. The project writer consumes `AcceptedProject`, never raw
  `EmittedProject`.

A downstream stage cannot be called with the upstream type, so skipping
validation is a compile error rather than a latent runtime bug.

## Consequences

- "Has this been validated/audited?" is answered by the type, not by reading the
  call site.
- The writer cannot persist unaudited bytes; the emitter cannot run on an
  unvalidated plan.
- New invariants are added by strengthening the validation constructor for the
  next typestate rather than by adding scattered runtime checks.
- The cost is more types and explicit conversions, which is the intended
  trade-off.

## References

- [ADR 0002](0002-reject-post-write-repair.md) — the writer persists only
  accepted output.
- [module-boundaries.md](../architecture/module-boundaries.md) — "Data Flow"
  shows the full `InputBundle → … → AcceptedProject` chain.
- `reverts-planner::plan` (`ValidatedEmitPlan`),
  `reverts-pipeline::pre_accept` (`PreAcceptProject`, `AcceptedProject`).
