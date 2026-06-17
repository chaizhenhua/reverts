# Unpack to Validated Reverts Output Workflow

## Goal

Define the end-to-end workflow for taking an external reverse-engineering target, unpacking it, importing it into Reverts, semantically naming its public surface, externalizing third-party package internals, emitting decompiled output, and validating that the emitted result compiles and can run in the target-specific environment.

The key boundary is:

- **Skills handle external formats and environment-specific unpacking.**
- **Reverts code handles facts, graphs, classification, naming, package matching, planning, emitting, and output validation.**

## Target Pipeline

```text
Target
  ↓
auto-unpack-target Skill
  ↓
unpacked manifest + extracted root
  ↓
reverts-cli import-unpacked
  ↓
InputBundle / SQLite
  ↓
classify-modules
  ↓
match-packages
  ↓
public-surface extraction
  ↓
agent semantic naming
  ↓
public-surface naming gate
  ↓
generate-project-v2
  ↓
validate-output
```

## Skill Responsibilities

### `auto-unpack-target`

Detects the input kind and dispatches to the right unpack Skill.

Supported target classes:

- Bun single-file executable with `/$bunfs/root/` payloads.
- macOS Electron `.app` with `Contents/Resources/app.asar`.
- Electron `.dmg` containing an Electron `.app`.
- Chrome / Edge extension `.crx`.
- Browser extension `.zip` with root `manifest.json`.
- Browser extension folder with root `manifest.json`.
- Native ELF / Mach-O / PE binaries without a known package format: classify only, do not native-decompile by default.

Output:

```text
auto-unpack-report.json
```

### `unpack-bunfs`

Handles Bun standalone executables.

Responsibilities:

- Scan for `/$bunfs/root/` entries.
- Extract BunFS payloads.
- Preserve `.node`, WASM, ELF, image, and text assets byte-for-byte.
- Generate a startup runner when possible.
- Validate safe CLI commands such as `--version`, `--help`, and subcommand help.
- Validate native addon loadability.

Output:

```text
bunfs-manifest.json
bunfs-manifest.jsonl
validation.json
root/
```

### `unpack-electron-app`

Handles Electron `.app` and `.dmg` inputs.

Responsibilities:

- Extract `.dmg` on non-macOS hosts with `7z` / `7zz` when needed.
- Locate the nested `.app`.
- Extract `app.asar`.
- Merge `app.asar.unpacked` native assets into the extracted app tree.
- Preserve original `app.asar` as `app.asar.original` in the copied bundle.
- Validate bundle structure, package main, native assets, and optional macOS launch smoke.

Output:

```text
electron-app-manifest.json
electron-app-manifest.jsonl
validation.json
<Name>-unpacked.app/
```

### `unpack-browser-extension`

Handles Chromium-family browser extensions.

Responsibilities:

- Download Chrome Web Store CRX by extension id.
- Download Edge Add-ons CRX by extension id.
- Parse CRX2 / CRX3 headers.
- Extract embedded ZIP payloads.
- Extract local extension ZIPs or copy unpacked extension folders.
- Validate `manifest.json`, referenced files, locales, background scripts/service workers, content scripts, popup pages, and web-accessible resources.
- Optionally smoke-load with Chrome / Chromium / Edge via `--load-extension`.

Output:

```text
browser-extension-manifest.json
browser-extension-manifest.jsonl
validation.json
extension/
```

## Reverts Code Responsibilities

The following capabilities must be implemented in Reverts rather than Skills.

### 1. `import-unpacked`

New CLI command:

```bash
reverts-cli import-unpacked \
  --input <unpacked-root> \
  --manifest <auto-unpack-report.json> \
  --project-name <name> \
  --output-db <db>
```

Responsibilities:

- Read Skill output manifests.
- Determine target kind and extracted source root.
- Write canonical Reverts input facts into SQLite:
  - `projects`
  - `source_files`
  - `project_files`
  - `modules`
  - `symbols`
  - `module_dependencies`
  - `project_assets`
  - target metadata in `project_config` or a dedicated target metadata table.

This command creates the bridge from unpacked external artifacts into `InputBundle`.

### 2. Target Import Model

Introduce a normalized target import model, for example:

```rust
enum TargetKind {
    BunCli,
    ElectronApp,
    BrowserExtension,
    GenericJsProject,
}

struct ImportedTarget {
    kind: TargetKind,
    source_root: PathBuf,
    entrypoints: Vec<EntryPoint>,
    assets: Vec<Asset>,
    native_assets: Vec<NativeAsset>,
}
```

This model should be internal to the importer and converted into `InputRows` / SQLite rows.

### 3. Target-Aware Importers

Each target kind needs import rules.

#### Bun CLI

Import facts:

- BunFS root.
- CLI entrypoint.
- Native `.node` assets.
- WASM/assets.
- `require` / `import` graph.
- Runner metadata from the unpack Skill.

#### Electron App

Import facts:

- Electron main entry.
- Preload scripts.
- Renderer entries.
- ASAR extracted source files.
- Native addons and app resources.

#### Browser Extension

Import facts:

- `manifest.json`.
- Background service worker or background scripts.
- Content scripts.
- Popup/options/devtools pages.
- `web_accessible_resources`.
- Locales, icons, and assets.

### 4. Module Classification

New CLI command:

```bash
reverts-cli classify-modules \
  --input <db> \
  --project-id <id> \
  --apply
```

Classifications:

```text
application
package
runtime
asset
generated
unknown
```

Inputs / evidence:

- Path evidence such as `node_modules/...`.
- `package.json` ownership.
- source map original paths.
- bundle chunk names and vendor paths.
- import graph ownership.
- package matcher fingerprints.
- unpack manifest target metadata.

Third-party package modules should be classified as package candidates before package matching.

### 5. Package Matching and Third-Party Skip Rules

Existing command:

```bash
reverts-cli match-packages \
  --input <db> \
  --project-id <id> \
  --materialize-package-sources \
  --apply
```

Required behavior in the full workflow:

- Package candidate modules enter package matching.
- Accepted package modules may emit as external imports or internal-to-externalized package modules.
- Internal third-party modules must not be queued for semantic naming.
- The emitted output should expose the package public import surface rather than renamed internal package implementation details.

### 6. Public Surface Extraction

New CLI command:

```bash
reverts-cli public-surface \
  --input <db> \
  --project-id <id> \
  --list
```

Public surface is target-aware.

Common surface sources:

- Module exports.
- Reexports.
- Entry module exports.
- Cross-module exposed APIs.

Bun CLI surface sources:

- CLI command handlers.
- Public runtime functions.
- Public config/constants.

Electron surface sources:

- Main/preload IPC handlers.
- Exposed bridge APIs.
- Renderer entry interfaces.

Browser extension surface sources:

- Manifest-declared background service worker/scripts.
- Content scripts.
- Popup/options/devtools pages.
- Message handlers.
- Web-accessible public resources.

### 7. Public Surface Semantic Naming Gate

New CLI command:

```bash
reverts-cli public-surface-names \
  --input <db> \
  --project-id <id> \
  --list-missing

reverts-cli public-surface-names \
  --input <db> \
  --project-id <id> \
  --require-complete
```

Gate rule:

```text
Every non-package public surface symbol must have semantic_name.
Package-internal third-party symbols are skipped.
```

The command should report missing names with enough evidence for agent work:

```text
module_id
original_name
surface_role
source_path
definition span
reference count
export/import evidence
```

### 8. Agent Naming Context

New CLI command:

```bash
reverts-cli symbol-context \
  --input <db> \
  --project-id <id> \
  --module-id <module-id> \
  --symbol <original-name> \
  --format json
```

The JSON should include:

```json
{
  "symbol": "a",
  "module_source": "...",
  "definition_span": {},
  "references": [],
  "exports": [],
  "imports": [],
  "neighbor_modules": [],
  "target_kind": "browser_extension",
  "public_surface_role": "background_message_handler"
}
```

Agent writes decisions through existing naming persistence:

```bash
reverts-cli symbol-names \
  --input <db> \
  --project-id <id> \
  --batch names.tsv \
  --apply
```

### 9. Naming Apply Safety

Extend `symbol-names` or add companion validation for:

- `--only-public-surface`.
- `--skip-package-internal`.
- collision report.
- reserved word checks.
- export alias safety checks.
- batch dry-run summary.

### 10. Emit Gate

`generate-project-v2` should either gain strict flags or be preceded by `validate-input`.

Possible flags:

```bash
reverts-cli generate-project-v2 \
  --input <db> \
  --project-id <id> \
  --output <dir> \
  --require-public-surface-names \
  --fail-on-audit-warning
```

Alternative command:

```bash
reverts-cli validate-input \
  --input <db> \
  --project-id <id> \
  --require-public-surface-names
```

### 11. Target-Specific Output Validation

New CLI command:

```bash
reverts-cli validate-output \
  --target-kind auto|bun-cli|electron-app|browser-extension \
  --original <target> \
  --output <emitted-output>
```

#### Bun CLI Validation

- Install/build emitted output.
- Run safe commands:
  - `--version`
  - `--help`
  - selected subcommand `--help`.
- Compare outputs against original where possible.

#### Browser Extension Validation

- Reconstruct extension layout if needed.
- Validate `manifest.json`.
- Smoke-load with Chrome / Chromium / Edge via `--load-extension`.
- Optionally run browser automation smoke tests.

#### Electron App Validation

- Reconstruct app layout.
- Validate package main and native assets.
- On macOS:
  - ad-hoc sign.
  - launch smoke.
- On non-macOS:
  - structural validation only.

## Recommended Crate Layout

### `reverts-import`

Responsibilities:

- Read unpack manifests.
- Normalize target import metadata.
- Produce `InputRows`.
- Persist import rows into SQLite.

Allowed dependency direction:

```text
reverts-import -> reverts-input, reverts-model, reverts-js, reverts-observe
```

### `reverts-surface`

Responsibilities:

- Public surface extraction.
- Surface role classification.
- Naming coverage gate.
- Symbol context extraction for agent naming.

Allowed dependency direction:

```text
reverts-surface -> reverts-input, reverts-graph, reverts-model
```

### `reverts-validate`

Responsibilities:

- Output validation.
- Target-specific compile/run/load smoke tests.
- Validation reports.

Allowed dependency direction:

```text
reverts-validate -> reverts-observe
```

Core tests should remain self-contained. Tests that require Node, Chrome, npm, network, or macOS launch should be opt-in integration tests, not default unit tests.

## Proposed End-User Command

Long-term command:

```bash
reverts-cli decompile \
  --target <target> \
  --output <out> \
  --require-public-surface-names \
  --validate-run
```

Expanded workflow:

```bash
auto_unpack.py <target> --out work/unpacked

reverts-cli import-unpacked \
  --manifest work/unpacked/auto-unpack-report.json \
  --project-name <name> \
  --output-db work/reverts.db

reverts-cli classify-modules \
  --input work/reverts.db \
  --project-id <id> \
  --apply

reverts-cli match-packages \
  --input work/reverts.db \
  --project-id <id> \
  --materialize-package-sources \
  --apply

reverts-cli public-surface-names \
  --input work/reverts.db \
  --project-id <id> \
  --list-missing

# Agent produces names.tsv from symbol-context tasks.

reverts-cli symbol-names \
  --input work/reverts.db \
  --project-id <id> \
  --batch names.tsv \
  --apply

reverts-cli public-surface-names \
  --input work/reverts.db \
  --project-id <id> \
  --require-complete

reverts-cli generate-project-v2 \
  --input work/reverts.db \
  --project-id <id> \
  --output <out>

reverts-cli validate-output \
  --target-kind auto \
  --original <target> \
  --output <out>
```

## Implementation Priority

### P0: Close the Basic Loop

1. `import-unpacked`.
2. Browser extension importer.
3. `public-surface --list`.
4. `public-surface-names --require-complete`.
5. `validate-output --target-kind browser-extension`.

Browser extensions are the best first target because validation is deterministic with Chromium `--load-extension` and does not require macOS.

### P1: Third-Party Package Handling

1. `classify-modules`.
2. Automatic package candidate marking.
3. `match-packages` orchestration.
4. Package-internal skip rules for naming.
5. Public package surface externalization.

### P2: Agent Naming Experience

1. `symbol-context`.
2. Naming task export.
3. Batch accept workflow.
4. Collision / reserved-word / export-safety gate.

### P3: Bun and Electron Runtime Validation

1. Bun CLI emitted runner validation.
2. Electron emitted app packaging.
3. macOS ad-hoc signing and launch smoke.

## Skill vs Reverts Boundary Summary

| Capability | Skill | Reverts code |
| --- | --- | --- |
| Identify target type | Yes | Record result |
| Unpack CRX / DMG / ASAR / BunFS | Yes | No |
| Download browser extension | Yes | No |
| Generate unpack manifest | Yes | Read it |
| Import into SQLite | No | Yes |
| AST / module / symbol analysis | No | Yes |
| Third-party package classification | Evidence only | Yes |
| Package matching | No | Yes |
| Public surface extraction | No | Yes |
| Semantic naming persistence | No | Yes |
| Agent naming context | No | Yes |
| Emit | No | Yes |
| TypeScript compile validation | Auxiliary | Yes |
| Original unpack smoke validation | Yes | No |
| Emitted output smoke validation | Auxiliary | Yes |

## Current Validation Findings

The current codebase already has working local pieces:

- `symbol-names` persistence and batch flow.
- package matching and package surface logic.
- `generate-project-v2` emit.
- TypeScript compile validation for small emitted projects.

Observed gaps from validation:

- No formal `import-unpacked` command exists yet.
- `symbol-names --list` exposes module/global symbols, but this is not the same as target-aware public surface.
- Some imported browser extension projects have many unnamed module/global symbols.
- Existing package matching does not run unless modules have package evidence/classification.
- Emitted browser-environment code may compile but fail under plain Node because `window` / `self` are expected; output validation must be target-specific.
- Medium-size projects can expose emit performance issues and need profiling/gates.
