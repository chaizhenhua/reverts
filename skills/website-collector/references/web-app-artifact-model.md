# Web-App Artifact Model

Load this reference only when changing `collect_website_artifact` behavior or
debugging role / language / path classification. The main
[SKILL.md](../SKILL.md) owns the workflow.

## Manifest shape

Identical to the electron and browser-extension collectors. Top level:

| Field | Value |
|---|---|
| `schema_version` | `1` |
| `profile` | `"web-app"` |
| `artifact_root` | absolute path to the staged capture (`<stage>/har`, `<stage>/url`) or the input directory |
| `sources` | array of source units |
| `edges` | array of `{from, to, kind, metadata}` reference edges |
| `metadata` | collector provenance (mode, entry URL, origin, notes, `collector_roles`) |

Each source unit:

```json
{
  "id": "assets_index_xyz_js_3f2a9c1b04",
  "logical_path": "assets/index-xyz.js",
  "physical_path": "/abs/stage/url/assets/index-xyz.js",
  "role": "entry_chunk",
  "language": "javascript",
  "ingest": true,
  "entry": false,
  "container": null,
  "metadata": { "file_size": 18342, "source_url": "https://app.example.com/assets/index-xyz.js" }
}
```

`id` is a stable slug derived from the logical path (`sha1[:10]` suffix), so the
same capture yields the same ids across runs. `source_map_id` is added to a
source unit when its `sourceMappingURL` resolved to a captured `.map` unit.

## Capture modes

| Mode | Trigger | Root | Completeness |
|---|---|---|---|
| `directory` | positional arg is a directory | the directory | only what is on disk |
| `url` | positional arg starts with `http` | `<stage>/url` | entry HTML + referenced scripts/styles/modulepreload + `--url-list` + source maps |
| `har` | `--from-har FILE` | `<stage>/har` | every code-like response body in the HAR (dynamic chunks included) |

HAR mode only materialises responses whose status is 2xx (or unset) and whose
MIME type / URL extension is code-like (`html`, `css`, `javascript`, `json`,
`wasm`, source map). Images and fonts are skipped unless assets are kept. When a
HAR entry has no captured body, the collector refetches the URL over the network
unless `--no-refetch-missing` is set, recording a note either way.

## Language resolution

Path extension first, then HAR/HTTP MIME type as a fallback for extensionless
URLs (`/assets/chunk?v=2`):

| Language | Extensions | MIME fallback |
|---|---|---|
| `source_map` | `.map`, `.js.map`, `.css.map` | — |
| `javascript` | `.js`, `.mjs`, `.cjs` | `application/javascript`, `text/javascript` |
| `typescript` | `.ts`, `.mts`, `.cts` | — |
| `jsx` / `tsx` | `.jsx` / `.tsx` | — |
| `html` | `.html`, `.htm` | `text/html`, `application/xhtml+xml` |
| `css` | `.css` | `text/css` |
| `json` | `.json`, `.webmanifest` | `application/json`, `application/manifest+json` |
| `native` | `.wasm` | `application/wasm` |
| `asset` | anything else | — |

Only `javascript` / `typescript` / `jsx` / `tsx` are ingest-eligible. An
extensionless response gets an extension appended from its sniffed language
(`MIME_EXT`) so downstream tooling and the role heuristics classify it.

## Role heuristics

Applied to the logical path (see the table in [SKILL.md](../SKILL.md#source-unit-roles)).
Order matters; first match wins:

1. `source_map` — source-map suffix.
2. `wasm_module` — `.wasm`.
3. `html_entry` — HTML suffix.
4. `service_worker` — `sw.js` / `service-worker.js` / path contains `service-worker`.
5. `worker` — filename contains `worker`, or path contains `worklet`.
6. `stylesheet` — `.css`.
7. `web_manifest` — `manifest.json` / `*.webmanifest`.
8. `data_json` — other `.json`.
9. JS-family chunk classification:
   - `runtime_chunk` — name matches `runtime` boundary or starts with `webpack`.
   - `vendor_chunk` — matches `vendor` / `chunk-vendors` / `framework` /
     `polyfill` / `commons`, or path under `node_modules/`.
   - `entry_chunk` — name starts with `main` / `index` / `app` / `entry` /
     `bundle` followed by a separator.
   - `js_chunk` — any other JS/TS file.
10. `asset` — fallthrough.

The full collector-emitted role per logical path is also preserved verbatim
under `metadata.collector_roles`, a schema-v1 backstop so a role survives even if
`SourceRole::from_str()` collapses an unknown value to `Unknown` server-side.

## URL → logical path mapping

`url_to_logical(url, entry_origin, language)`:

1. Path segments are sanitised to `[A-Za-z0-9._-]`; a trailing `/` becomes
   `/index.html`.
2. A query string is folded into the last segment as `_q<sha1[:8]>` so
   `chunk.js?v=1` and `chunk.js?v=2` stay distinct source units.
3. Same-origin assets keep their URL path as the logical path. Cross-origin
   assets are namespaced under `_cross/<host>/…` so CDN chunks never collide
   with first-party paths.
4. Extensionless responses get an extension from the sniffed language.

This mapping is the bridge that lets `build_edges` resolve a `<script src>` or
`sourceMappingURL` (which may be root-relative or absolute) back to the captured
unit: each unit records its absolute `source_url`, references are resolved with
`urljoin(source_url, ref)`, and the result is matched against the `source_url`
index or re-mapped through `url_to_logical`.

## Edges

| Kind | From → To | Source |
|---|---|---|
| `script` | HTML → JS | `<script src>`, `<script type=module src>`, `<link rel=modulepreload>` |
| `stylesheet` | HTML → CSS | `<link rel=stylesheet>` |
| `source_map` | JS/CSS → `.map` | trailing `sourceMappingURL=` comment |

Edges are only emitted when both endpoints exist as captured source units;
unresolved references are dropped (not fabricated). Directory mode resolves refs
path-relatively; URL/HAR modes resolve through the recorded `source_url`.

## What the collector does not do

- It does not execute JavaScript. Dynamic chunks are captured by recording a HAR
  or a request list from a real browser, not by the collector.
- It does not de-minify, rename, or extract bundled modules. Bundle-module
  extraction happens in the ReverTS discovery/match stage after ingest.
- It does not fabricate files for unresolved references or omitted HAR bodies; it
  records a note and moves on.
