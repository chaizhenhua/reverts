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
Crate responsibilities and allowed dependency directions are defined in
[docs/architecture/module-boundaries.md](docs/architecture/module-boundaries.md).
The input-side data model is documented in
[docs/architecture/input-data-model.md](docs/architecture/input-data-model.md).
The shorter invariant summary remains in
[docs/architecture/output-core.md](docs/architecture/output-core.md).

## Development

The workspace is pinned to Rust `1.93.0` (`rust-toolchain.toml`). Every gate
runs with `--locked` so a Cargo.lock drift fails fast.

```bash
cargo fmt --check                                          # formatting (edition 2024)
cargo clippy --workspace --locked --all-targets -- -D warnings
cargo test --workspace --locked
cargo test -p reverts-pipeline --test external_corpus -- --ignored --nocapture  # full corpus (~1s)
```

Git hooks live in `lefthook.yml`. Install once via
`scripts/install-hooks.sh` (requires `lefthook` on `PATH` or at
`$HOME/.local/bin/lefthook`). The hooks enforce `rustfmt --edition 2024` and
`cargo check` on commit, and `cargo clippy -D warnings` + `cargo test` on
push. The same checks also run in CI on every push and pull request to
`main` (`.github/workflows/ci.yml`).

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
