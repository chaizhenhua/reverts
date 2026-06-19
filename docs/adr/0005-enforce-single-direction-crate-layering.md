# ADR 0005: Enforce Single-Direction Crate Layering

## Status

Accepted — 2026-05-27

## Context

Reverts Next is split into ~17 crates organized as a layered compiler pipeline:
foundation primitives, input conversion, graph/model analysis, planning,
emission, and orchestration. The intended rule is that production dependencies
flow strictly downward — lower layers must not depend on higher ones — so the
core stays pure and the side-effecting layers (`reverts-cli`,
`reverts-rollup-adapter`) sit at the top.

The dependency directions were documented in
[module-boundaries.md](../architecture/module-boundaries.md), but documentation
alone does not prevent an upward edge from being added during a refactor. A
partial boundary test existed; it checked only a handful of named forbidden
edges, leaving most of the documented DAG unenforced.

## Decision

The crate layering is machine-enforced in
`crates/reverts-cli/tests/architecture_boundaries.rs`:

- Every workspace `reverts-*` crate is assigned a numeric **layer rank**.
- Every production (`[dependencies]`) edge must point to a strictly lower rank.
- Dev-dependencies and build-dependencies are excluded; test-support crates
  (`reverts-fixtures`) may be used by any crate as a dev-dependency but must
  never appear as a normal dependency.
- A `reverts-*` crate with no assigned rank fails the test, forcing a conscious
  placement decision when a new crate is introduced.

The ranks are the authoritative encoding of the layering; the prose tables in
`module-boundaries.md` must stay consistent with them.

## Consequences

- An accidental upward edge (e.g. a foundation crate importing the planner)
  fails CI instead of silently eroding the architecture.
- Adding a crate is a deliberate act: the author must rank it and update the
  boundary doc.
- The layering can still evolve, but only by editing the rank table on purpose,
  which keeps the change reviewable.
- `reverts-fixtures` is guaranteed to stay out of the production build.

## References

- [module-boundaries.md](../architecture/module-boundaries.md) — crate map,
  dependency-direction diagram, and filesystem/network access matrix.
- `crates/reverts-cli/tests/architecture_boundaries.rs` — the enforcement.
