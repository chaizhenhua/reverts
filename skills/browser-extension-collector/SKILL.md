---
name: browser-extension-collector
description: Collect browser-extension artifacts into ReverTS manifests for CRX, XPI, unpacked, or installed extensions before decompilation.
---

# Browser Extension Collector

Use this skill to turn a browser-extension artifact (`.crx`, `.xpi`, or
unpacked directory) into the standard ReverTS app artifact manifest, then
ingest it through the MCP server. Output schema is identical to
[electron-collector](../electron-collector/SKILL.md); only the `profile`
field and the role namespace differ.

## Install

Bundled with the `reverts` MCP server distribution. See
[skills/README.md](../README.md#install) for end-user install
(`npm install -g reverts`) and local-dev symlink installation
(`./skills/install`). Restart your Claude/Codex session after the first
install so the skill registry rebinds.

## Agent Boundary

This skill is for artifact collection and ingestion orchestration. Do not
patch generated files, hand-edit `manifest.json`, repair imports, or work
around malformed CRX signatures. Mechanical defects must be filed as
ReverTS pipeline issues; the agent's role here is collection + ingest +
semantic naming after structural recovery.

## Platform Support

The collector script is cross-platform Python 3 with no Node dependency.
ZIP and CRX parsing are bundled.

### Input dispatch

Inputs are resolved against this ordered table; first match wins.

| Input shape | Detection | Extraction path | External tool |
|---|---|---|---|
| `.crx` file | suffix `.crx` and magic `Cr24` at byte 0 | parse CRX2 / CRX3 header → strip → unzip embedded archive into stage dir | bundled (zipfile) |
| `.xpi` file | suffix `.xpi` (or `.zip` containing `manifest.json`) | unzip into stage dir | bundled (zipfile) |
| Unpacked extension directory | directory containing `manifest.json` at root | use as `artifact_root` directly | none |
| Chrome user-profile extension dir | path matches `Extensions/<id>/<version>/` and contains `manifest.json` | use as `artifact_root` directly | none |
| Directory with one nested unpacked extension | exactly one immediate subdirectory contains `manifest.json` | descend into the subdirectory | none |

Rejected inputs (collector exits non-zero, never silently degrades):

- `.zip` file without `manifest.json` at the archive root.
- `.crx` with magic mismatch, truncated header, or unsupported version
  (only CRX2 magic version `2` and CRX3 magic version `3` are supported).
- Web Store URLs (e.g. `chromewebstore.google.com/...`). Download the
  `.crx` locally and pass the path explicitly.

## Source-unit roles

Roles assigned during collection (extension namespace, in addition to the
shared `js_chunk` / `html_entry` / `source_map` / `asset` roles):

| Role | manifest.json field | Notes |
|---|---|---|
| `manifest_v3` | top-level `manifest_version: 3` | the manifest itself; one per extension |
| `manifest_v2` | top-level `manifest_version: 2` | the manifest itself; one per extension |
| `service_worker` | `background.service_worker` | MV3 only |
| `background_script` | `background.scripts[*]` | MV2 |
| `background_page` | `background.page` | MV2 (HTML host for background scripts) |
| `content_script` | `content_scripts[*].js[*]` | injected into matching pages |
| `content_style` | `content_scripts[*].css[*]` | injected stylesheets |
| `popup_html` | `action.default_popup` / `browser_action.default_popup` / `page_action.default_popup` | toolbar popup |
| `options_html` | `options_page` / `options_ui.page` | options page |
| `devtools_html` | `devtools_page` | devtools panel host |
| `sidebar_html` | `sidebar_action.default_panel` | Firefox sidebar |
| `override_html` | `chrome_url_overrides.{newtab,bookmarks,history}` | new-tab/bookmarks/history overrides |
| `accessible_chunk` | `web_accessible_resources[*].resources[*]` | files reachable from web pages |
| `locale_message` | `_locales/<lang>/messages.json` | i18n strings |
| `offscreen_html` | matched by path `*offscreen*.html` | MV3 offscreen documents (no manifest field) |

Source-unit `language` follows the same heuristic as electron-collector
(extension-based: `javascript`, `typescript`, `html`, `css`, `json`,
`source_map`, `native`, `asset`). Only the JS/TS family is `ingest`-eligible.

## Collector Command

Run the bundled script from the repository root:

```bash
python3 skills/browser-extension-collector/bin/collect_browser_extension_artifact \
  /path/to/Extension-or-CRX-or-XPI \
  --output-manifest /tmp/extension-artifact-manifest.json \
  --stage-dir /tmp/browser-extension-stage \
  --json-report
```

Filtering works identically to electron-collector:

```bash
python3 skills/browser-extension-collector/bin/collect_browser_extension_artifact \
  /path/to/Extension \
  --output-manifest /tmp/manifest.json \
  --stage-dir /tmp/stage \
  --ingest-include 'background/*.js' \
  --ingest-exclude 'vendor/*.js' \
  --json-report
```

The manifest always records source units and edges; `--ingest-include` /
`--ingest-exclude` are validation aids, not degradation switches.

## ReverTS Workflow

1. Resolve or create a ReverTS project for the extension (`name`/`version`
   from the parsed `manifest.json` are written to manifest metadata).
2. Run the collector script and inspect the JSON report.
3. Call `ingest_app_manifest(project_id, manifest_path, run_discovery=false)`
   to register artifact inventory.
4. Validate inventory with `list_app_artifacts` and `get_artifact_manifest`.
5. Run the standard ReverTS recovery phases (discovery, generation,
   validation, quality reporting).
6. After mechanical recovery is structurally valid, use
   [decompile](../decompile/SKILL.md) for the rename worklist and
   [reverts-decompile](../reverts-decompile/SKILL.md) for export validation.

## Completion Criteria

A collection run is considered successful only when **all** of the
following hold:

1. The collector exits 0 and the JSON report contains `status: "ok"`.
2. The manifest has `schema_version == 1`, `profile == "browser-extension"`,
   and a non-empty `artifact_root`.
3. `sources` contains exactly one source unit with role `manifest_v2` or
   `manifest_v3`, and that unit's parsed JSON has `name` + `version` +
   `manifest_version`.
4. If the manifest declares `background.service_worker` (MV3), exactly one
   source unit has role `service_worker` with a real physical path.
5. If the manifest declares `content_scripts`, every entry's `js[*]` and
   `css[*]` paths resolve to source units with role `content_script` /
   `content_style`.
6. After `ingest_app_manifest(project_id, manifest_path, run_discovery=false)`,
   `list_app_artifacts(project_id)` returns the same source-unit count as
   the manifest. Mismatch indicates an ingestion bug — file as a ReverTS
   issue, do not retry blindly.

Stop the workflow and report failure if any of these fails. Do not patch
the manifest or hand-edit the stage directory to make a bad run look good.

## Failure Recovery

| Failure | Signal | Action |
|---|---|---|
| CRX magic mismatch | first 4 bytes of `.crx` are not `Cr24` | re-download the `.crx`; the file is corrupt or not a CRX |
| Unsupported CRX version | header version field is not 2 or 3 | file a ReverTS feature request; do NOT mutate the bytes |
| CRX header truncated | EOF before `pubkey + signature + ZIP` (CRX2) or `protobuf header + ZIP` (CRX3) | re-download |
| `.xpi` is not a valid ZIP | `BadZipFile` from zipfile module | re-download; `.xpi` files are always ZIP archives |
| `manifest.json` missing | not at root after extraction | input is not an extension; check whether you passed the wrong directory level |
| `manifest.json` malformed | `json.JSONDecodeError` | bail with line/col; do NOT auto-fix |
| `manifest_version` outside {2, 3} | manifest field is missing or odd | warn and continue with role `manifest` (no version suffix); MV1 is unsupported, MV4 is forward-incompatible |
| Manifest references a missing file | e.g. `background.service_worker: "missing.js"` | record it in manifest `metadata.unresolved_manifest_references`; do NOT fabricate the file and do NOT emit a source-unit edge without a real target |
| Stage dir not writable | extraction fails with `EACCES` / `EROFS` | pass `--stage-dir` to a writable path and retry |
| `ingest_app_manifest` rejects manifest | MCP returns schema validation error | file as a ReverTS bug with the manifest attached; do NOT mutate JSON to satisfy validator |

If extraction succeeds but no JS/TS source units have `ingest=true`, the
extension contains only HTML/CSS/JSON/assets (rare but legal). The
collector exits 0 but `report.ingest_enabled == 0` — flag the case to the
user; ReverTS has nothing to decompile.

## Output Contract

The script writes a manifest accepted by `ingest_app_manifest`:

```json
{
  "schema_version": 1,
  "profile": "browser-extension",
  "artifact_root": "/absolute/path/to/unpacked-or-staged-extension",
  "sources": [],
  "edges": [],
  "metadata": {
    "collector": "browser-extension-collector",
    "collector_version": 1,
    "input_path": "...",
    "unresolved_manifest_references": [],
    "extension": {
      "name": "...",
      "version": "...",
      "manifest_version": 3,
      "default_locale": "en"
    },
    "container": "crx" | "xpi" | "directory"
  }
}
```

Source-unit roles include all electron-collector roles plus the
extension-specific ones listed in
[Source-unit roles](#source-unit-roles).

Read [browser-extension-artifact-model.md](references/browser-extension-artifact-model.md)
only when changing collector behavior or debugging role/path
classification.
