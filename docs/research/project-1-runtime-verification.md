# Verification recipe: project 1 (default / Claude Code 2.0.75)

Recorded while building a measurable baseline for the emit-size optimization
work. Use this when you want to confirm that a change to the
matcher/planner/emitter has not regressed project-1 runtime behavior.

## TL;DR

```bash
./target/release/reverts-cli generate-project-v2 \
    --input ~/.reverts/.reverts.db --project-id 1 --output /tmp/p1
cd /tmp/p1
npm install --no-audit --no-fund --silent           # ~70s
npm run build                                       # ~12s, 0 tsc errors
node ./dist/cli.js --version                        # → "2.0.75 (Claude Code)"
node ./dist/cli.js --help                           # → full CLI help
```

> The historical manual `@sentry/*` pin is no longer required after
> commit `878f66e` ("align package.json deps to a single major per
> scope") landed scope-coherence picking. See the "Why the Sentry
> pinning was needed" section below for the bug it papered over and
> how the fix addresses it.

## Baseline numbers (matcher state of 2026-05-23, main = 9f3b6ae)

| metric | value |
|---|---|
| emitted files | 3904 |
| emitted `.ts` files | 3889 |
| total emit size | 53 M |
| `package.json` deps | 215 |
| `tsc --noEmit` errors | 0 (every file is `@ts-nocheck`) |
| audit findings | 13 warnings (DuplicateTopLevelBinding, all downgraded per ADR 0002) |
| `npm install` time | ~1 m 13 s |
| `tsc -p tsconfig.runtime.json` time | ~12 s |
| compiled `dist/` size | 59 M |
| `--version` output | `2.0.75 (Claude Code)` |
| `--help` output | full Claude CLI help text |

## Why the Sentry pinning was needed (historical, fixed in 878f66e)

Pre-878f66e, `node ./dist/cli.js --version` crashed without manual pinning:

```
TypeError: Cannot read properties of undefined (reading 'call')
    at .../buildPolyfills/_optionalChain.js:20:43
    at .../sentry/http-integration.js:43:32
    at setupIntegration (.../@sentry/core/build/esm/integration.js:112:17)
```

The matcher picks each Sentry package's latest cached version independently,
producing a `package.json` that mixes `@sentry/core@8.55.0` with
`@sentry/node@7.120.4`. Sentry's v7 → v8 transition broke the integration
contract that the bundled `http-integration` module relies on, so
`integration.setupOnce(addGlobalEventProcessor, getCurrentHub)` is called
with `getCurrentHub === undefined`, then crashes in `_optionalChain`.

`pick_scope_coherent_runtime_dependency_versions` in `crates/reverts-pipeline/src/lib.rs`
now groups packages by `@scope` prefix, scores each candidate major by how
many packages in the scope are attributed to it, and picks the winning
major before per-package version selection. For project 1's Sentry case
the v7 quartet (`core`, `node`, `tracing`, `utils`) wins the vote and
`@sentry/core` drops from 8.55.0 to 7.120.3 to join them. The lone
v8-only `@sentry/browser` still ships as 8.55.0 because no v7 attribution
exists for it, but the v7 core/node/tracing/utils group now boots
cleanly — that's enough for the runtime gate.

## What this gate covers

- **Bundle decompilation correctness**: the runtime helpers, lazy
  initialization, module wiring all hold together — the binary identifies
  itself, parses CLI args, and refuses invalid input with a Claude Code
  error message (not a JS-runtime error).
- **External import resolution**: 215 npm packages resolve and load
  without `MODULE_NOT_FOUND`.
- **TypeScript scaffolding**: 3889 `.ts` files compile through `tsc`
  (thanks to blanket `@ts-nocheck`, the gate is structural, not type-safety).

## What this gate does NOT cover

- Actual functional correctness of Claude Code's logic (would need a live API
  key and conversation harness).
- Sentry telemetry side effects (it runs but probably without remote
  reporting in this isolated environment).
- Any TypeScript type drift introduced by emitter changes (every file is
  `@ts-nocheck` so wrong types don't surface here).

## Using this gate around an optimization

Each emit-size optimization (binding rewrite, bundle aggregation, source-map
recovery, …) must rerun the full recipe and confirm:

1. `npm install --silent` still completes cleanly.
2. `npm run build` produces 0 `tsc` errors.
3. `node ./dist/cli.js --version` outputs `2.0.75 (Claude Code)`.
4. `node ./dist/cli.js --help` produces the full CLI help.

A failure at step 3 or 4 means the optimization broke decompiled
runtime — back it out, do not commit.
