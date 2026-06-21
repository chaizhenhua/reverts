# Browser Extension Artifact Model

Use this reference when modifying or debugging
`bin/collect_browser_extension_artifact`.

## Input Resolution

Resolved in this order (first match wins):

1. `.crx` file (suffix + `Cr24` magic at byte 0)
2. `.xpi` file (suffix; structure validated as ZIP)
3. unpacked directory containing `manifest.json` at root
4. directory whose only immediate subdirectory contains `manifest.json`
   (typical when the user passes a Chrome user-profile extension dir like
   `~/.config/google-chrome/Default/Extensions/<id>/` — the version
   subdirectory is descended into automatically)

For `.crx`, both CRX2 and CRX3 magic versions are supported. Refer to
the Chromium source for header layout:

- CRX2 (deprecated since Chrome 75 but still in the wild):
  `Cr24` + uint32 version=2 + uint32 pubkey_length + uint32 sig_length +
  pubkey + signature + ZIP body.
- CRX3:
  `Cr24` + uint32 version=3 + uint32 header_length + protobuf header
  (CrxFileHeader) + ZIP body.

The collector strips the header and treats the trailing bytes as a
standard ZIP archive.

## Manifest Principles

- One source unit per file. The `manifest.json` itself is a source unit
  with role `manifest_v2` or `manifest_v3`.
- Inventory all meaningful files unless `--no-assets` is used.
- Enable `ingest` only for JS/TS-family source units (parity with
  electron-collector).
- Keep `artifact_root` pointed at the resolved unpacked extension root
  (the directory containing `manifest.json`).
- Preserve the original input path in manifest metadata.
- Treat `.wasm` / `.so` / `.dylib` / `.dll` / `.node` files as
  inventory-only (`role=native_addon`).
- Locale messages (`_locales/<lang>/messages.json`) are inventory-only;
  they don't go through the JS decompile pipeline.

## Role Heuristics

Role is assigned by combining manifest references with path patterns.
**Manifest references take precedence over path heuristics** — e.g. a
file named `background.js` is `js_chunk` by default, but if it's listed
in `background.scripts`, it becomes `background_script`.

### Manifest-driven roles

```
manifest_version: 3 → manifest.json gets role manifest_v3
manifest_version: 2 → manifest.json gets role manifest_v2
background.service_worker         → role service_worker
background.scripts[*]             → role background_script
background.page                   → role background_page
content_scripts[*].js[*]          → role content_script
content_scripts[*].css[*]         → role content_style
action.default_popup              → role popup_html
browser_action.default_popup      → role popup_html  (MV2)
page_action.default_popup         → role popup_html  (MV2)
options_page                      → role options_html
options_ui.page                   → role options_html
devtools_page                     → role devtools_html
sidebar_action.default_panel      → role sidebar_html  (Firefox)
chrome_url_overrides.{newtab|bookmarks|history} → role override_html
web_accessible_resources[*].resources[*]        → role accessible_chunk
                                  (or web_accessible_resources[*] string in MV2)
```

### Path-based fallback

Used only if no manifest reference applies:

```
_locales/<lang>/messages.json     → role locale_message
*offscreen*.html                  → role offscreen_html  (MV3 offscreen)
*.js, *.mjs                       → role js_chunk
*.html, *.htm                     → role html_entry
*.css                             → role content_style? no — only when
                                    referenced by content_scripts; otherwise
                                    role asset (browsers don't auto-load CSS)
*.json                            → role asset
*.{png,jpg,svg,...}              → role asset
*.{wasm,so,dylib,dll,node}       → role native_addon
*.{js,mjs,css}.map               → role source_map
```

Source-unit `entry: true` is set for the manifest, the service worker,
each background_script, each background_page, popup_html, options_html,
devtools_html, and sidebar_html. Content scripts are not entry points
(they're injected at navigation time).

## Edges

Edges record cross-file declarations and references found during
collection:

| Edge `kind` | From → To | Source |
|---|---|---|
| `manifest_declares` | manifest → service_worker | `background.service_worker` field |
| `manifest_declares` | manifest → background_script | each `background.scripts[*]` entry |
| `manifest_declares` | manifest → background_page | `background.page` field |
| `manifest_declares` | manifest → content_script | each `content_scripts[*].js[*]` |
| `manifest_declares` | manifest → content_style | each `content_scripts[*].css[*]` |
| `manifest_declares` | manifest → popup_html / options_html / devtools_html / sidebar_html / override_html | corresponding manifest field |
| `manifest_declares` | manifest → accessible_chunk | each `web_accessible_resources[*]` entry |
| `script` | html_entry / popup_html / options_html / devtools_html → js_chunk | parsed `<script src=...>` |
| `stylesheet` | html_entry / popup_html / ... → asset (.css) | parsed `<link rel=stylesheet>` |
| `source_map` | js_chunk → source_map | trailing `//# sourceMappingURL=` |

Edges are only emitted for resolved source-unit pairs. Manifest
references that do not resolve to a packaged file are recorded in
manifest `metadata.unresolved_manifest_references`; they are not
represented as `artifact_edges` because `artifact_edges.to_unit` is a
non-null source-unit id.

## Metadata schema

The manifest's `metadata` object always includes:

```json
{
  "collector": "browser-extension-collector",
  "collector_version": 1,
  "input_path": "/abs/path/to/original/input",
  "resolved_artifact_root": "/abs/path/to/unpacked/root",
  "container": "crx" | "xpi" | "directory",
  "unresolved_manifest_references": [],
  "extension": {
    "name": "<from manifest.json or __MSG_*__ token>",
    "version": "<from manifest.json>",
    "manifest_version": 2 | 3,
    "default_locale": "<from manifest.json or null>",
    "minimum_chrome_version": "<from manifest.json or null>"
  },
  "crx": {                // present only when container == "crx"
    "version": 2 | 3,
    "header_size": <bytes>,
    "pubkey_size": <bytes for CRX2 / null for CRX3>,
    "signature_size": <bytes for CRX2 / null for CRX3>
  }
}
```

The `extension.name` field may be a localization token like `__MSG_extName__`
when the manifest defers to `_locales`; the collector does not resolve
the locale because role assignment doesn't depend on the resolved name.
Downstream tools should prefer `metadata.extension.version` for version
identification.
