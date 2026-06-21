#!/usr/bin/env bash
# Claude Code decompiles Claude Code — the one-sentence flow, as the deterministic
# commands the reverts-decompile skill runs under the hood.
#
#   Input:  Claude Code's own npm package (a 13 MB minified cli.js)
#   Output: a tsc-clean, runnable TypeScript project that *is* Claude Code
#
# Reproduce / record:
#   asciinema rec -c "bash demo.sh" cc-2.1.89-demo.cast
set -euo pipefail

CLI="${REVERTS_CLI:-$HOME/.cache/cargo-target-shared/release/reverts-cli}"
PKG="${CC_PKG:-$HOME/Codes/reverts/cc-2.1.89/package}"
NODE_MODULES="${CC_NODE_MODULES:-$HOME/Codes/reverts/cc-2.1.89/out2/node_modules}"
SKILLS="${SKILLS_DIR:-$HOME/.codex/skills}"
OUT="${OUT:-/tmp/cc89-demo}"

banner() { printf '\n\033[1;36m==== %s ====\033[0m\n' "$1"; }

rm -rf "$OUT"; mkdir -p "$OUT"

banner "0. the input: one 13 MB minified line"
ls -la "$PKG/cli.js"
head -c 220 "$PKG/cli.js"; echo " …"

banner "1. collect — auto-unpack detects node-bundle, runs the collector"
python3 "$SKILLS/auto-unpack-target/scripts/auto_unpack.py" "$PKG" --out "$OUT/unpack" \
  | python3 -c "import sys,json;r=json.load(sys.stdin);print('kind:',r['classification']['kind'],'| skill:',r['classification']['skill'],'| dispatch ok:',r['dispatch'].get('ok'))"

banner "2. import — bundle facts into Reverts SQLite"
"$CLI" import-unpacked \
  --input "$OUT/unpack/node-bundle/unpacked" \
  --manifest "$OUT/unpack/node-bundle/reverts-import-evidence.json" \
  --project-name claude-code --output-db "$OUT/project.sqlite" \
  --bundle-source-bytes 100000 --max-source-bytes 20000000 | tail -1

banner "3. generate — split the bundle into readable TypeScript modules"
"$CLI" generate-project-v2 --input "$OUT/project.sqlite" --project-id 1 --output "$OUT/output" >/dev/null
echo "TypeScript modules: $(ls "$OUT/output"/modules/*.ts | wc -l)"

banner "4. compile — tsc on the recovered project"
cd "$OUT/output"
ln -sfn "$NODE_MODULES" node_modules
tsc -p tsconfig.runtime.json --noEmitOnError false
node ./scripts/copy-assets.mjs >/dev/null 2>&1 || true
echo "tsc: 0 errors"

banner "5. run — the decompiled code IS Claude Code"
echo "\$ node dist/cli.js --version"; node dist/cli.js --version
echo "\$ node dist/cli.js -p 'say hi'"; node dist/cli.js -p "say hi"
