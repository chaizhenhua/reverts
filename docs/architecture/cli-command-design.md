# CLI command design

The `reverts-cli` surface grew one flat command at a time. This note records the
target shape and the migration discipline so the surface stays coherent as it
grows.

## Principles

1. **No version suffixes in command names.** A command name is a contract, not a
   changelog. `generate` — never `generate-project-v2`. When a command is
   reworked, rename in place and keep the old name as a hidden alias for one or
   two releases (see Migration).
2. **The pipeline stages are verbs; everything else groups under a noun.** The
   recovery flow reads as a sentence: `import → match → classify → name →
   generate`. Supporting tools group by the resource they act on (`package`,
   `report`).
3. **One verb per operation, applied consistently.** Assigning a semantic name is
   `name <subject>`, never four unrelated `*-names` commands plus two `naming-*`
   commands.
4. **Reads vs. writes.** Mutating commands take `--apply` (dry-run by default).
   Read-only state lives under `report`.

## Target shape

Top level exposes five pipeline verbs and a few noun groups instead of ~24 flat
commands:

```
reverts import <path>             # ingest unpack evidence
reverts match            [--apply]   # populate package attributions/surfaces
reverts classify         [--apply]   # application/third-party/runtime-glue
reverts name <subject>   [--apply]   # symbols | bindings | modules | clusters | plan | progress | from-reference | from-package
reverts generate -o <dir>            # emit the TypeScript project

reverts package <cmd>                # candidates | hints | surface | versions | cache {audit,prune}
reverts report  <cmd>                # coverage | inventory | identifiers | runtime | packages
reverts assets  extract
reverts dev     recall               # evaluation harness, not a pipeline stage
```

### Old → new mapping

| Current | Target |
|---|---|
| `generate-project-v2` | `generate` ✅ **done** (alias kept) |
| `import-unpacked` | `import` |
| `match-packages` | `match` |
| `match-packages-report` | `report packages` |
| `match-modules-recall` | `dev recall` |
| `module-classify` | `classify` |
| `symbol-names` | `name symbols` |
| `binding-names` | `name bindings` |
| `module-names` | `name modules` |
| `cluster-names` | `name clusters` |
| `naming-plan` | `name plan` |
| `naming-progress` | `name progress` |
| `reference-source-names` | `name from-reference` |
| `ownership-source-names` | `name from-package` |
| `island-package-candidates` | `package candidates` |
| `package-externalization-hints` | `package hints` |
| `package-surface-decisions` | `package surface` |
| `package-version-diagnostics` | `package versions --diagnose` |
| `package-cache-audit` | `package cache audit` |
| `package-cache-prune-stale` | `package cache prune` |
| `full-inventory` | `report inventory` |
| `coverage-ledger` | `report coverage` |
| `identifier-inventory` | `report identifiers` |
| `runtime-inventory` | `report runtime` |
| `extract-assets` | `assets extract` |

Also fold the per-command `--input <DB>` / `--project-id <ID>` into top-level
`global` args: `reverts --db <DB> --project <ID> <command>`.

## Migration discipline (zero breakage)

Each rename keeps the old name working so skills and scripts never break in lock
step with the binary:

1. clap `#[command(name = "<new>", alias = "<old>")]` for dispatch.
2. The hand-rolled help (`help.rs::command_topic`) resolves the deprecated alias
   to the new help topic.
3. Skills and `docs/` (non-historical) use the new name; `docs/research/*`
   demos keep the name they actually ran for the record.
4. Remove the alias after one or two releases.

## Status

`generate-project-v2 → generate` has shipped under this discipline (clap alias +
help-topic alias; skills and architecture docs updated). The remaining rows are
mechanical but touch the hand-rolled help system (topic enum, per-command help
strings) and are best done as one focused pass rather than piecemeal, because
nesting (`reverts package cache audit`) requires restructuring `command_topic`
and the help-topic table together.
