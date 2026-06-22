# Runtime Validation Profiles

This reference defines which validation tools to use after ReverTS output is
generated. The main `reverts-decompile` skill owns the loop; this file is loaded
when selecting source/profile-specific runtime tests or debugging a smoke
failure.

## Required validation tools

Manual inspection is not a substitute for a scripted pass/fail signal.

| Step | Required tool | Contract |
|---|---|---|
| Project metadata, decompile status, coverage, regeneration | `reverts-cli` (`naming-progress --input <db> --project-id <id> [--json]` and `coverage-ledger --input <db> --project-id <id>` for status/coverage; `generate-project-v2 --input <db> --project-id <id> --output <dir> --source-root src` to regenerate after a pipeline fix; `runtime-inventory --input <db> --project-id <id>` for emitted runtime helpers) <!-- TODO(reverts-cli): list_app_artifacts / get_artifact_manifest / query / update_modules have no direct CLI equivalent --> | Use DB/AST-backed data. Do not infer runtime profile from filenames alone when artifact metadata exists. |
| Dependency install | Shell command in `output_dir` (`pnpm install`; `npm install` only when selected by package-manager evidence or pnpm is unavailable) | Capture stdout/stderr and installed package evidence for triage. |
| Compile/edit validation | Shell command in `output_dir` (`pnpm exec tsc --noEmit -p tsconfig.json`, or `npm exec tsc -- --noEmit -p tsconfig.json`) plus ReverTS/AST structural audits when available | Real type checking only; no `--noCheck`, no hand-editing generated files as the final fix. |
| Browser extension UI/runtime | Playwright MCP browser tools. Codex uses `mcp__playwright__`; Claude/Ant uses the configured Playwright MCP equivalent. | Launch Chromium with the generated extension, collect `console.error`/`pageerror`/CDP runtime errors, and perform UI interactions. A shell Playwright script is allowed only when MCP is unavailable/denied, and must be reported as a fallback. |
| Web app UI/runtime | Playwright MCP browser tools plus the package's start/preview/static-server shell command | Open the served URL, interact with visible controls/routes, and fail on browser errors. |
| Electron runtime | Shell launch (`pnpm exec electron .` or package start script) plus Playwright/CDP attachment for renderer checks when possible | Assert main process readiness, renderer DOM readiness, zero uncaught errors, clean exit. |
| CLI / Node / library runtime | Shell `node`/`tsx`/package-bin commands | Run import/load smoke and at least one advertised command path where applicable. |
| Pipeline repair after failed validation | Repository edit tool (`apply_patch` or host-equivalent structured file edit) plus `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` | Add/update a regression test first, fix ReverTS, regenerate output, rerun the selected validation tools. Do not ship generated-output edits as the fix. |

## Source/profile test selection

Choose runtime tests from the decompiled source's recovered profile, then run all
applicable profiles.

1. Prefer ReverTS artifact metadata: `profile` and source-unit roles from
   recovered artifact metadata
   <!-- TODO(reverts-cli): list_app_artifacts / get_artifact_manifest have no direct CLI equivalent -->.
2. If metadata is absent, inspect generated manifests with structured JSON reads:
   `manifest.json` means `browser-extension`; `package.json` with `electron`,
   `main` + `preload`, or Electron scripts means `electron`; `bin`/shebang means
   `cli-node`; HTML entry plus web scripts means `web-app`; package-only
   `main`/`exports` with no UI means `node-library`.
3. If multiple source-unit roles or entrypoints exist, run every matching smoke
   test. Example: a browser extension with options, popup, content scripts, and
   service worker gets extension-load, service-worker, and UI interaction checks.
4. If no profile can be recovered, default to install + compile/edit validation
   + `node-library` import smoke, and report the missing profile evidence as a
   ReverTS metadata gap.

| Source/profile | Required runtime test |
|---|---|
| `browser-extension` or generated `manifest.json` with extension keys | [browser-extension](#browser-extension) via Playwright MCP |
| `electron` or Electron package evidence | [electron](#electron) |
| Web app / HTML app / Vite/Webpack dev server | [web-app](#web-app) via Playwright MCP |
| CLI or Node executable bundle | [CLI / Node bundle](#cli--node-bundle) |
| Node library/package export only | [node-library](#node-library) import/load smoke |

## browser-extension

1. Launch Chromium with `--load-extension=<output-dir> --headless=new`
   (Playwright's bundled Chromium is fine; Chrome stable also works). Use the
   `chrome.developerPrivate.getExtensionsInfo` extension API or CDP
   `Target.getTargets` to enumerate the loaded extension.
2. Assert on the extension record: `state == "ENABLED"`, `manifestErrors == []`,
   `runtimeErrors == []`.
3. Service worker (MV3) or background page (MV2): attach via CDP, confirm it
   reached activated/page-loaded state, and assert zero error-level console
   entries.
4. UI surfaces declared in `manifest.json`: open popup/options/devtools/sidebar
   pages as tabs, wait for `DOMContentLoaded`, assert non-empty DOM and at least
   one expected selector hit.
5. Exercise visible UI: primary popup actions, dashboard/options buttons, tabs,
   toggles, one reversible setting, filter-list add/validate using a harmless
   rule such as `example.com##.ad-banner` when supported, and matched-rules /
   strict-block / diagnostics pages when present.
6. Aggregate `console.error`, `pageerror`, failed dynamic import, service worker
   exception, and uncaught promise rejection from every context. Any count above
   zero fails the smoke run.

## electron

1. Launch with `electron .` or the recovered start script, headless if supported
   or under a virtual display.
2. Assert the main process reaches readiness within a fixed timeout by reading a
   real sentinel or attaching the inspector and watching for the main window
   execution context.
3. Assert no `uncaughtException` or `unhandledRejection` fires.
4. Renderer UI: attach via CDP/Playwright when possible, assert
   `document.readyState == "complete"`, collect console logs, and fail on errors.
5. Exit with `app.quit()` or the documented shutdown path and require exit code 0.

### Real-Electron GUI verification: drop-in into the ORIGINAL `.app`

A packaged desktop app has multiple main-process entrypoints: the main
bundle PLUS per-window **preload** scripts (e.g. `.vite/build/<window>.js`). The
recovered project is a TypeScript module tree, not a runnable Electron app
layout, and a generic `electron .` over it does NOT exercise the real GUI for two
environment reasons — neither a main-bundle recovery defect:

- the original preload scripts are built for the app's CUSTOM Electron binary
  (its fuses) and asar layout; a generic `npm` `electron` binary cannot load them
  (`Unable to load preload script`), so the renderer never receives its
  context-bridge API and the renderer paints nothing (a blank window in the app's
  background color), even with `--no-sandbox` and the real preload file present;
- `app.getAppPath()`-relative preload paths and the `.vite/build` + `.vite/renderer`
  split are the original run layout, which the recovered tree does not reproduce.

So the faithful GUI smoke is a **drop-in**: take the ORIGINAL unmodified `.app`
(its custom binary, fuses, asar, preload, renderer, native) and replace ONLY the
original main bundle (the asar's main entry, e.g. an electron-vite
`.vite/build/index.js`) with the recovered main, then launch the original `.app`.
This isolates the recovered main while every preload/renderer/env stays original
(so preloads load).

**This procedure has been validated end-to-end on a branded production app: the
recovered main renders the full signed-in UI pixel-identical to the original**,
spawning the identical helper process tree (main + GPU + N×Renderer + utility),
zero crashes, passing the asar-integrity + fuse checks. Reproducible procedure:

1. **Build the drop-in main bundle.** esbuild the recovered entrypoint to a single
   CJS file matching the original main bundle's shape: `--bundle --format=cjs
   --platform=node`, `--external:electron` plus every native/`.node`/externalized
   package the project depends on (each externalized bare specifier). Restore
   `import.meta` under CJS with
   `--banner:js="const __m=require('url').pathToFileURL(__filename).href;"`
   and `--define:import.meta.url=__m`. Use a fire-and-forget call to the recovered
   entry export (not top-level `await`) to avoid a CJS TLA error.
2. **Repack + re-seal the asar.** Replace the original main bundle inside a COPY of
   the original `.app`, repack `app.asar` with `@electron/asar`, then update
   `Info.plist`'s `ElectronAsarIntegrity` to the new header hash (SHA256 of the
   asar `headerString` via `@electron/asar` `getRawHeader`) — required because the
   app's fuses enable `EnableEmbeddedAsarIntegrityValidation` + `OnlyLoadAppFromAsar`.
   Re-sign ad-hoc: `codesign --force --deep --sign - <Copy.app>`.
3. **Launch via macOS `open <Copy.app>` — NOT the binary directly.** This is the
   decisive finding: launching `Contents/MacOS/<bin> --no-sandbox` directly does
   NOT spawn the Renderer/GPU helper processes (the window shows but stays blank);
   the renderer only renders under a proper LaunchServices `open`. Verify the app's
   `… Helper (Renderer)` / `(GPU)` processes appear, then `osascript -e 'tell
   application "<App>" to activate'` and screenshot.
   - **Gatekeeper block (`"Apple could not verify … is free of malware"` / `open`
     exits 0 but NO processes survive):** the copied/repacked app lost notarization
     and still carries the **quarantine** xattr, so Gatekeeper hard-blocks it.
     `xattr -dr com.apple.quarantine <Copy.app>` BEFORE (or after, then re-sign)
     launching. Do not disable Gatekeeper globally.
   - Update the integrity hash with `PlistBuddy` (`Set
     :ElectronAsarIntegrity:Resources/app.asar:hash <hash>`), NOT `plutil` — the
     key `Resources/app.asar` contains a dot that `plutil` keypaths mis-split.
   - Screenshot a specific window focus-independently with `screencapture -x -o
     -l<windowID>`, finding `<windowID>` via `CGWindowListCopyWindowInfo`
     (pyobjc-Quartz) filtered to the app's owner name and a real window size.
4. **Differential baseline.** Launch the ORIGINAL unmodified `.app` the identical
   way first. The recovered-vs-original window must match; any blank or missing
   content the original ALSO shows under the same launch is environmental
   (login/network/headless), not a recovery defect.

Reusing the user's existing login: `open` with the default user-data-dir reads the
real session (`~/Library/Application Support/<App>/`); close any concurrently
running copy first to avoid an IndexedDB `LOCK`. Fresh OAuth login redirects via an
`<app>://`-style deep link that LaunchServices routes to whichever bundle id is
registered (the original `/Applications` copy by default, since the drop-in shares
the bundle id) — re-register the drop-in with `lsregister -f` or reuse the existing
session to avoid the round-trip.

### Deterministic main-process equivalence smoke (when a real launch is infeasible)

A branded production Electron app (no display, requires sign-in, spawns helper
processes) often cannot be driven to renderer readiness in a sandbox. A real
`electron .` launch is also a poor *recovery* signal: the app fails for
environmental reasons (auth, network, GUI) long before any recovery defect
surfaces, and launching it has real side effects. Use a deterministic
**original-vs-recovered equivalence trace** as the main-process smoke:

1. Build an instrumented stub that intercepts `require('electron')` (and native
   `.node`/native packages, plus `net`/`http(s)`/`tls`/`child_process` so the run
   does no real I/O) via a `Module._load` hook. Model a real *main* process:
   `ipcRenderer` is `undefined`; `app.getPath`/`getVersion`/`getName` return
   strings; `app.whenReady()` resolves; define `process.versions.electron`. Every
   electron interaction (property path + call + arg shape) appends to a
   deterministic trace; deep-proxy everything else.
2. Run the **original** main bundle entry (the asar's main entry) and the
   **recovered** entry (the generated bundle's main, e.g. `dist/cli.js`) under the
   *identical* stub. The program is
   the same; only its textual form differs, so the two traces — and the
   load/async error sets — must match.
3. Pass criterion: recovered trace length and ordered interaction set equal the
   original's, and recovered raises no error the original does not. A divergence,
   or a recovered-only `SyntaxError`/`ReferenceError`/`has already been
   declared`/`before initialization`/dropped-statement crash, is a recovery
   defect — triage with the buckets above and fix the pipeline.

Module-load *order* legitimately shifts when a bundle is split into modules, so
diff the interaction **multiset** and the error set, not raw positional indices.
This smoke is deterministic, side-effect-free, and catches real
divergences (e.g. a dropped side-effecting top-level statement) that `tsc` cannot.

### Recovery defect classes the equivalence smoke catches (all fixed in-pipeline)

The smoke surfaced three distinct scope-hoisting defects on a large branded
desktop main bundle, each advancing the original-vs-recovered frontier when fixed;
all are now repaired in `reverts-next` with self-contained regression tests. They
recur on any large esbuild bundle, so the smoke remains the gate.

1. **Dropped side-effecting top-level statement** (`Cannot read properties of
   undefined`). A bare top-level expression statement with an observable side
   effect — a lazy global-registry init `(X=globalThis).__reg ?? (X.__reg =
   make())` whose only binding reaches live code through the `globalThis`
   side-channel — was dropped while its sibling read survived. Fix: preserve any
   non-declaration top-level statement outside every module span as an entrypoint
   side effect, regardless of byte position (`reverts-graph`
   `collect_runtime_prelude_declarations`).
2. **Reassigned import** (`Assignment to constant variable`). esbuild scope-hoists
   a shared mutable `var` into one module's slice while the WRITE lands in
   another; the writer imported the name (ESM imports are read-only) and
   reassigned it. Fix: a file that writes an imported binding re-homes it to a
   local `var` (`reverts-planner` `localize_written_imports`).
3. **Unbound init-thunk call** (`X is not defined`). The eager entrypoint calls a
   sliced module's `__esm` init thunk (`cdA()`) with no import and no other edge
   to that module. Fix: wire a referenced-but-unbound init-thunk call to its
   unique definer, gated on a shared bundle runtime-helpers import — deferred
   call ⇒ cycle-safe (`reverts-planner` `complete_init_thunk_imports`).

After these, the recovered main process completes full module-init under the smoke
with zero load/async errors and a 100%-covered interaction multiset (one extra
idempotent builtin require from eager init order is benign).

## CLI / Node bundle

1. Run `node ./<entry>.ts` or `tsx ./<entry>.ts` with `--version`, `--help`, or
   equivalent metadata flag. Require exit code 0 and empty stderr.
2. Run one concrete advertised subcommand path that exercises the recovered
   import graph, not just argument parsing.
3. If interactive, send a scripted input transcript via `expect` or
   `node:readline` and assert prompts appear in order.

## web-app

1. Start the generated app with the recovered package script (`pnpm run dev`,
   `pnpm run preview`, `pnpm start`) or a static server for generated HTML.
2. Open the served URL with Playwright MCP. Assert `document.readyState` reaches
   `interactive` or `complete`, the root UI element is non-empty, and no
   browser errors occur during initial load.
3. Exercise at least one visible route/control/form flow that exists in the
   recovered UI. Prefer original app metadata or obvious navigation controls
   over invented selectors.
4. Stop the server and assert the process exits cleanly.

## node-library

1. Run a module-load smoke with shell tooling: `node -e "import('./dist/index.js')"`
   or `tsx ./src/<entry>.ts`, matching the generated output shape.
2. If `package.json` declares `exports`, `main`, or named entrypoints, import
   each public entrypoint once.
3. Assert zero `SyntaxError`, `ReferenceError`, `TypeError`, unhandled rejection,
   or stderr stack trace.

## Failure routing

If any smoke check fails, triage it like a compile error and route it back to a
pipeline fix with a regression test.

| Smoke symptom | Likely root cause | Bucket |
|---|---|---|
| `SyntaxError: Identifier 'X' has already been declared` at load | `let → var` widen pass promoted past an enclosing `let X` lexical region | A |
| `ReferenceError: X is not defined` at top-level import resolution | dangling `__reverts_*` reference; synthesis audit orphan | A |
| `TypeError: Cannot read properties of undefined (reading 'Y')` from a `__reverts_pkg_*` namespace | app symbol routed through package import; misclassification | D |
| Service worker fails with `Unexpected token 'export'` | manifest needs `"type": "module"` for MV3 background — manifest-generation gap, not runtime patch | E |
| Cross-source-partition import resolved to an unrelated artifact source unit | source-partition evidence gate did not fire | B |
