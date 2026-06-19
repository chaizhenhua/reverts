# ADR 0002: Reject Post-Write Repair

## Status

Accepted — 2026-05-14

## Context

Post-write repair, rescue, and final sweep passes can make broken output appear
to work while leaving the upstream emitter incorrect. They also make failures
harder to reproduce because the observable file is no longer the direct result
of the planner.

## Decision

Reverts Next does not add post-write repair, rescue, or final sweep passes. A
bug discovered in emitted output must be fixed at the input, graph, constraint,
planner, or emitter boundary that produced the invalid structure.

## Consequences

- The project writer only persists accepted output and does not infer missing
  declarations or patch imports.
- Structural audit findings remain visible until the upstream mechanism is
  corrected.
- Tests must target the failure mode directly instead of asserting that a repair
  pass covered the symptom.
- Some failures may initially block output, which is preferred to silently
  emitting misleading source.

## References

- [ADR 0001](0001-use-ast-first-output-pipeline.md) is the positive form of this
  decision: build structure before writing instead of patching after.
- [ADR 0008](0008-allow-in-memory-pre-accept-transforms.md) carves out the
  ordered, audited in-memory transforms that are *not* post-write repair, and
  explains the distinction.
