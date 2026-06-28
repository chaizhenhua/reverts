# Vendor match-and-externalize experiment (Claude app)

Directive: "对于识别到的 vendor 要尝试匹配和外化" — for recognized inlined vendor
libraries, try to **match** (fingerprint-confirm) and **externalize** (turn the
inlined island bodies back into `import … from 'pkg'`).

## What was tried
Seeded 34 `island_package_candidates` (the ~10 already-anchored packages plus 24
newly-proposed inlined libs: `@anthropic-ai/sdk`, the `@opentelemetry/instrumentation-*`
family, `semantic-conventions`, `core`, `context-async-hooks`, `rxjs`, `ajv`,
`lodash`, `execa`, `cross-spawn`, `get-stream`, `fs-extra`, `winston`, `pako`,
`xmldom`, …). Ran the full cascade:

```
reverts-cli match-packages --input project.sqlite --project-id 1 \
  --materialize-package-sources --apply
```

It downloaded each package from npm, fingerprint-matched the island corpus, and
wrote 1101 `package_island_anchors` across 12 packages.

## Result: net negative — reverted
The match **anchors** the new packages but they **skip externalization**:

```
island-package skip: rxjs   (0 member binding(s)): no single unit transitively
                                  reaches all of the package's member submodules
island-package skip: winston   (0 member binding(s)): … same …
island-package skip: fs-extra  (0 member binding(s)): … same …
island-package skip: @anthropic-ai/sdk (0 member binding(s)): … same …
```

These are **tree-shaken inlined** libraries: esbuild kept only the reachable
slices and there is **no single barrel unit** whose import graph transitively
reaches every member submodule, so the externalizer cannot synthesize a sound
`import { … } from 'pkg'`. (Same reason `@sentry/*` and `debug` have always
skipped.) This confirms `[[inlined-thirdparty-inventory]]`.

Worse, re-running the cascade with the **broad 34-candidate corpus scope**
*regressed* a package that previously externalized:

| | island externalize |
|---|---|
| baseline (`bak-pre-vendor-match`) | **5** — `@opentelemetry/api`, `@opentelemetry/sdk-trace-base`, `import-in-the-middle`, `require-in-the-middle`, `semver` |
| broad vendor match | **4** — lost `@opentelemetry/sdk-trace-base` |

This is the over-scoping failure mode documented in `[[match-packages-perf]]`
("DON'T per-package-split island cascade"): the wider corpus shifted anchoring so
`sdk-trace-base`'s barrel unit no longer reached all members.

**Action:** restored the canonical DB to `project.sqlite.bak-pre-vendor-match`
(the 5-island-externalize baseline). The externalization ceiling holds at **5
island packages + 3 shape-A** (`form-data`, `node-pty`, `ws`) = **8 total**.

## Standing conclusion
Inlined, tree-shaken vendor libraries can be **anchored / relocated under
`vendor/<pkg>`** (via `cluster-names`) for readability, but they **cannot be
externalized** into runtime `import` statements unless a single inlined unit
happens to reach the whole package surface (only `api`, `sdk-trace-base`,
`iitm`, `ritm`, `semver` do). Don't re-run the broad-scope cascade — it costs
the `sdk-trace-base` win.

Artifacts:
- `accepted-candidates.tsv` — the 34 seeded candidates (snapshot of DB state).
- `match-run.log` — the cascade output (1101 anchors, 12 packages).
- `generate.log` — generate against the broad-anchor DB (4 externalize, the regression).
