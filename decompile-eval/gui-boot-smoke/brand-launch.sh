#!/usr/bin/env bash
# Package a decompiled Electron app into a BRANDED .app and launch it, so the
# dock/app icon and tray icon are correct.
#
# WHY this is needed (the icon mechanism):
#   The decompiler imports the app's asar (code + node_modules) but NOT the
#   surrounding `.app/Contents/Resources` shell files — `electron.icns` (the dock
#   icon, referenced only by Info.plist `CFBundleIconFile`) and the tray PNGs
#   (`TrayIconTemplate*.png`, loaded at runtime via `process.resourcesPath`). The
#   recovered code has NO `app.dock.setIcon` call, so the macOS dock icon is
#   purely bundle-driven: a bare `electron .` always shows Electron's icon.
#   The fix is therefore two mechanism steps, both done here:
#     1. stage the shell resources (icns + tray pngs) from the source .app, and
#     2. wrap the recovered app in a branded .app whose Info.plist names Claude +
#        `CFBundleIconFile=electron` → Contents/Resources/electron.icns.
#   (The durable fix is upstream: `import-unpacked` should capture the source
#    bundle's Contents/Resources shell assets — icons AND locale/translation
#    files — into the generated output's assets. The tray *menu* still needs the
#    locale files; same root cause.)
#
# Usage:  brand-launch.sh <generated-app-dir> <source-claude.app> <electron-runtime-dir> [seconds]
set -euo pipefail

APP="${1:?generated app dir (has main.bundle.mjs or built dist + node_modules)}"
SRCAPP="${2:?source Claude.app to copy shell resources + icns from}"
ERUNTIME_DIR="${3:?dir containing node_modules/electron (the Electron runtime)}"
SECONDS_RUN="${4:-30}"

ERUNTIME="$(find "$ERUNTIME_DIR/node_modules/electron/dist" -maxdepth 1 -name 'Electron.app')"
OUT=/tmp/Claude-branded.app
rm -rf "$OUT"; cp -R "$ERUNTIME" "$OUT"

# Recovered app inside the bundle.
rm -rf "$OUT/Contents/Resources/app"; mkdir -p "$OUT/Contents/Resources/app"
cp -R "$APP/main.bundle.mjs" "$APP/package.json" "$APP/node_modules" "$OUT/Contents/Resources/app/" 2>/dev/null

# Stage shell resources (icons + tray + locales) that the main process loads by
# path. Prefer the SELF-CONTAINED `<output>/resources/` produced by
# `generate --shell-resources` (the upstream fix); fall back to the source .app.
SHELL_SRC="$SRCAPP/Contents/Resources"
[ -d "$APP/resources" ] && SHELL_SRC="$APP/resources"
cp "$SHELL_SRC/electron.icns" "$OUT/Contents/Resources/electron.icns" 2>/dev/null || \
  cp "$SRCAPP/Contents/Resources/electron.icns" "$OUT/Contents/Resources/electron.icns"
cp "$SHELL_SRC/"TrayIconTemplate*.png "$OUT/Contents/Resources/" 2>/dev/null || true
cp -R "$SHELL_SRC/"*.lproj "$OUT/Contents/Resources/" 2>/dev/null || true
cp -R "$SHELL_SRC/locales" "$OUT/Contents/Resources/" 2>/dev/null || true
cp -R "$SHELL_SRC/i18n" "$OUT/Contents/Resources/app/resources/i18n" 2>/dev/null || true

# Brand the bundle so the dock shows Claude + its icon.
PB=/usr/libexec/PlistBuddy; PLIST="$OUT/Contents/Info.plist"
"$PB" -c "Set :CFBundleName Claude" "$PLIST" 2>/dev/null || "$PB" -c "Add :CFBundleName string Claude" "$PLIST"
"$PB" -c "Set :CFBundleDisplayName Claude" "$PLIST" 2>/dev/null || "$PB" -c "Add :CFBundleDisplayName string Claude" "$PLIST"
"$PB" -c "Set :CFBundleIconFile electron" "$PLIST" 2>/dev/null || "$PB" -c "Add :CFBundleIconFile string electron" "$PLIST"
"$PB" -c "Set :CFBundleIdentifier com.anthropic.claudefordesktop.real" "$PLIST" 2>/dev/null || true

# macOS caches dock/Finder icons by code signature + bundle id. Editing
# Info.plist/icns on an already-signed Electron.app leaves the OLD (Electron)
# icon cached, so the change appears to "not load". Re-sign ad-hoc so the new
# signature invalidates the cache, bump mtime, and reset the icon services cache.
codesign --remove-signature "$OUT" >/dev/null 2>&1 || true
codesign --force --deep --sign - "$OUT" >/dev/null 2>&1 || true
touch "$OUT"
/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister \
  -f "$OUT" >/dev/null 2>&1 || true
# The Dock caches running-app icons; reload it so the freshly-signed icon shows.
# (No sudo needed — this restarts only the current user's Dock.)
killall Dock >/dev/null 2>&1 || true

echo "branded: $OUT (icon=$("$PB" -c 'Print :CFBundleIconFile' "$PLIST"), name=$("$PB" -c 'Print :CFBundleName' "$PLIST"))"
rm -rf /tmp/claude-real-data; mkdir -p /tmp/claude-real-data
"$OUT/Contents/MacOS/Electron" --no-sandbox --user-data-dir=/tmp/claude-real-data > /tmp/brand-app.log 2>&1 &
EPID=$!
sleep "$SECONDS_RUN"
kill "$EPID" 2>/dev/null || true; pkill -P "$EPID" 2>/dev/null || true; sleep 1; kill -9 "$EPID" 2>/dev/null || true
echo "launched $OUT for ${SECONDS_RUN}s; log: /tmp/brand-app.log"
