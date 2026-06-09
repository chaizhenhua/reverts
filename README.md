# Reverts Next

Reverts Next is a clean implementation line for the decompilation output core.
It starts from explicit IR, constraints, package-surface decisions, emit plans,
and audit reports instead of legacy post-write repair passes.

The repository intentionally has no imported Git history from the prior
implementation. Existing code may be used as reference material only when it is
copied behind the new crate boundaries and covered by self-contained tests.

## Architecture

The detailed output architecture is maintained in
[docs/architecture/decompilation-output-v2.md](docs/architecture/decompilation-output-v2.md).
The shorter invariant summary remains in
[docs/architecture/output-core.md](docs/architecture/output-core.md).

## Architecture Decisions

- [ADR 0001: Use an AST-First Output Pipeline](docs/adr/0001-use-ast-first-output-pipeline.md)
- [ADR 0002: Reject Post-Write Repair](docs/adr/0002-reject-post-write-repair.md)
- [ADR 0003: Require Self-Contained Failure-Mode Tests](docs/adr/0003-require-self-contained-failure-mode-tests.md)

## Research Basis

The output core follows decompilation research on explicit intermediate
representations, graph invariants, shape/type constraints, compiler-aware
recovery, and semantics-preserving emission. The working bibliography and its
project impact are maintained in
[docs/research/decompilation-references.md](docs/research/decompilation-references.md).
