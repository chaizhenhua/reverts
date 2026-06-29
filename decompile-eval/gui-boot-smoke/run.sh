#!/usr/bin/env bash
# Headless GUI boot smoke for a decompiled Electron app.
#
# Bundles the recovered main with esbuild, then loads it under a deterministic
# Electron mock (no display / native ABI needed) and asserts the bootstrap
# reaches main-window/view creation without a decompile-caused crash.
#
# Usage:  run.sh <generated-app-dir> [esbuild-bin]
#   <generated-app-dir>  a `generate --source-root src` project (has src/cli.ts)
#
# PASS = the result line reports "status":"window-created" (a BrowserWindow/
# BaseWindow or WebContentsView was constructed). Remaining rejections are deep
# service-wiring mock gaps (real services/renderer needed), not decompile bugs.
set -euo pipefail

APP="${1:?usage: run.sh <generated-app-dir> [esbuild-bin]}"
ESBUILD="${2:-esbuild}"
HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# 1. A no-top-level-await entry (the bundle must not mix require()+TLA).
cat > "$APP/.__boot_entry.mjs" <<'EOF'
import { zUt } from './src/modules/entrypoint.js';
globalThis.__bootSmoke = globalThis.__bootSmoke || {};
(async () => {
  try { await zUt(); globalThis.__bootSmoke.zutResolved = true; }
  catch (e) { globalThis.__bootSmoke.bootError = (e && (e.stack || String(e))) || 'x'; }
})();
EOF

# 2. Bundle the recovered main (externalise electron/natives + the runtime pkgs).
BANNER='import{createRequire as __cr}from"node:module";import{fileURLToPath as __fu}from"node:url";import{dirname as __dn}from"node:path";const require=__cr(import.meta.url);const __filename=__fu(import.meta.url);const __dirname=__dn(__filename);'
"$ESBUILD" "$APP/.__boot_entry.mjs" \
  --bundle --format=esm --platform=node \
  --external:electron --external:node-pty --external:ws \
  '--external:@ant/*' '--external:*.node' \
  '--external:@opentelemetry/*' --external:semver --external:form-data \
  --external:import-in-the-middle --external:require-in-the-middle \
  --banner:js="$BANNER" \
  --outfile="$WORK/main.bundle.mjs"
rm -f "$APP/.__boot_entry.mjs"

# 3. Harness: mocks + the 5 pure-JS ESM externals + iitm/ritm no-op stubs.
cp "$HERE/electron-mock.cjs" "$HERE/register.mjs" "$HERE/runner.mjs" "$WORK/"
printf '{ "name":"gui-boot-smoke","private":true,"type":"module" }\n' > "$WORK/package.json"
( cd "$WORK" && npm install --no-audit --no-fund --no-save \
    @opentelemetry/api@1.9.1 @opentelemetry/sdk-trace-base@2.8.0 \
    import-in-the-middle@2.0.6 require-in-the-middle@8.0.1 semver@7.8.5 >/dev/null 2>&1 )
for pkg in import-in-the-middle require-in-the-middle; do
  d="$WORK/node_modules/$pkg"; rm -rf "$d"; mkdir -p "$d"
  printf '{ "name":"%s","version":"0.0.0-stub","main":"index.cjs" }\n' "$pkg" > "$d/package.json"
  printf 'function Hook(){}\nHook.prototype.unhook=function(){};\nmodule.exports=Hook;\nmodule.exports.Hook=Hook;\nmodule.exports.default=Hook;\n' > "$d/index.cjs"
done

# 4. Run the smoke.
( cd "$WORK" && node runner.mjs )
