# Electron Artifact Model

Use this reference when modifying or debugging `bin/collect_electron_artifact`.

## Input Resolution

Resolve inputs in this order:

1. `.dmg` / `.img` file
2. `.app` bundle
3. `Contents/Resources`
4. packaged Electron root containing `resources/`
5. direct resources directory containing `app.asar`, `package.json`, or renderer assets
6. directory containing one nested `.app`

For `.dmg` / `.img`, prefer `hdiutil` on macOS because it handles signed and compressed images most reliably. On Linux/Windows, use 7-Zip and inspect nested HFS/APFS/ISO images when present.

## Manifest Principles

- Inventory all meaningful files as source units unless `--no-assets` is used.
- Enable ingest only for JS/TS-family source units with real physical paths.
- Extract ASAR code-like files into the stage directory before marking them ingest-enabled.
- Keep `artifact_root` pointed at the resolved Resources directory.
- Preserve the original input path in manifest metadata.
- Treat native `.node`, `.wasm`, `.dll`, `.dylib`, and `.so` files as inventory only.

## Role Heuristics

- `package_manifest`: `package.json`
- `html_entry`: `.html` / `.htm`
- `source_map`: `.map` and common `*.js.map` / `*.css.map`
- `native_addon`: native binary extensions
- `preload`: paths containing `preload`
- `worker`: paths containing `worker`, `worklet`, or `service-worker`
- `vendor_chunk`: paths containing `vendor` or `node_modules`
- `main`: package `main`, `main.js`, `electron.js`, or `background.js`
- `js_chunk`: remaining JS/TS-family code
- `asset`: non-code resources

Package `main` and root `index.html` style entries should set `entry=true`.

## Edges

Build lightweight edges only when both endpoints are present in the manifest:

- HTML `script src` -> JS source unit as `script`
- HTML stylesheet link -> CSS source unit as `stylesheet`
- JS/CSS `sourceMappingURL` -> map source unit as `source_map`

Do not infer edges that cannot be resolved to a manifest source unit.
