---
name: electron-collector
description: Collect Electron application artifacts from DMG, extracted .app bundles, Resources directories, or ASAR inputs and run the end-to-end ReverTS decompilation handoff through ingest, generation, and Electron runtime validation.
---

# Electron Collector

Use this skill to turn an Electron app artifact into the standard ReverTS app
artifact manifest, import it with the `reverts-cli` binary, and hand off to the
standard decompile + post-export validation skills. Electron `.dmg` and `.img`
disk images are first-class inputs when they contain a `.app` bundle. An
already-extracted `<App>.app` directory is also a first-class input; pass the
`.app` directory, not nested `Contents/Resources/*.img` helper images, unless
the task is binary disk-image reverse engineering rather than Electron source
recovery.

## Agent Boundary

This skill is for artifact collection and recovery orchestration. Do not use an
Agent to patch generated files, repair imports, edit `package.json`, or inspect
long logs for mechanical recovery issues. Mechanical issues must be captured as
ReverTS work items and validation reports. The Agent's role is semantic
renaming after mechanical recovery has produced a rename worklist.

## End-to-End Contract

For Electron decompilation requests, do not stop after producing
`artifact-manifest.json`. The successful path is:

1. Collect the artifact manifest with this skill's script.
2. Run `reverts-cli import-unpacked --input <Contents/Resources/app>
   --manifest <manifest.json> --project-name <name> --output-db <db.sqlite>`.
   This single command creates the project, registers source units, and
   discovers modules — there is no separate "create project" or "discovery
   toggle" step.
3. Validate inventory with `reverts-cli full-inventory --input <db>
   --project-id <id>`.
4. Run [decompile](../decompile/SKILL.md) — it now drives `reverts-cli` —
   until its public-surface gate passes, then run `reverts-cli
   generate --input <db> --project-id <id> --output <dir>
   --source-root src` with strict gates enabled.
5. Run [reverts-decompile](../reverts-decompile/SKILL.md) using the Electron
   runtime profile: install, `tsc --noEmit`, Electron startup, and renderer
   UI/console checks through Playwright/CDP when available.

`import-unpacked` always discovers modules; there is no inventory-only import
mode. A full decompilation run therefore has modules to process as soon as the
import succeeds. Large Electron apps can spend many minutes in import discovery
and variable-flow analysis; let the command run to completion, and inspect the
persisted SQLite project/source-unit/module counts (via `full-inventory`)
before deciding that the operation failed.

## Install

This skill drives the `reverts-cli` binary, built at
`./target/release/reverts-cli` (rebuild with
`cargo build --release --bin reverts-cli`). See
[skills/README.md](../README.md#install) for the full install matrix
(`npm install -g reverts`, local-dev `./skills/install`). The working tree
must contain the `reverts-cli` binary, the `skills/` directory (including
`electron-collector` and its `bin/` collector script), and Python 3 for the
collector.

## Platform Support

The collector script is cross-platform Python 3 and has no Node dependency
for ASAR parsing.

### Input dispatch

The collector resolves the input path against this ordered list (first match
wins). See `references/electron-artifact-model.md` for the full resolution
rules; below is the dispatch summary.

| Input shape | Detection | Extraction path | External tool |
|---|---|---|---|
| `.dmg` / `.img` file | suffix `.dmg` or `.img` | mount/extract → resolve as `.app` | `hdiutil` (macOS) or `7z` / `7zz` / `7za` (Linux/Windows) |
| mounted DMG / copied DMG dir | directory containing one `*.app/` | resolve as `.app` | none |
| macOS `.app` bundle | suffix `.app` | use `Contents/Resources` | none |
| `Contents/Resources/` | directory containing `app.asar` or renderer assets | use as `artifact_root` | none |
| Windows/Linux Electron root | directory containing `resources/` | descend into `resources/` | none |
| `resources/` directory | contains `app.asar` / `package.json` / renderer assets | use as `artifact_root` | none |
| `app.asar` | path ends `app.asar` | parse with bundled Python ASAR reader, extract code-like entries to stage dir | none |
| `app.asar.unpacked/` | path ends `app.asar.unpacked` | inventory filesystem entries directly | none |

Source-unit roles assigned during inventory: `main`, `preload`, `renderer`,
`html_entry`, `js_chunk`, `vendor_chunk`, `worker`, `source_map`,
`native_addon`, `package_manifest`, `asset`. See
[electron-artifact-model.md](references/electron-artifact-model.md#role-heuristics)
for the role-assignment heuristics.

## Collector Command

Run the bundled script from the repository root:

```bash
python3 skills/electron-collector/bin/collect_electron_artifact \
  /path/to/ElectronApp-or-DMG-or-IMG \
  --output-manifest /tmp/electron-artifact-manifest.json \
  --stage-dir /tmp/electron-collector-stage \
  --json-report
```

Use `--ingest-include` / `--ingest-exclude` to bound validation runs while keeping full inventory semantics. Example:

```bash
python3 skills/electron-collector/bin/collect_electron_artifact \
  /path/to/ElectronApp.app \
  --output-manifest /tmp/electron-artifact-manifest.json \
  --stage-dir /tmp/electron-stage \
  --ingest-include '<renderer-dist>/feature/*.js' \
  --ingest-include '<renderer-dist>/assets/c*.js' \
  --ingest-exclude '<renderer-dist>/assets/index-*.js' \
  --json-report
```

The manifest always records source units and edges. Only JS/TS-family sources passing the ingest filters are enabled for ReverTS ingestion.

## ReverTS Workflow

1. Run the collector script and inspect the JSON report.
2. Run `reverts-cli import-unpacked --input <Contents/Resources/app>
   --manifest <manifest.json> --project-name <name> --output-db <db.sqlite>`
   to create the project, register artifact inventory, and discover modules in
   one step. Note the project id assigned in the new SQLite database for the
   following steps.
3. Validate inventory with `reverts-cli full-inventory --input <db>
   --project-id <id> --json <inventory.json>` and compare source-unit counts
   against the collector manifest.
4. Run the standard [decompile](../decompile/SKILL.md) control loop (it drives
   `reverts-cli`) for semantic naming and strict output generation, ending with
   `reverts-cli generate --input <db> --project-id <id>
   --output <dir> --source-root src`.
5. Run [reverts-decompile](../reverts-decompile/SKILL.md) with the Electron
   validation profile. Do not substitute browser-extension or web-app checks
   for Electron unless artifact metadata says the project has multiple
   profiles.
6. Only after mechanical recovery is structurally valid, use the semantic-name
   commands (`symbol-names`, `binding-names`, `naming-plan`) for Agent naming.

For full recovery, do not use ingest filters as a degradation mechanism. Filters are only for smoke validation of collector behavior. Full recovery should inventory and ingest all JS/TS-family source units that the manifest marks as recoverable.

Future recovery tools should collapse steps 2-5 into a persistent recovery job,
but the boundary remains the same: collector and ReverTS code perform mechanical
recovery; Agent naming is separate.

## Completion Criteria

A collection run is considered successful only when **all** of the following hold:

1. The collector exits 0 and the JSON report contains `status: "ok"`.
2. The manifest has `schema_version == 1` and a non-empty `artifact_root`.
3. `sources` is non-empty and contains at least one of `main`, `renderer`,
   or `js_chunk` source units.
4. Every entry with `ingest_enabled == true` has a real path under the
   stage directory or directly inside `artifact_root` (no dangling refs).
5. After `reverts-cli import-unpacked ...`, the source-unit count reported by
   `reverts-cli full-inventory --input <db> --project-id <id>` matches the
   manifest. A mismatch indicates an import bug — file it as a
   ReverTS issue, do not retry blindly.
6. `reverts-cli naming-progress --input <db> --project-id <id>` (and
   `coverage-ledger` for the unified ledger) can see discovered modules. If
   discovery fails during import, fix the pipeline mechanism and rerun
   collection + import; do not remove source units or mutate the manifest to
   bypass the failure.
7. Post-export validation completes through the Electron profile from
   `reverts-decompile`: dependency install succeeds, real TypeScript compile
   runs, Electron startup reaches renderer readiness, and captured
   console/page errors are clean or triaged as ReverTS work items.

Stop the workflow and report failure if any of these fails. Do not patch
the manifest or hand-edit the stage directory to make a bad run look good.

## Failure Recovery

| Failure | Signal | Action |
|---|---|---|
| `hdiutil` missing on macOS | collector falls through to `7z*` and reports tool missing | Install Xcode command-line tools (`xcode-select --install`) and retry |
| All `7z` variants missing on Linux/Windows for `.dmg` / `.img` | collector reports missing disk-image extractor | Install `p7zip-full` (apt) / `7zip` (winget); retry |
| `.dmg` / `.img` mounts but yields no `.app` | mount/extract succeeds, no `*.app` under extracted root | Inspect the disk image manually; the app may be inside a nested HFS/APFS/ISO image — pass that nested path explicitly |
| ASAR header invalid / truncated | collector raises `ASAR header parse error` | Re-fetch the artifact (file likely truncated during transfer); do not "repair" the bytes |
| Stage dir not writable | extraction fails with `EACCES` / `EROFS` | Pass `--stage-dir` to a writable path and retry |
| `import-unpacked` rejects manifest | `reverts-cli` exits non-zero with a manifest coverage/evidence error | Fix the collector or ReverTS import mechanism with a regression test; do NOT mutate JSON to satisfy the validator |
| `import-unpacked` fails during source preparation or discovery | `reverts-cli` error names source-unit IDs and parse/format reason | Reproduce with the named source, add a ReverTS regression test, fix the upstream parser/metadata/discovery path, regenerate the manifest, and import again |
| `import-unpacked` runs long during discovery | SQLite source-unit/module counts are increasing or logs show variable-flow/signature work | Let the command finish; on interruption, inspect persisted counts with `full-inventory` and continue from the existing DB rather than re-importing blindly unless no project row was created |
| Source-unit count mismatch after import | `full-inventory` source-unit count != `manifest.sources.length` | Stop; file as ReverTS bug. Re-importing will mask the defect |

If a real DMG cannot be mounted or extracted on the current host, stop and
report the missing platform tool. Do not fake a manifest for unavailable
files.

## Tool Summary

| Step | Tool |
|---|---|
| Collect `.dmg` / `.img` / `.app` / `Resources` / `app.asar` | `python3 skills/electron-collector/bin/collect_electron_artifact ... --json-report` |
| Project + import + discovery | `reverts-cli import-unpacked --input <app> --manifest <manifest.json> --project-name <name> --output-db <db.sqlite>` |
| Inventory + progress | `reverts-cli full-inventory --input <db> --project-id <id> [--json <file>]`, `reverts-cli naming-progress --input <db> --project-id <id> [--json]`, `reverts-cli coverage-ledger --input <db> --project-id <id>` |
| Semantic naming + output | [decompile](../decompile/SKILL.md) (drives `reverts-cli`), ending with `reverts-cli generate --input <db> --project-id <id> --output <dir> --source-root src` |
| Install/compile/runtime/UI | [reverts-decompile](../reverts-decompile/SKILL.md), Electron profile in `references/runtime-validation-profiles.md` |

## Output Contract

The script writes the collector manifest that feeds `reverts-cli
import-unpacked --manifest`:

```json
{
  "schema_version": 1,
  "profile": "electron",
  "artifact_root": "/absolute/path/to/Resources",
  "sources": [],
  "edges": [],
  "metadata": {
    "collector": "electron-collector"
  }
}
```

<!-- TODO(reverts-cli): manifest shape reconciliation — `import-unpacked
--manifest` expects a `reverts.import_evidence.v1` manifest (every input file
covered, with matching size/hash evidence), but this collector emits a
`schema_version: 1`, `profile: electron` manifest. Reconcile the collector
output with the import-evidence schema (or add a converter) before relying on a
direct hand-off. -->

The `full-inventory --manifest` flag likewise expects a
`reverts-import-evidence.json`; the same reconciliation applies when passing the
collector manifest there for coverage counts.

Source unit roles include `main`, `preload`, `renderer`, `html_entry`, `js_chunk`, `vendor_chunk`, `worker`, `source_map`, `native_addon`, `package_manifest`, and `asset`.

Read `references/electron-artifact-model.md` only when changing collector
behavior or debugging role/path classification.
