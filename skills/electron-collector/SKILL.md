---
name: electron-collector
description: Collect Electron application artifacts from DMG, extracted .app bundles, Resources directories, or ASAR inputs and run the end-to-end ReverTS decompilation handoff through ingest, generation, and Electron runtime validation.
---

# Electron Collector

Use this skill to turn an Electron app artifact into the standard ReverTS app
artifact manifest, ingest it through the MCP server, and hand off to the
standard decompile + post-export validation skills. Electron `.dmg` and `.img`
disk images are first-class inputs when they contain a `.app` bundle. An
already-extracted `Claude.app` directory is also a first-class input; pass the
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
2. Create or resolve a ReverTS project rooted at the Electron app directory.
3. Call `ingest_app_manifest(..., run_discovery=true)` so source-unit
   registration and module discovery run through the ReverTS pipeline.
4. Validate inventory with `list_app_artifacts` and `get_artifact_manifest`.
5. Run [decompile](../decompile/SKILL.md) until its public-surface gate passes
   and `generate_app_decompiled_files` succeeds with strict gates enabled.
6. Run [reverts-decompile](../reverts-decompile/SKILL.md) using the Electron
   runtime profile: install, `tsc --noEmit`, Electron startup, and renderer
   UI/console checks through Playwright/CDP when available.

Use `run_discovery=false` only for inventory-only debugging. A full
decompilation run needs discovery enabled; otherwise the later decompile phases
have no modules to process. Large Electron apps can spend many minutes in
discovery and variable-flow analysis; use an MCP client/tool timeout long enough
for the run, and inspect persisted project/artifact counts before deciding that
the operation failed.

## Install

This skill ships with the `reverts` MCP server distribution. See
[skills/README.md](../README.md#install) for the full install matrix
(`npm install -g reverts`, local-dev `./skills/install`, MCP server
registration). The release must contain the `reverts-mcp` binary, the
`skills/` directory (including `electron-collector` and its `bin/`
collector script), and the npm launcher when installed via npm.

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
  --ingest-include 'ion-dist/audio/voice/*.js' \
  --ingest-include 'ion-dist/assets/v1/c*.js' \
  --ingest-exclude 'ion-dist/assets/v1/index-*.js' \
  --json-report
```

The manifest always records source units and edges. Only JS/TS-family sources passing the ingest filters are enabled for ReverTS ingestion.

## ReverTS Workflow

1. Resolve or create a ReverTS project for the app directory or artifact root.
2. Run the collector script and inspect the JSON report.
3. Call `ingest_app_manifest(project_id, manifest_path, run_discovery=true)` to register artifact inventory and discover modules.
4. Validate inventory with `list_app_artifacts` and `get_artifact_manifest`.
5. Run the standard [decompile](../decompile/SKILL.md) control loop for
   semantic naming and strict output generation.
6. Run [reverts-decompile](../reverts-decompile/SKILL.md) with the Electron
   validation profile. Do not substitute browser-extension or web-app checks
   for Electron unless artifact metadata says the project has multiple
   profiles.
7. Only after mechanical recovery is structurally valid, use semantic rename
   worklist tools for Agent naming.

For full recovery, do not use ingest filters as a degradation mechanism. Filters are only for smoke validation of collector behavior. Full recovery should inventory and ingest all JS/TS-family source units that the manifest marks as recoverable.

Future recovery tools should collapse steps 3-6 into a persistent recovery job,
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
5. After `ingest_app_manifest(project_id, manifest_path, run_discovery=true)`,
   `list_app_artifacts(project_id)` returns the same source-unit count as
   the manifest. A mismatch indicates an ingestion bug — file it as a
   ReverTS issue, do not retry blindly.
6. `decompile_status(project_id)` can see discovered modules. If discovery
   fails during ingest, fix the pipeline mechanism and rerun collection +
   ingest; do not remove source units or mutate the manifest to bypass the
   failure.
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
| `ingest_app_manifest` rejects manifest | MCP returns schema validation error | Fix the collector or ReverTS ingest mechanism with a regression test; do NOT mutate JSON to satisfy the validator |
| `ingest_app_manifest` fails during source preparation or discovery | MCP error names source-unit IDs and parse/format reason | Reproduce with the named source, add a ReverTS regression test, fix the upstream parser/metadata/discovery path, regenerate the manifest, and ingest again |
| MCP client times out during discovery | DB/source-unit/module counts are increasing or logs show variable-flow/signature work | Reconnect with a longer timeout and continue from persisted state; do not re-ingest blindly unless no artifact row was created |
| Source-unit count mismatch after ingest | `list_app_artifacts.count` != `manifest.sources.length` | Stop; file as ReverTS bug. Re-ingesting will mask the defect |

If a real DMG cannot be mounted or extracted on the current host, stop and
report the missing platform tool. Do not fake a manifest for unavailable
files.

## Tool Summary

| Step | Tool |
|---|---|
| Collect `.dmg` / `.img` / `.app` / `Resources` / `app.asar` | `python3 skills/electron-collector/bin/collect_electron_artifact ... --json-report` |
| Project + ingest | ReverTS MCP `create_project`, `ingest_app_manifest`, `list_app_artifacts`, `get_artifact_manifest` |
| Semantic naming + output | [decompile](../decompile/SKILL.md): `decompile_status`, `query`, `submit_module_decompilation`, `update_modules`, `generate_app_decompiled_files` |
| Install/compile/runtime/UI | [reverts-decompile](../reverts-decompile/SKILL.md), Electron profile in `references/runtime-validation-profiles.md` |

## Output Contract

The script writes a manifest accepted by `ingest_app_manifest`:

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

Source unit roles include `main`, `preload`, `renderer`, `html_entry`, `js_chunk`, `vendor_chunk`, `worker`, `source_map`, `native_addon`, `package_manifest`, and `asset`.

Read `references/electron-artifact-model.md` only when changing collector
behavior or debugging role/path classification.
