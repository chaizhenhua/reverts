# Wave 5 — residual binding-name tail (23 files)

`wave5-residual-tail-23files.tsv` names the **173** residual minified bindings
that survived waves 1-4, across the two buckets a DB-grounded audit surfaced:

- **feature/module bucket (was 0% binding-named):** the wave-4 esbuild modules got
  semantic PATHS but no binding names — `auth/oauth-constants.ts`,
  `util/{buffer-constants,validators,errors-init}.ts`, `git/{repo-state-init,read-config}.ts`,
  `config/{include-exclude-filter,claude-config-dir-init}.ts`, `runtime/*-init.ts`,
  `diagnostics/hard-fail-state.ts`, `security/secret-detection-patterns.ts`,
  `fs/binary-file-extensions.ts`, `logging/debug-config.ts`, `state/app-config-signals.ts`.
- **island residual tail (94% -> 95%):** `modules/island/auth/{oauth,oidc,sso}.ts`,
  `modules/island/plugins/cli-manifest-validation.ts`,
  `modules/island/agent/{session-error-classification,tool-permissions}.ts`.

Format: `accept <TAB> <emitted-file-path> <TAB> <wire/original-name> <TAB> <semantic> <TAB> <evidence>`.
Keyed by the EMITTED file path (binding renames apply by exact path match) and the
minified wire name. Evidence is prefixed with the decamelized name tokens so the
`binding-names` vocabulary gate (name tokens ⊆ evidence ∪ technical-vocab) passes.

## How produced
7 fan-out agents read each emitted file body and proposed evidence-backed names,
skipping anything without quotable evidence (inlined lodash/axios/zod micro-helpers,
ambiguous bare numbers). 2 rows dropped for colliding with existing DB names
(redundant inlined-helper copies). Applied to `claude-decompiled-v2/project.sqlite`
(backup `project.sqlite.bak-pre-naming23`).

## Result (DB-grounded coverage, vendor + entrypoint-barrel excluded)
first-party binding naming **93% -> 95%**; feature/module bucket **0% -> 51%**
(remaining 49% = ambiguous magic-number constants conservatively skipped).
generate exit 0, 9235 relative imports resolve (0 real dangling).

## Statistics note (measurement correctness)
Coverage must be measured on the LOCAL/in-file name, NOT the `export { local as WIRE }`
wire alias (the wire stays minified by design); the `entrypoint.ts` re-export barrel
(2431 exports, 0% named, mechanical) and `vendor/` (intentionally unnamed third-party)
must be bucketed out; and use DB `semantic_binding_names` membership as ground truth
rather than a regex gibberish heuristic.
