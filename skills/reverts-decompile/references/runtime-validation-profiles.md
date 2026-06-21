# Runtime Validation Profiles

This reference defines which validation tools to use after ReverTS output is
generated. The main `reverts-decompile` skill owns the loop; this file is loaded
when selecting source/profile-specific runtime tests or debugging a smoke
failure.

## Required validation tools

Manual inspection is not a substitute for a scripted pass/fail signal.

| Step | Required tool | Contract |
|---|---|---|
| Project metadata, artifact profile, structural audits, persisted fixes | ReverTS MCP tools (`decompile_status`, `list_app_artifacts`, `get_artifact_manifest`, `query`, `generate_app_decompiled_files`, `update_modules` as needed) | Use DB/AST-backed data. Do not infer runtime profile from filenames alone when artifact metadata exists. |
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
   `list_app_artifacts` / `get_artifact_manifest`.
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
