# ADR 0007: Split Responsibilities Between Skills and Reverts Code

## Status

Accepted — 2026-05-27

## Context

End-to-end decompilation starts from environment-specific, messy inputs: Bun
single-file executables, Electron `.app`/`.dmg` bundles, browser-extension
CRX/XPI/ZIP archives. Unpacking these needs format parsers, platform tools
(`7z`, Chromium `--load-extension`, macOS signing), and network access to stores.
None of that belongs inside the deterministic, self-contained analysis core
required by [ADR 0003](0003-require-self-contained-failure-mode-tests.md).

## Decision

There is one boundary, applied consistently:

- **Skills** (`auto-unpack-target`, `unpack-bunfs`, `unpack-electron-app`,
  `unpack-browser-extension`, and the validation skills) own external formats,
  downloads, platform tooling, and environment-specific unpacking. They emit a
  manifest (`auto-unpack-report.json` and per-target manifests) plus an extracted
  root, and they own the *original-artifact* smoke validation.
- **Reverts code** owns everything from the manifest inward: importing facts into
  `InputBundle`/SQLite (`import-unpacked`), module classification
  (`module-classify`), package matching, public-surface extraction and the naming
  gate (`naming-plan` / `naming-progress`), planning, emission, and audit. It
  reads manifests but never unpacks formats or reaches the network in required
  tests.

The crate-level consequence: the importer and surface/naming/validation use-cases
currently live as module seams inside `reverts-cli`. Promotion to dedicated
crates (`reverts-import`, `reverts-surface`, `reverts-validate`) is a deferred,
optional refactor, not a prerequisite — the Skill/Reverts boundary holds
regardless of whether those use-cases sit in `reverts-cli` or in their own
crates.

## Consequences

- The analysis core stays self-contained and testable without `node`, Chrome,
  npm, network, or macOS — those are opt-in integration concerns owned by Skills.
- New target formats are added as Skills that emit the same manifest contract;
  Reverts code does not grow a parser per format.
- Output validation that needs a real runtime (browser load, Electron launch) is
  Skill/integration territory; required Reverts tests assert structural failure
  modes instead.
- The proposed `reverts-import`/`reverts-surface`/`reverts-validate` crates
  remain a layout option, tracked in the workflow doc, not an obligation.

## References

- [unpack-to-validated-output-workflow.md](../architecture/unpack-to-validated-output-workflow.md)
  — the full workflow and the Skill-vs-Reverts responsibility table.
- [ADR 0003](0003-require-self-contained-failure-mode-tests.md) — why the core
  must stay free of external programs.
