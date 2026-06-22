---
name: website-collector
description: Collect a website frontend (live URL, HAR capture, or downloaded asset directory) into a ReverTS web-app artifact manifest, then ingest and decompile the minified SPA bundle back into a runnable TypeScript project.
---

# Website Collector

Use this skill to turn a website's shipped frontend — the HTML entry plus its
minified JavaScript chunks, CSS, source maps, and WASM — into the standard
ReverTS app artifact manifest, import it with the `reverts-cli` binary, and hand
off to the decompile + web-app runtime validation skills. Output schema is
identical to
[electron-collector](../electron-collector/SKILL.md) and
[browser-extension-collector](../browser-extension-collector/SKILL.md); only the
`profile` (`web-app`) and the role namespace differ.

The hard input is a **single-page app** (React/Vue/Svelte/etc.) served as
minified webpack/esbuild/Vite/Rollup chunks with no source maps. The collector
captures the full asset set; the ReverTS pipeline extracts the bundled modules
and reconstructs readable TypeScript.

## Agent Boundary

This skill is for artifact collection and recovery orchestration. Do not use an
Agent to hand-edit generated files, patch imports, rewrite `package.json`, or
de-minify chunks by hand. Mechanical recovery is the ReverTS pipeline's job and
its defects must be filed as ReverTS work items with regression tests. The
Agent's role is semantic renaming after mechanical recovery has produced a
rename worklist, plus capture orchestration (driving a browser to record a HAR).

## Capturing a real SPA

Static HTML lists only the chunks referenced at first paint. Production SPAs
load most code through dynamic `import()` calls the bundler runtime issues at
run time, which a pure-HTML crawl never sees. Capture completeness, in order of
preference:

1. **HAR capture (recommended).** Drive the site in a real browser (Playwright
   MCP `browser_navigate`, then exercise the routes/controls so lazy chunks
   load), then save a HAR. Pass it with `--from-har`. Every JS/CSS/HTML/WASM/map
   response body is materialised, including dynamically-imported chunks.
2. **URL + url-list.** Navigate with Playwright, read the full request set from
   `browser_network_requests`, write the script/style URLs to a file, and pass
   `--url-list`. The collector fetches the entry HTML plus every listed URL.
3. **URL only.** `--from` a URL with no list: entry HTML + its `<script src>`,
   `<link rel=stylesheet>`, `<link rel=modulepreload>`, and each
   `sourceMappingURL`. Use only when you know the app is not heavily code-split.
4. **Directory.** A directory you already downloaded (e.g. via the browser's
   "Save page" / a site mirror). Inventoried in place.

### Input dispatch

The collector resolves the input against this ordered list (first match wins).

| Input shape | Selection | Capture path | Notes |
|---|---|---|---|
| HAR file | `--from-har FILE` | parse `log.entries`, write each code-like response body to stage | best for SPAs; captures dynamic chunks; refetches bodies HAR omitted unless `--no-refetch-missing` |
| `http(s)://…` URL | positional arg starts with `http` | fetch entry HTML, follow `<script>`/`<link>`/modulepreload, then `--url-list` URLs, then `sourceMappingURL` | add `--url-list` for code-split apps |
| Existing directory | positional arg is a directory | inventory files in place; entry = `index.html` or shallowest HTML | fully offline |

Rejected inputs (collector exits non-zero, never silently degrades): a path that
is neither an http(s) URL, an existing directory, nor a `--from-har` file; an
entry URL that does not return HTML; a HAR with no `log.entries`.

## Source-unit roles

Roles assigned during collection (web-app namespace, plus the shared
`source_map` / `asset` roles):

| Role | Heuristic |
|---|---|
| `html_entry` | `.html` / `.htm` |
| `entry_chunk` | JS named `main*` / `index*` / `app*` / `entry*` / `bundle*` |
| `runtime_chunk` | JS named `runtime*` or `webpack*` (bundler runtime/loader) |
| `vendor_chunk` | JS matching `vendor` / `chunk-vendors` / `framework` / `polyfill` / `commons` or under `node_modules/` |
| `js_chunk` | any other JS/TS-family file |
| `service_worker` | `sw.js` / `service-worker.js` / path contains `service-worker` |
| `worker` | filename contains `worker` or path contains `worklet` |
| `stylesheet` | `.css` |
| `wasm_module` | `.wasm` |
| `web_manifest` | `manifest.json` / `*.webmanifest` |
| `data_json` | other `.json` |
| `source_map` | `.map` / `.js.map` / `.css.map` |
| `asset` | everything else (images, fonts) — only kept with `--include`/default, never ingested |

Only the JS/TS family (`javascript`, `typescript`, `jsx`, `tsx`) is
ingest-eligible. See
[web-app-artifact-model.md](references/web-app-artifact-model.md) for the full
path → language → role resolution and the URL → logical-path mapping rules.

## Install

Bundled with the `reverts` distribution. See
[skills/README.md](../README.md#install) for the full install matrix
(`npm install -g reverts`, local-dev `./skills/install`). The release must
contain the `reverts-cli` binary, the `skills/` directory (including
`website-collector` and its `bin/` collector script), and the npm launcher.

The pipeline mechanism is the `reverts-cli` binary — build it with
`cargo build --release --bin reverts-cli` and make sure it is on `PATH`
(or invoke it by its built path, e.g. `./target/release/reverts-cli`).

The collector script is cross-platform Python 3 with no third-party dependency;
HTTP fetching uses the standard library. HAR/directory modes need no network.

## Collector Command

Run the bundled script from the repository root. HAR capture (recommended for
SPAs):

```bash
python3 skills/website-collector/bin/collect_website_artifact \
  --from-har /tmp/site-capture.har \
  --output-manifest /tmp/website-artifact-manifest.json \
  --stage-dir /tmp/website-collector-stage \
  --json-report
```

Live URL (add `--url-list` for code-split apps):

```bash
python3 skills/website-collector/bin/collect_website_artifact \
  https://app.example.com/ \
  --url-list /tmp/network-requests.txt \
  --output-manifest /tmp/website-artifact-manifest.json \
  --stage-dir /tmp/website-collector-stage \
  --json-report
```

`--ingest-include` / `--ingest-exclude` bound a validation run to specific JS
logical paths while keeping full inventory semantics; they are smoke-validation
aids, not degradation switches. The manifest always records all source units and
edges.

## ReverTS Workflow

1. Resolve or create a ReverTS project for the captured app.
2. Run the collector and inspect the JSON report.
3. Call `ingest_app_manifest(project_id, manifest_path, run_discovery=true)` so
   source-unit registration and module discovery run. For a single big bundle,
   discovery is where bundled-module extraction is driven; expect minutes on
   large chunks and use a generous MCP client timeout.
4. Validate inventory with `list_app_artifacts` and `get_artifact_manifest`.
5. Run [decompile](../decompile/SKILL.md) until its public-surface gate passes
   and `generate_app_decompiled_files` succeeds with strict gates enabled.
6. Run [reverts-decompile](../reverts-decompile/SKILL.md) with the **web-app**
   runtime profile: install, `tsc --noEmit`, serve the recovered app, then
   Playwright-load the served URL and assert the root UI renders, routes/controls
   work, and the browser console is error-free.

Do not use ingest filters as a degradation mechanism for full recovery. Full
recovery should inventory and ingest every JS/TS-family source unit the manifest
marks recoverable.

## Completion Criteria

A collection run is successful only when **all** hold:

1. The collector exits 0 and the JSON report has `status: "ok"`.
2. The manifest has `schema_version == 1`, `profile == "web-app"`, and a
   non-empty `artifact_root`.
3. `sources` is non-empty and contains at least one `entry_chunk`, `js_chunk`,
   or `vendor_chunk` source unit (there is JavaScript to decompile).
4. There is at least one `html_entry` source unit marked `entry: true`, or the
   capture recorded an `entry_url` in `metadata`.
5. Every `ingest == true` source unit has a real physical path under the stage
   directory or inside `artifact_root` (no dangling refs).
6. After `ingest_app_manifest(... run_discovery=true)`, `list_app_artifacts`
   returns the same source-unit count as the manifest. A mismatch is an
   ingestion bug — file it as a ReverTS issue, do not retry blindly.
7. `decompile_status(project_id)` can see discovered modules. If discovery
   produced zero modules from a single large bundle, the bundle-extraction stage
   failed — fix the pipeline mechanism and rerun; do not mutate the manifest.
8. Web-app post-export validation completes through
   [reverts-decompile](../reverts-decompile/SKILL.md): dependency install
   succeeds, real `tsc` runs, the app serves, and Playwright confirms a
   non-empty root UI with a clean console (or each error is triaged as a ReverTS
   work item).

Stop and report failure if any fails. Do not patch the manifest or hand-edit the
stage directory to make a bad run look good.

## Failure Recovery

| Failure | Signal | Action |
|---|---|---|
| Entry URL returns non-HTML | `Entry URL did not return HTML` | you passed an asset URL, not the page; pass the document URL |
| SPA missing most code | report shows only a handful of chunks; recovered app is mostly blank | the crawl missed dynamic chunks; recapture with `--from-har` or `--url-list` from `browser_network_requests` |
| No ingest-enabled units | `report.ingest_enabled == 0` | page is HTML/CSS only (server-rendered, no client bundle); nothing to decompile — flag to the user |
| HAR omits response bodies | notes list `no response body for …` | re-record the HAR with content capture enabled, or allow `--no-refetch-missing` off so the collector refetches |
| Network fetch fails | `Network error fetching …` / `HTTP 4xx/5xx` | the asset is gone or auth-gated; capture via an authenticated browser session and use HAR/directory mode |
| Cross-origin CDN chunks | units appear under `_cross/<host>/…` | expected; CDN-hosted chunks are namespaced by host so they never collide with first-party paths |
| `ingest_app_manifest` rejects manifest | MCP schema validation error | fix the collector or ReverTS ingest with a regression test; do NOT mutate JSON to satisfy the validator |
| Discovery extracts 0 modules from the bundle | `decompile_status` shows no modules though a large `entry_chunk` ingested | reproduce against the bundle, add a ReverTS bundle-extraction regression test, fix the extractor, re-ingest |
| Source-unit count mismatch after ingest | `list_app_artifacts.count != manifest.sources.length` | stop; file as a ReverTS bug. Re-ingesting masks the defect |

If a site cannot be fetched on the current host (auth wall, geofence, bot
challenge), stop and report it. Do not fabricate a manifest for assets you could
not capture.

## Tool Summary

| Step | Tool |
|---|---|
| Drive browser + record HAR / list requests | Playwright MCP `browser_navigate`, `browser_network_requests`, plus the host's HAR export |
| Collect HAR / URL / directory | `python3 skills/website-collector/bin/collect_website_artifact … --json-report` |
| Project + ingest | ReverTS MCP `create_project`, `ingest_app_manifest`, `list_app_artifacts`, `get_artifact_manifest` |
| Semantic naming + output | [decompile](../decompile/SKILL.md): `decompile_status`, `query`, `submit_module_decompilation`, `update_modules`, `generate_app_decompiled_files` |
| Install / compile / serve / UI | [reverts-decompile](../reverts-decompile/SKILL.md), web-app profile in `references/runtime-validation-profiles.md` |

## Output Contract

The script writes a manifest accepted by `ingest_app_manifest`:

```json
{
  "schema_version": 1,
  "profile": "web-app",
  "artifact_root": "/absolute/path/to/staged-or-downloaded-site",
  "sources": [],
  "edges": [],
  "metadata": {
    "collector": "website-collector",
    "collector_version": 1,
    "capture_mode": "har | url | directory",
    "entry_url": "https://app.example.com/",
    "origin": "https://app.example.com",
    "notes": []
  }
}
```

Each source unit carries `id`, `logical_path`, `physical_path`, `role`,
`language`, `ingest`, `entry`, `container`, and `metadata` (including
`source_url` for URL/HAR captures, used to resolve `script` / `stylesheet` /
`source_map` edges). Read
[web-app-artifact-model.md](references/web-app-artifact-model.md) only when
changing collector behavior or debugging role/path classification.
