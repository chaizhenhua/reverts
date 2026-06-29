# Decompiled-app GUI boot smoke

A headless, deterministic boot smoke for the **decompiled Claude desktop Electron
app**. It validates that the recovered main process boots through Electron's
lifecycle and reaches **main-window/view creation** — the GUI bootstrap path —
without a decompile-caused crash, with no real display or native ABI required.

## How it works

1. **Bundle** the recovered main (`src/cli.ts` → `entrypoint.zUt`) with esbuild,
   externalising `electron`, the natives (`node-pty`, `@ant/*`, `*.node`), and the
   runtime packages (`@opentelemetry/*`, `semver`, `form-data`, `ws`,
   `import-in-the-middle`, `require-in-the-middle`). A successful bundle is itself
   a structural check: the whole recovered module graph links (no dangling import).
2. **Mock** the runtime: `electron-mock.cjs` records `BrowserWindow` /
   `BaseWindow` / `WebContentsView` construction, resolves `app.whenReady()`, and
   drives `ready`/`activate` so the window-creation handlers run; `register.mjs`
   intercepts `require()` of electron + natives and shims the Electron-only
   `process` extras (`getSystemVersion`, `versions.electron`, …). The 5 pure-JS
   ESM externals install for real; iitm/ritm are no-op stubs.
3. **Run** `runner.mjs` loads the bundle and polls for window/view creation,
   printing a `BOOT_SMOKE_RESULT` JSON line.

## Run

```bash
./run.sh /path/to/generated-app /path/to/esbuild
# e.g.
./run.sh /Users/.../claude-decompiled-v2/app \
         /Users/.../claude-decompiled/app/node_modules/.bin/esbuild
```

**PASS** = `"status":"window-created"` (a BrowserWindow/BaseWindow or
WebContentsView was constructed).

## Result (Claude app decompile, 2026-06-29)

`status: window-created`, `viewsCreated: 4`, `whenReadyResolved: true`. The
recovered main:

- loads + executes the full 335k-line bundle (0 dangling imports);
- boots through `app.whenReady()` and runs the `app.on('ready', …)` async
  bootstrap with **real application logic** — enterprise-config load, growthbook
  feature-flag fetch (`https://claude.ai/api/desktop/features`),
  `RemotePluginManager` sync, `buddy-ble`, file-drop dispatch;
- executes `createMainWindow()` → constructs the main `WebContentsView`s and
  proceeds into `wireMainViewContents` (feature subscriptions, sessions bridge).

The only remaining errors are progressively-deeper **service-wiring mock gaps**
(a real renderer/services/network would satisfy them) and missing Electron
window-instance methods — **none are decompile defects**. This matches the known
environment limitation: full Chromium rendering needs real Electron + a display,
which reproduces in the original app too.

For a full visual launch, use real Electron 41.6.1 with a display (see the
real-Electron recipe in the project memory); this smoke is the deterministic,
headless, CI-friendly proxy.

## Real launch (no mocks) — `brand-launch.sh`

`brand-launch.sh <generated-app> <source-Claude.app> <electron-runtime> [secs]`
runs the REAL recovered code under real Electron 41.6.1 with the real natives
(`@ant/*`, `node-pty`, `ws` from the decompile's `assets/`) and real runtime
packages — zero mocks. Validated: the app boots, **creates a real window**, and
the renderer loads **https://claude.ai/login** (real growthbook feature-flag
fetch, hCaptcha). This is the authoritative GUI validation.

### Icon mechanism (why the dock icon was wrong, and the fix)

Root cause is an **asset-extraction gap, in two layers**:

1. The decompiler imports the app's **asar** (code + node_modules) but NOT the
   surrounding **`.app/Contents/Resources` shell files** — `electron.icns` (the
   dock icon) and `TrayIconTemplate*.png` (loaded at runtime via
   `getResourcesDir()` → `process.resourcesPath`). They sit OUTSIDE the asar and
   are referenced by dynamic path, so static asar-only import never captured
   them: `assets/` has zero icon files.
2. The recovered code has **no `app.dock.setIcon`** call, so the macOS dock icon
   is purely **bundle-driven** (`Info.plist CFBundleIconFile`). A bare
   `electron .` can therefore never show the Claude icon.

`brand-launch.sh` is the mechanism fix at the packaging layer: it (a) stages the
shell resources (`electron.icns` + tray pngs + locale files) from the source
`.app`, and (b) wraps the recovered app in a branded `.app`
(`CFBundleName=Claude`, `CFBundleIconFile=electron` → Claude's `electron.icns`),
so the **dock icon is correct** and the tray icon image resolves.

**Durable upstream fix:** extend `import-unpacked` to capture the source bundle's
`Contents/Resources` shell assets (icons, tray, locales, favicon) into the
generated output's `assets/` at the resources-dir path. The tray *menu* labels
(`formatIntlMessage`) need the app's own i18n message catalog — the same
shell-resource class, a separate file — and are staged by the same fix.

