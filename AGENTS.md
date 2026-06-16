# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

Rust workspace pinned to toolchain `1.93.0` (`rust-toolchain.toml`). All checks use `--locked`.

```bash
cargo fmt --check                                          # formatting (edition 2024)
cargo clippy --workspace --locked -- -D warnings           # lint gate (also run by pre-push)
cargo test --workspace --locked                            # full test suite (pre-push)
cargo check --workspace --locked                           # pre-commit gate
cargo test -p reverts-planner <name_substring>             # single test in one crate
cargo test -p reverts-planner -- --nocapture               # show stdout
```

Git hooks live in `lefthook.yml`. Install once via `scripts/install-hooks.sh` (requires `lefthook` on PATH or at `$HOME/.local/bin/lefthook`). The hooks enforce `rustfmt --edition 2024` on staged files, `cargo check` on commit, and `cargo clippy -D warnings` + `cargo test` on push.

## Architecture

This repository is the clean-start implementation of Reverts' decompilation output core; it has no imported git history from the prior implementation. Legacy code may be used as reference only when copied behind new crate boundaries with self-contained tests.

### Workspace layout

`Cargo.toml` lists 16 active members; all are production crates (no placeholders). The authoritative map of responsibilities, layer assignments, and allowed dependency edges lives in `docs/architecture/module-boundaries.md`. The active monolith-reduction work and its phased plan live in `docs/architecture/refactor-roadmap.md` — consult that before starting any new extraction so you don't conflict with in-flight phases.

### Pipeline and dependency direction

The output pipeline flows strictly downward — lower layers must not depend on higher layers:

```
reverts-cli → reverts-pipeline → reverts-analyze → reverts-model → reverts-graph → reverts-input
                                                 → reverts-package
                              → reverts-planner → reverts-model, reverts-package
                              → reverts-emitter → reverts-planner
                              → reverts-observe
            → reverts-package-matcher → reverts-input, reverts-js, reverts-observe
```

Foundation crates (`reverts-js`, `reverts-ir`, `reverts-observe`, `reverts-fixtures`) may be used by any production crate. `reverts-fixtures` is test-only — production crates must not depend on it.

Data flow: `InputBundle → RevertsGraph → DefUseGraph → ImportExportGraph → PackageSurfaceResolution → EmitPlan → EmittedProject → AuditReport`. The planner and emitter only receive already-validated structural data.

### Non-negotiable design rules (encoded in ADRs)

- **AST-first only.** Source transformations use OXC parsing + codegen. Regex or string-based source manipulation is not acceptable for new output behavior when AST support exists. (ADR 0001)
- **No post-write repair.** No rescue, final-sweep, or string-rewrite passes. A bug in emitted output must be fixed at the input, graph, constraint, planner, or emitter boundary that produced it — never patched after writing. The project writer only persists accepted output. (ADR 0002)
- **Self-contained core tests.** Unit and integration tests must not depend on `node`, `npm`, network access, real package installations, real project databases, or state from prior runs. Failure modes become small in-memory fixtures. (ADR 0003)
- **Workspace lints (in `Cargo.toml`).** `unsafe_code` is forbidden; `clippy::correctness`, `clippy::unwrap_used`, `clippy::todo`, and `clippy::dbg_macro` are denied. Code must compile clean under these.

### Output invariants enforced by audits

Every emitted file must parse with OXC; every read must have a definition, import, or finding; every synthetic reference must have a same-file declaration or import; every bare package import must have a package-surface decision; duplicate top-level bindings must be reported; the entry dispatcher must not statically import runtime-heavy modules. These come from `docs/architecture/decompilation-output-v2.md` and `docs/architecture/output-core.md`.

## Repository conventions

- **Commit message format is enforced** (`lefthook.yml` `commit-msg`): exactly one line, ≤100 chars, matching `<emoji> <type>(<scope>): <subject>` (e.g. `✨ feat(ir): add module graph primitives`). Commits with `Co-Authored-By:`, AI generation markers (`Generated with/by`, `🤖 Generated`), or tool/platform provenance lines are rejected. The same check runs on rebased commits via `post-rewrite`.
- **Root markdown files** are restricted to `README.md`, `AGENTS.md`, `CHANGELOG.md`, `CONTRIBUTING.md`, `LICENSE.md`. All other docs live under `docs/` — ADRs in `docs/adr/`, architecture in `docs/architecture/`, research in `docs/research/`.
- **Vertical-slice workflow.** New behavior follows: smallest failing self-contained test → implementation → `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked` → single-line conventional commit.
