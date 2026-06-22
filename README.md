# ReverTS

**Decompile minified production JavaScript bundles back into readable,
runnable, semantically-named TypeScript projects.**

Most "unminifiers" stop at reformatting one file and renaming a few variables.
ReverTS does *structural* recovery: it splits a single esbuild / webpack /
rollup bundle back into its original modules, restores the `import`/`export`
boundaries between them, externalizes the npm dependencies that were inlined
into the bundle, recovers real export and function names, and then **installs,
type-checks, and runs** the reconstructed project to prove the output actually
works.

> ⚠️ **Intended for authorized analysis only** — security research,
> malware/extension review, interoperability, and recovering source you own.
> Respect software licenses and terms of service before decompiling anything.

## What makes it different

- **Structure recovery** — one bundle becomes the original module tree with real
  `import`/`export` edges, not one giant beautified file.
- **Package externalization** — detects npm dependencies that the bundler
  inlined and restores them to genuine boundaries (`require('electron')`,
  `import ... from 'ws'`, `undici`, …) instead of emitting third-party code as
  if it were yours.
- **Semantic naming** — recovers real export and function names instead of `a`,
  `b`, `c`, using cross-version reference matching against historical sources.
- **Validated output** — the rebuilt project is installed, compiled, and run.
  ReverTS only ships output it can stand behind, enforced by audit invariants
  rather than hope.

## Inputs

ReverTS ingests real-world artifacts end-to-end:

| Input | Source formats |
|-------|----------------|
| Electron apps | `.dmg`, extracted `.app`, `Resources/`, ASAR |
| Browser extensions | CRX, XPI, unpacked, installed |
| Websites / SPAs | live URL, HAR capture, downloaded asset directory |
| Raw bundles | esbuild / webpack / rollup output |

## Proven on

- **The Claude desktop app** (Electron) — recovered through module resolution,
  CommonJS→ESM lowering, and multi-bundle separation, all the way down to its
  genuine `require('electron')` external boundary.
- **cc-2.1.89** — self-decompile case study.

Full write-ups live in [`docs/research/`](docs/research/).

## Getting started

One command installs the pre-compiled `reverts-cli` binary and the skill
bundle — no Node toolchain and no MCP server required:

```bash
curl -fsSL https://raw.githubusercontent.com/chaizhenhua/reverts-next/main/install.sh | sh
```

This downloads the binary from [GitHub Releases](https://github.com/chaizhenhua/reverts-next/releases)
into `~/.reverts/bin` and installs the skills into `~/.claude/skills` (and
`~/.codex/skills` when present). Supported platforms: Linux and macOS on x86_64
and aarch64. Override the install with `REVERTS_VERSION`, `REVERTS_HOME`,
`REVERTS_SKILLS_DIR`, or `REVERTS_NO_SKILLS=1` (see [`install.sh`](install.sh)).

After installing, restart your Claude/Codex session so the skill registry
rebinds. The skills appear under the `reverts:` namespace:

- `reverts:electron-collector` — collect an Electron app and hand off to decompile
- `reverts:browser-extension-collector` — collect a CRX/XPI/unpacked extension
- `reverts:website-collector` — capture a live URL / HAR / asset directory
- `reverts:decompile` — the core webpack/esbuild bundle decompilation pipeline
- `reverts:reverts-decompile` — post-export install / `tsc` / startup validation

The skills drive `reverts-cli` directly; see [`skills/README.md`](skills/README.md)
for the skill layout. To install from source instead, build with
`cargo build --release --bin reverts-cli` and symlink the skills with
`./skills/install`.

### Core CLI

`reverts-cli` is a Rust toolbox that operates on a SQLite facts database. The
generation step is:

```bash
reverts-cli generate-project-v2 --input <facts.db> --project-id <id> \
    --output <out-dir> --source-root src
```

Run `reverts-cli --help` for the full command set (ingest, package matching,
externalization, naming, inventory, and coverage reporting).

## How it works

Pure Rust, AST-first on [OXC](https://github.com/oxc-project/oxc). There is no
regex or string-level source rewriting, and **no post-write repair** — any
defect in the output is fixed at the input, graph, planner, or emitter boundary
that produced it, never patched after the fact. Analysis runs over first-class
graphs (module, def-use, import/export, function-call, module-init, and
resolved-symbol), and every emitted file must clear a set of audit invariants
before it is written to disk.

Data flow:

```
InputBundle → RevertsGraph → DefUseGraph → ImportExportGraph →
PackageSurfaceResolution → EmitPlan → EmittedProject → AuditReport
```

Deeper architecture docs:
[decompilation output](docs/architecture/decompilation-output-v2.md),
[crate boundaries](docs/architecture/module-boundaries.md),
[input data model](docs/architecture/input-data-model.md).

## Development

The workspace is pinned to Rust `1.93.0` (`rust-toolchain.toml`). Every gate
runs with `--locked` so a `Cargo.lock` drift fails fast.

```bash
cargo fmt --check                                          # formatting (edition 2024)
cargo clippy --workspace --locked --all-targets -- -D warnings
cargo test --workspace --locked
```

Git hooks live in `lefthook.yml` (install once via `scripts/install-hooks.sh`).
The hooks enforce `rustfmt --edition 2024` and `cargo check` on commit, and
`cargo clippy -D warnings` + `cargo test` on push. The same checks run in CI on
every push and pull request to `main` (`.github/workflows/ci.yml`).

## Architecture decisions

- [ADR 0001: AST-First Output Pipeline](docs/adr/0001-use-ast-first-output-pipeline.md)
- [ADR 0002: Reject Post-Write Repair](docs/adr/0002-reject-post-write-repair.md)
- [ADR 0004: Bundler-Aware Module Extraction](docs/adr/0004-bundler-aware-module-extraction.md)
- [ADR 0006: Typestate Output Gating](docs/adr/0006-typestate-output-gating.md)

## Research basis

The output core follows decompilation research on explicit intermediate
representations, graph invariants, shape/type constraints, compiler-aware
recovery, and semantics-preserving emission. The working bibliography is in
[docs/research/decompilation-references.md](docs/research/decompilation-references.md).

## License

Apache License 2.0 — see [`LICENSE`](LICENSE). Copyright 2026 The ReverTS Authors.
