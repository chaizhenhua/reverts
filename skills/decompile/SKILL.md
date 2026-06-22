---
name: decompile
description: Run ReverTS bundle decompilation through semantic naming, output generation, structural audits, and validation handoff.
argument-hint: "[project-id | file-path | directory] [-o output-dir]"
disable-model-invocation: false
---

# Bundle Decompilation Skill

Decompile a webpack/esbuild JavaScript bundle into readable TypeScript source files with semantic names.

## Install

Bundled with the `reverts` MCP server distribution; see
[skills/README.md](../README.md#install) for end-user install (`npm install -g
reverts`) and local-dev symlink installation (`./skills/install`). After
installing, restart your Claude/Codex session so the skill registry rebinds.

## Agent Boundary

This skill is an orchestrator over the ReverTS MCP server. It assumes the
server is reachable, the database is writable, and the bundle has already been
ingested into a project. It does not patch generated files, hand-edit
`package.json`, or build a TypeScript scaffolding by hand — those belong to
[reverts-decompile](../reverts-decompile/SKILL.md). Mechanical defects
discovered here MUST be filed as ReverTS pipeline issues, not papered over in
this run.

When a user asks an Agent/Ant to "complete decompilation", this skill MUST NOT
stop at file generation. After Phase 5 succeeds, immediately hand off to
[reverts-decompile](../reverts-decompile/SKILL.md) and run its post-decompile
validation contract: dependency install, real TypeScript compile/edit
validation, source/profile-selected runtime smoke validation, and
Playwright-backed UI interaction checks for browser or extension outputs.

## Core Rules

- Run the control loop until the Phase 4 completion gate passes **or** a hard
  blocker fires (see [Hard Blockers](#hard-blockers) below). Do not loop past
  a blocker; surface it and stop.
- Do not ask for confirmation on routine progress questions. Only ask the
  user when an input is missing, when a hard blocker is hit, or to choose the
  **naming target tier** beyond the mandatory `public-surface` gate (see
  [Naming to target](#naming-to-target-mechanism-first-coverage)) — that scope
  is a cost decision the user owns.
- The main agent is an orchestrator: status → dispatch → re-check status.
- Default to `cat:"app"`. Use `cat:"pkg"` only with exact npm package identity,
  npm-installable version (e.g. `"4.28.1"`, a concrete semver that
  `npm install pkg@ver` can resolve), and clear upstream-source confidence.
  A package classification is not complete until `update_modules` records
  `emit`, `subpath`/`specifier`, and evidence, then `verify_package_attributions`
  accepts it against the installed `node_modules` tree.
- **All public-surface names must be semantic before output.** Public surface =
  exported symbols + owned globals **+ module file paths + module names**. The
  emitted file path and the module name are read by every importer and by anyone
  browsing the tree, so they are public surface too — rename them as part of the
  public-surface pass, not as an afterthought (`module-names --accept
  <MODULE_ID=path>` for paths; `reference-source-names` sets both automatically
  when an upstream tree exists). The `public_surface` ratio must reach 100%.
- Prioritize public surface first: module paths/names, exported symbols, owned
  globals, constructor/state fields, internal helpers, locals last.
- Do not rename bundler/runtime helpers into fake business terms just to
  remove warnings.

Reference docs, loaded only when needed: [analysis patterns](references/analysis-patterns.md), [submit format](references/submit-format.md), [sub-agent templates](references/sub-agent-templates.md), [init-shim classification](references/init-shim-classification.md), [post-output audits](references/post-output-audits.md), and [guardrails](references/guardrails.md).

## Concurrency

All sub-agent dispatch in this skill writes to the same SQLite database. With
more than 5 concurrent agents, lock contention causes widespread retry loops
that waste more time than they save.

- Use **3–5 sub-agents** maximum, never 8–10.
- Each sub-agent processes its modules **sequentially** (one at a time).
- Batch **50–80 modules per agent** in Phase 2; smaller batches in Phase 3.
- If you observe `database is locked` retries above ~10% of operations,
  reduce the agent count by one and re-dispatch the remaining work.

## Hard Blockers

Stop the control loop and report the blocker if any of the following holds:

| Blocker | Signal | Recovery |
|---|---|---|
| Missing input | `$ARGUMENTS` empty and `list_projects()` shows no candidate | Ask the user which bundle/project to decompile |
| Permission denied | MCP write tool returns `permission denied` / FS write fails | Ask the user to grant the permission or relocate the output dir |
| MCP unreachable | `decompile_status` returns connection error or hangs >30s | Run `cargo build --release --bin reverts-mcp`, then `/mcp` to reconnect; do NOT `pkill` |
| Schema mismatch | `decompile_status` returns `schema version mismatch` | Run the latest migration binary or re-ingest into a fresh project |
| Same op fails 3× | Same tool call with same params fails three consecutive times | Stop and surface the error; do not loop further |

## Phase 0: Resolve Input

Resolve `$ARGUMENTS` into `project_id` and optional `output_dir`.

### Output directory

If `$ARGUMENTS` contains `-o <path>` or `--output <path>`, resolve it to `output_dir`. Otherwise the MCP server default is `{project_root_path}/out`.

**The output directory must be persistent — never under `/tmp` or any
scratch/temp location.** A decompiled app is a long-term project: its generated
source, its SQLite project DB, and its `e2e/` validation harness must survive
reboots and tmp cleanup, and regeneration (`generate-project-v2 --output <dir>`
preserves a pre-existing `e2e/` subtree). Prefer a stable project root such as
`~/<workspace>/<app>-decompiled/` holding the project DB beside the generated
app, e.g. `…/app/` (generated source) + `…/project.sqlite`. If the resolved
output is a temp path, relocate it and tell the user where it lives.

**Prefer the modern layout: `generate-project-v2 --source-root src`.** It emits
recovered source under `src/`, a `NodeNext` tsconfig (the recovered code runs on
Node ESM with explicit `.js` specifiers — `NodeNext` models that), a
`package.json` `exports` map, `README.md` + `.gitignore`, and relocates pipeline
metadata (`symbol-index.json`, `binding-name-index.json`) to a `.reverts/`
sidecar so the source tree is clean. The flat layout (omit the flag) stays
available for backward compatibility. The naming-plan/progress `--symbol-index`
path then points at `.reverts/symbol-index.json`.

### User-specifiable knobs (don't hardcode; honor user requests)

Layout and names are inputs the user can dictate — never bake in app-specific
assumptions:

- **Layout / source root** — `--source-root <dir>` (default `src` when modern).
- **Entry** — the runtime entrypoint is auto-detected, but its emitted export
  name is renamable through the binding channel (e.g. `zUt → runMain` via
  `binding-names`); downstream tooling derives the entry from the generated
  `cli.ts`, so it follows the rename.
- **Module names / paths** — `module-names --accept <id=path>` (or
  `reference-source-names`); the user's chosen paths win.
- **Symbol / binding names** — `symbol-names` (module symbols) /
  `binding-names` (island bindings); accept user-provided names verbatim,
  agent-proposals only fill the residue.

### Source argument

After stripping `-o`, resolve the remaining argument as:

| Argument | Action |
|----------|--------|
| empty | Ask user which bundle files or directory to decompile |
| number | Treat as `project_id`; verify with `list_projects()` |
| file path | Find/create project, then ingest file if needed |
| directory | Find bundle files, find/create project, ingest if needed |
| other string | Try matching project name via `list_projects()` |

### Resume detection

After resolving `project_id`:

1. Call `decompile_status(project_id)`
2. Call `query(project_id, entity="modules", category="app", has_semantic="false", page_size=1)`
3. If modules already exist, skip setup and enter the decision loop.

## Control Loop

Repeat:

1. `decompile_status(project_id)`
2. `query(project_id, entity="modules", category="app", has_semantic="false", page_size=1)`
3. Match the first applicable rule below
4. Dispatch parallel sub-agents
5. Re-check status

### Decision Table

| Condition | Action |
|----------|--------|
| third-party not yet externalized (vendored modules **or** inlined island libraries still present) | run full externalization first — shape A + shape B (see [Package matching](#package-matching--externalization-third-party)); externalization precedes agent naming |
| unnamed app modules > 0 | classify + name agents (combined) |
| `incomplete_decompilation > 0` | symbol naming agents for incomplete modules |
| `missing_semantic_name > 0` | symbol naming agents |
| `mechanical_semantic_name > 0` (module-level) | mechanical fix agents |
| `mechanical_semantic_name > 0` (symbol/global-level) | mechanical fix agents |
| `package_attribution_unverified > 0` | package attribution correction + verification agents |
| `non_existent_package > 0` | package reclassification agents |
| `public_surface` ratio < 100% **and** an upstream first-party source tree is available and not yet applied | deterministic auto-name first (see [Naming to target](#naming-to-target-mechanism-first-coverage) step 2) |
| `public_surface` ratio < 100% | symbol naming agents for the residue, routing accepts by `rename_channel` (see [Naming to target](#naming-to-target-mechanism-first-coverage)) |
| path organization not reviewed | path organization agents |
| otherwise | proceed to output |

### Progress line

After each check, report:

```text
Status: public_surface={named}/{total} ({pct}%) | {unnamed} unnamed | {incomplete} incomplete | {mechanical} mechanical | {missing} missing -> dispatching {N} agents
```

The `public_surface` field (from `decompile_status`) tracks public-surface symbol naming progress and must reach 100% before output generation.

## Naming to target: mechanism-first coverage

The coverage gate is a *result*, not a wish. Reach it by spending the cheapest,
highest-confidence mechanism first and falling back to agent naming only for the
residue. Run these in order every time; do not jump straight to per-module agent
naming.

> **Ask the user for the target tier before spending agent-naming budget.**
> `public-surface` is the mandatory gate ([Phase 4](#phase-4-completion-gate));
> `declarations` and `full` are optional and can be **5–7× larger** (real bundles
> run to tens of thousands of bindings, dominated by low-value local names). After
> step 1 (denominator) and step 2 (deterministic auto-name), run `naming-progress`
> per tier and **present the remaining counts to the user, then ask which tier to
> drive to 100%** — do not silently grind to `full`. This is an explicit exception
> to "don't ask routine questions": naming scope is a cost decision the user owns.
> Default to `public-surface` only if the user does not answer. Deterministic
> mechanisms (steps 1–2) and the package/externalization pass always run
> regardless of the chosen tier; the question only bounds *agent* naming.

1. **Externalize all third-party code FIRST — including inlined island
   libraries — then refine the denominator.** This is the single most important
   ordering rule, learned the hard way: a scope-hoisting bundler inlines whole
   libraries (`@opentelemetry/*`, `@sentry/*`, `semver`, `shimmer`, `debug`,
   `lodash`, …) into the eager island, and they can account for the **majority**
   of the island's bindings. Naming inlined third-party code is wasted work and
   inflates the denominator with names no human should ever read. So before any
   agent naming:
   - run the full [Package matching & externalization](#package-matching--externalization-third-party)
     pass — **both** shape A (vendored `node_modules` modules) **and** shape B
     (inlined island libraries via `island-package-candidates` →
     `match-packages --materialize-package-sources`);
   - then `module-classify --auto --apply` to mark package/runtime-glue modules.

   An inflated denominator makes 100% unreachable; a silently deflated one fakes
   it. Re-confirm denominator integrity per the
   [Phase 4 rules](#p0--naming-denominator-integrity-no-silent-exclusion) — the
   entry-island group must be present and counted, but only over the bindings
   that remain *after* externalization. **Agent naming may not begin while
   recognizable inlined-library regions still sit in the denominator** — route
   them to externalization (next bullet) instead.

2. **Auto-name deterministically when an upstream first-party source tree is
   available.** A historical/published source tree for the same app names
   modules, exports, AND bindings — including the entry island — by structural
   match, auto-accepting only high/medium-tier hits with recorded evidence
   (`reference-source-names --reference-source-root <dir> --reference-version
   <ver> --apply [--min-tier high|medium]`). This is the largest coverage lever
   and the accepted names pass the naming gate by construction. Run it BEFORE
   spending any agent budget. It is precision-gated, so it never invents names;
   absence of a reference tree just means you skip to step 4.

3. **Measure honestly, reusing the just-emitted index.** `naming-progress --json
   --symbol-index <out>/symbol-index.json` (and `naming-plan` likewise) avoids a
   re-emit each loop iteration. Read the per-tier `named/total` and the per-file
   groups; verify the entry-island group (null `module_id`,
   `rename_channel: "binding-names"`) is counted.

4. **Plan the residue per tier.** `naming-plan --target-level <tier>
   --symbol-index <out>/symbol-index.json` emits the unnamed worklist grouped by
   file, each target carrying `evidence_tokens` and a `rename_channel`. Work the
   tiers in goal order: `public-surface` → `declarations` → `full`.

5. **Agent-name the residue, routing accepts by `rename_channel`.** Module
   bindings accept through the module/symbol channel
   (`symbol-names --batch --apply`, keyed by `module_id`); module-less
   entry-island bindings accept through the file-path channel
   (`binding-names --batch --apply`, keyed by `file_path`). Always pass
   `--evidence` so `origin=agent` names clear the naming gate on the first try;
   the worklist's `evidence_tokens` are the raw material for that evidence.

   **The naming agent is also a third-party DETECTOR — wire it back to
   externalization.** Reading the island, an agent reliably recognizes inlined
   libraries by their fingerprints: the lazy-CJS module idiom
   (`var NS = {}; var flag; function init(){…}` triples), `VERSION` constants,
   and known class/enum/API names (`ProxyTracer`, `ZodError`, `SentryClient`, the
   semver regex ladder, lodash `getRawTag`/`isPlainObject`). When an agent finds a
   contiguous region that is a vendored package, it must **STOP naming that region
   and instead emit a package candidate** —
   `island-package-candidates --accept <pkg> [--version <v>] --evidence "<the
   API/version anchors it saw>"` — so the next externalization pass removes the
   whole region from the denominator. Do NOT spend names on code that is about to
   be externalized. After each detection round, re-run shape-B externalization and
   re-measure; only the genuine first-party remainder gets named.

6. **Name the module files, not just the symbols.** Readability also means the
   emitted file paths. Accept a semantic path per module
   (`module-names --accept <MODULE_ID=path> --apply`, stored as a
   `module_path_overrides` row); `generate-project-v2` then moves the module's
   file to that path and recomputes every importing file's relative specifier.
   Reference-source matching can also set these automatically when an upstream
   tree exists. Wire/export names are untouched, so the build still links.

7. **Regenerate and re-measure; loop until the tier target is met.**
   `generate-project-v2` applies accepted names, then return to step 3.
   Export-name readability holds by construction: an exported symbol's semantic
   name propagates into every importing module, so consumers read the semantic
   name. For a binding whose semantic name is provably safe to expose
   project-wide — defining module not namespace-imported or re-exported, name
   globally unique — the public import/export *wire* name is renamed too, so the
   emitted module surface reads `export { parseDocument }` /
   `import { parseDocument }` with no minified alias. Bindings that fail that
   gate keep the wire alias (`export { parseDocument as Cb }`), and a renamed
   island entry binding keeps its export alias so `cli`/importers never break.
   Advance to the next tier only after the current one reaches 100%.

This ordering is what makes the [Phase 4](#phase-4-completion-gate) coverage gate
*achievable* rather than aspirational: deterministic mechanisms cover the bulk,
agent naming closes the precise remainder, and both naming channels keep the
entry island in scope.

### Naming-channel routing and id-spaces (do not mix them)

A binding is named through exactly one channel, decided by where it lives:

| Where the binding lives | Channel / CLI | Key |
|---|---|---|
| First-party **module** symbol (has a `modules` row) | `symbol-names --batch` | DB `modules.id` |
| **Module-less** island binding (`entrypoint.ts`, `island/cluster-*.ts`) | `binding-names --batch` | `file_path` |
| Module **file path** | `module-names --accept <id=path>` | DB `modules.id` |

Two id-spaces exist and are easy to confuse: `symbol-names`/`module-names`
validate against **DB `modules.id`** (`module_belongs_to_project`), while
`naming-plan` and emitted file prefixes use the **output module id** (the
`NNN-` prefix in `modules/NNN-….ts`). For vendored `node_modules` modules the
two coincide (emitted prefix `== modules.id`), so `symbol-names --accept
22:Orig=Sem` works directly. For main-bundle-extracted modules they may diverge;
resolve the DB id via the `modules` table before accepting, or route island-level
work through `binding-names`. A `module N does not belong to project` error means
you handed an output id where a DB id was required.

Renaming a module export's **wire** name (the all-or-nothing collapse of
`export { Sem as Orig }` → `export { Sem }`) propagates to *every* importer,
including the entry island's own direct and packed source-module imports. If a
named build ever fails to bundle with `No matching export … for import 'Orig'`,
that is a propagation bug in the pipeline (an importer kept the stale wire name),
not something to hand-fix in the output — file it and fix the mechanism. The
pipeline now guards this class deterministically: `audit_emitted_named_export_consistency`
(`DanglingNamedImport`, **Error**) resolves every first-party named import to its
target module and blocks output if the imported wire name is not actually
exported there — so a propagation regression fails in-pipeline with the exact
importer / name / target instead of cryptically at esbuild. Targets with a bare
`export *` are treated as opaque (never falsely flagged).

## Package matching & externalization (third-party)

Externalization replaces a recovered copy of a third-party library with a bare
`import 'pkg'` / `require('pkg')`, shrinking the first-party surface that has to
be named and verified. There are two distinct shapes; run both.

### A. Vendored packages (their own `node_modules` modules)

Libraries shipped as real files (`node_modules/ws/…`, `node_modules/node-pty/…`,
private `@scope/*`) own `modules` rows and are matched deterministically.

1. **Classify** — `module-classify --auto --apply` marks vendored
   `node_modules` paths as third-party (deterministic; also refines the naming
   denominator). For ambiguous modules, an agent supplies verdicts via
   `module-classify --batch <MODULE_ID<TAB>classification<TAB>evidence>`.
2. **Match against local sources** — `match-packages --package-source-root
   <appRoot> --apply`. `<appRoot>` is the extracted app root whose
   `node_modules` holds the real package sources (and the merged
   `app.asar.unpacked` natives). The matcher fingerprints each module's surface
   against the package source and, only when the module's function surface is a
   subset of the package's, accepts an `external_importable` attribution. Private
   `@scope/*` packages with no public registry 404 and are skipped — expected.
3. Output then emits bare `import`/`require` for accepted packages; the `.node`
   natives are carried as assets.

### B. Inlined libraries (bundled into the island, no module)

A scope-hoisted bundle inlines libraries (zod, lodash, mermaid, d3, …) into the
eager island with **no module boundary** and all names minified — the vendored
matcher (module-keyed) can never see them. Recover them with the
**agent-proposes → deterministic-confirms** anchoring flow (the user contract:
"third-party packages are proposed by the model/Agent, then confirmed by
deterministic matching"):

1. **Agent proposes candidate package names** from string anchors / API shapes
   visible in the island (`z.object`, `ZodError`; `cloneDeep`, lodash internals;
   mermaid/d3 globals): `island-package-candidates --accept <pkg> [--version <v>]
   --evidence "<anchors>" --apply`. A wrong guess is harmless — it just fails to
   match and anchors nothing.
2. **Deterministic confirm** — `match-packages --reference-source-root <appRoot>
   --materialize-package-sources --apply`. This reads accepted candidates (and
   reference `package.json` devDependencies — bundled libs live there, not in
   shipped deps), downloads only concrete compatible versions into the package
   cache, and fingerprints island bindings against them, writing
   `package_island_anchors` (keyed by `(project, source_file, binding)`, no
   `module_id`). `generate-project-v2` then drops anchored island bindings from
   the naming denominator.
   - Anchoring uses minification-robust axes (structural/feature/string anchors).
     Per-function structural hashing alone is too weak across esbuild
     scope-hoist+minify vs npm source; this is why module-level **clustering of
     the island happens first** — cluster, then match at module granularity.
   - The step-A command already runs island anchoring opportunistically: when
     `<appRoot>` ships the inlined libs' sources (under its `node_modules` or as
     reference `devDependencies`), `match-packages --package-source-root <appRoot>`
     builds an "island corpus" and anchors in the same pass — no separate
     candidate step needed. Use the explicit `island-package-candidates` +
     `--materialize-package-sources` flow only for libs whose sources are NOT
     on disk and must be fetched from the registry.

3. **Invalid / missing version → Agent re-analyzes and re-proposes.** When
   materialization logs `skipping … : no matching npm version` (or a candidate has
   no version), the hint was wrong and that package will NOT anchor. Do **not**
   give up — `match-packages` now prints the next step inline. Dispatch an Agent
   to determine a **real** npm version:
   - List actual versions: `npm view <pkg> versions --json`. Never propose a
     version absent from this list.
   - Read the inlined source (`island/vendor/<pkg>*.ts` or the original bundle)
     for a `VERSION` constant, characteristic API shape, and **dependency
     coherence** — `npm view <pkg>@<v> dependencies` must be consistent with
     already-anchored packages (e.g. an `@opentelemetry/api@1.9.0` anchor pins the
     compatible `core`/`sdk-trace-base` line; a bundled `@sentry/electron@7.4.0`
     literal pins its exact `@sentry/core`/`@sentry/node`).
   - Beware version-line confusion: a `VERSION` string lifted from one module may
     belong to a *different* package in the same family (OTel's experimental line
     `0.2xx` vs the stable `2.x` line for `core`/`sdk-trace-base`).
   - Re-propose with the corrected version: `island-package-candidates --accept
     <pkg> --version <real> --evidence "<dating chain>" --apply`, then re-run
     `match-packages`. A wrong re-guess is still harmless (no anchor).

### Report quirk: verify the *output*, not just the metric

`match-packages` can log `0 package source eliminated (0.00%)` while still
writing valid attributions/surfaces and producing bare imports — the elimination
% is a source-byte metric that lags single-pass runs. Do NOT conclude
externalization failed from that line. Confirm against the regenerated output:
grep for the expected `import 'pkg'` / `require('pkg')`, check the naming
denominator shrank (anchored + externalized bindings leave `symbol-index.json`),
and that `tsc`/trace still pass. `AmbiguousPackageSurfaceVersion` for a package's
uninstalled *optional* native deps (e.g. `bufferutil`, `utf-8-validate`,
`cpu-features` for `ws`) is expected and harmless — those stay inlined.

### Verify (both shapes)

`match-packages-report --all-projects` (or per-project) shows match,
externalization, and source-elimination rates. After regenerating, confirm bare
imports are present, `tsc -p tsconfig.runtime.json --noEmit` still exits 0, and
the equivalence trace is unchanged (externalized package calls become stubbed
interactions but must keep the same multiset). Never demote a real match to
`app` to silence a verification miss — fix the identity/version/subpath instead.

### C. When a package CANNOT be externalized → relocate to `vendor/`

Externalization is not always possible, and forcing it would emit broken code.
A package is **un-externalizable** when any of these hold — `generate-project-v2`
logs the reason as `island-package skip: <pkg> (…)`:

- **No coverable barrel.** "no single unit transitively reaches all of the
  package's member submodules" — the inlined units don't share one entry that
  dominates them, so there is no synthesizable barrel to route a single
  `import 'pkg'` through (seen for `node-pty`, `ws` as island libs; they still
  externalize via shape A when they own real `node_modules` modules).
- **App reaches into internals.** The emitter safety gate rejects when a member
  outside the barrel-dominated closure is referenced by retained (first-party)
  code — deleting it would dangle. The whole package is conservatively kept
  inlined rather than emit a hanging reference.
- **Tree-shaken fragment.** Only a partial slice of the library was bundled
  (most of `zod`, `lodash`, `rxjs`, `ajv`, …). There is no complete package to
  import, and minified fragments fingerprint-match weakly or not at all. These
  never anchor in the first place.

For all three, **do not name the region as first-party and do not leave it as an
anonymous `cluster-NNNN.ts`.** Relocate it to a `vendor/`-prefixed path whose
name reflects the recognized package, so the output is honest about provenance:

1. The agent reading the cluster recognizes the library (string anchors, API
   shapes, `VERSION`, known class names) and assigns a path via
   `cluster-names --accept <fingerprint>=vendor/<package-or-submodule> --evidence
   "<what it saw>" --apply` (keyed by the cluster's stable content fingerprint,
   `origin=agent`). Examples already in use: `vendor/zod-checks-string`,
   `vendor/ajv-formats`, `vendor/opentelemetry-instrumentation-koa`.
2. Regenerate — the cluster now emits as `modules/island/vendor/<package>.ts`
   instead of `cluster-NNNN.ts`. The code stays inlined (it runs; equivalence
   trace unchanged), but it is clearly labeled third-party.
3. **`vendor/` contents are NOT first-party naming work.** Once a region is
   relocated under `vendor/`, agents must not spend semantic-naming effort on its
   internal bindings — it is recovered third-party code, not application code.
   Only genuinely first-party clusters get binding/symbol names.

Decision order per recognized third-party region: **externalize (A or B) first;
only if that is impossible, relocate to `vendor/` via `cluster-names`.** The end
state is N externalized `import 'pkg'` + a `vendor/` tree that mirrors the
remaining inlined libraries by package, leaving only true first-party code as
anonymous/named clusters.

## Phase 1: Setup

Only for fresh projects:

1. `list_projects()`
2. `create_project(...)` if needed
3. `ingest_decompile_sources(...)`
4. `detect_runtime_helpers(project_id)`
5. `submit_runtime_helpers(project_id, confirmations=[...])`

Pass the confirmed helper mapping to every naming/fix agent.

## Phase 2: Classify + Name (Combined Single Pass)

Classification and symbol naming happen in ONE pass per agent. Each agent reads source, classifies module category, assigns semantic name, AND names all unnamed symbols simultaneously.

### Agent dispatch

- Batch **50-80 modules** per agent
- Dispatch **3-5 agents** (NOT 8-10 — SQLite lock contention degrades throughput)
- Each agent processes modules **sequentially** (one at a time) to avoid lock contention
- Use `update_modules(...)` for classification, then `submit_module_decompilation(...)` for symbol naming

### Per-module workflow (inside agent)

For each module the agent must:

1. `get_module(project_id, module_name, include_symbols=true)` — check if already complete
2. `get_source(project_id, target="module", module_name, transform=true)` — read transformed source
3. **Classify**: determine `cat` (app vs pkg) using package fingerprints (see decompiler.md)
4. **Name**: assign semantic path AND name all unnamed symbols in one submission
5. Submit via `update_modules` (for classification) + `submit_module_decompilation` (for symbols)
6. For `cat:"pkg"`, include package attribution fields:
   - `pkg`: exact npm package name
   - `ver`: exact installed package version
   - `emit`: `external_import` when output should import from npm, `vendored_asset` when the file stays materialized in output
   - `specifier`: legal import specifier for `external_import`
   - `subpath`: package-relative source file evidence when known
   - `evidence`: concise structured evidence explaining the upstream match

### Init-wrapper fast path

If a module has only 1-2 unnamed symbols and is a pure init-wrapper (just imports + trivial assignments):
- Name the export symbol by converting the semantic path's last segment to camelCase + "Module" suffix
- Example: semantic `init/opentelemetry-api-chain-2` -> export symbol = `opentelemetryApiChain2Module`
- No need to read source deeply for these

### Package detection during classification

During classification, agents MUST check for package fingerprints BEFORE defaulting to `app`. See the package fingerprint list in the decompiler agent prompt. Common patterns:
- AWS SDK: `class X extends $Command`, `__exports(m, { XCommand: () => ... })`
- Zod: `z.object({...})`, `ZodType`, `ZodString`, `_parse`, `_def`
- Lodash: `_.chunk`, `_.merge`, `baseClone`, `copyObject`, internal utilities
- Semver: `SemVer`, `Range`, `parse`, `valid`, `gt`, `lt`, comparators
- YAML: `Lexer`, `Parser`, `Document`, `Scalar`, `Pair`, CST types
- OpenTelemetry: `Span`, `Tracer`, `SpanContext`, `propagation`
- gRPC: `Channel`, `Client`, `ServerCredentials`, `Metadata`

## Phase 3: Diagnostic cleanup

### 3.1 Incomplete decompilation

If `incomplete_decompilation > 0` after Phase 2:
- These are modules where the export symbol still has no semantic name
- Dispatch agents to get_module + get_source + submit_module_decompilation for each
- Typically init-wrappers missed in Phase 2

### 3.2 Mechanical names

- Fetch `decompile_status(..., issue_type="mechanical_semantic_name")`
- Process by subject_kind in order: **module** first, then **symbol**, then **global**
- Module-level: often indicates misclassified packages or bad init-wrapper names
- Symbol-level: often indicates cross-module name collisions that need disambiguation
- Global-level: often indicates cascading issues from large init-wrapper modules
- Dispatch fix agents, 3-5 max

### 3.3 Package fixup

> CLI pipeline: the deterministic externalization flow (classify → match local
> sources → anchor inlined island libraries) lives in
> [Package matching & externalization](#package-matching--externalization-third-party).
> The MCP steps below are the server-tool equivalents.


- Fetch `decompile_status(..., issue_type="package_attribution_unverified")`
- For `missing_attribution` or `proposed`, read source evidence and resubmit
  `update_modules` with `pkg/ver/emit/specifier/subpath/evidence`
- Run `verify_package_attributions(project_id, node_modules_root=...)`
- If verification rejects the proposal, correct the package identity,
  version, subpath, or export specifier; do not demote to `app` unless source
  evidence proves it is first-party code
- Fetch `decompile_status(..., issue_type="non_existent_package")`
- Reclassify wrong `pkg` modules back to `app`
- Correct package names/versions only when evidence is strong

### 3.3.1 Trivial init-wrapper misclassification

Bundles contain many tiny init-wrapper modules (≤50 bytes, 0-2 symbols) that just call a single dependency or alias one global: `var X = O(() => { dep(); });` or `var X = O(() => { Y = Z; });`. They have no independent identity — their classification must follow the **owner of their target global**.

A blind bulk-demote to `cat:app` is wrong: real-package init shims (e.g. lodash, zod, date-fns internal init wrappers) belong to `cat:pkg + <package_name>` so output emits `import 'lodash'` instead of a relative import to a stub file. See [init-shim-classification.md](references/init-shim-classification.md) for the full recovery protocol, source fingerprints per package, and the cross-project mislabel cascade failure mode.

**Detect candidates** (SQL on the DB):
```sql
SELECT original_name, semantic_name, package_name, module_category, wrapper_kind
FROM modules
WHERE module_type='esm_lazy'
  AND wrapper_kind IN ('pure_init_wrapper','composite_init_wrapper')
  AND (byte_end - byte_start) <= 50
  AND symbol_count <= 2;
```

**Classify each match by parent-chain authority** (do NOT bulk-demote):

1. **Source-content fingerprint first.** Read the shim and the module it transitively wraps; look for the package's literal symbols/constants (table in `init-shim-classification.md`).
2. **Parent-chain inheritance second.** Find modules that depend on this shim. If they unanimously share a `package_name` AND have **specific** semantic names (`lodash/_internal/base-clone`, `parse5/html-constants` — not generic `<pkg>/init-wrapper-NN`), inherit `cat=pkg, pkg=<parent.pkg>, ver=<parent.ver>`.
3. **App parent only → demote.** If parents are all `cat=application`, set `cat=app`, leave pkg/ver null.
4. **Contaminated cluster or split-vote parents → leave `unknown`.** Better than guessing wrong; the matcher signature pass can resolve later.

This typically resolves 80-95% of init-shim half-residue cleanly. The remaining ~5% sit in cross-project mislabel clusters (most commonly the `highlight.js` cluster on Claude Code projects) and need npm signature matching to untangle.

### 3.4 Path organization

- Review the full app tree
- Rename only paths that improve clarity
- Group by architectural boundary first:
  - `app`
  - `features`
  - `ui`
  - `config`
  - `runtime`
  - `pkg/vendor`

## Phase 4: Completion Gate

Use severity tiers instead of demanding cosmetic perfection.
**All public-surface symbols MUST have semantic names before output generation.**

Public surface = exported symbols + owned globals + module names + module file
paths. Check with `decompile_status`: the `public_surface` field tracks
public-surface naming progress (e.g. `public_surface=2395/5609 (42.7%)`). This
ratio must reach **100%** before proceeding to output, and module paths/names
must be semantic — a tree of `modules/247-esbuild-rbr.ts` is not a named public
surface even if every symbol inside is named.

Reaching it is the job of the [Naming to target](#naming-to-target-mechanism-first-coverage)
loop — drive the metric with deterministic auto-naming first, then agent naming
for the residue. The gate below only *checks* the result; it does not produce it.

### P0 — must be zero (hard gate, blocks output)

- `missing_semantic_name == 0` (symbol-level: every exported symbol has a semantic name)
- `incomplete_decompilation == 0` (every module's export symbol is named)
- `package_attribution_unverified == 0` (every `cat:"pkg"` module has an accepted attribution contract)
- `non_existent_package == 0`
- unnamed app module query `total == 0`
- `public_surface` ratio == 100% (all public-surface symbols named)

### P0 — naming-denominator integrity (no silent exclusion)

The completion ratio is only meaningful if its denominator covers **all**
recovered first-party code. A scope-hoisting bundler (esbuild/rollup) leaves a
large block of eager top-level code that belongs to no module — the pipeline
emits it as one or more **unmodularized recovered-code files** (the "entry
island") owned by no `module_id`. That code can hold the *majority* of the
application's declarations. If the naming progress reports a small denominator
next to large generated island file(s), the denominator is excluding them and a
"100%" is fake.

The island may be emitted as a **single** `modules/entrypoint.ts` OR
**decomposed into per-cluster files** (`modules/island/cluster-*.ts`) when the
planner splits it. Both forms carry `unmodularized_recovered_code` and name
through the **same `binding-names` channel** (file-path keyed). Every one of
those files is its own naming-plan/progress group with a null `module_id`. Count
them all — a split island that lists 52 cluster groups plus the residual
`entrypoint.ts` group must contribute the union of their bindings to the
denominator, not just `entrypoint.ts`.

Before accepting the gate:

1. Confirm the naming universe includes module-less code. The naming progress
   report lists a per-file group with a null `module_id`
   (`rename_channel: "binding-names"`) for **every** unmodularized recovered-code
   file — the residual `entrypoint.ts` and each `modules/island/cluster-*.ts`.
   Each group's symbol count must be non-zero whenever that file was generated,
   and those symbols MUST be in the `full`-tier denominator.
2. If a generated island file (entry or any cluster) exists but contributes
   **zero** symbols to the denominator, that is a pipeline defect (symbol
   indexing not registering the island), not a naming-complete state. File it
   and fix the mechanism — do not proceed to output.
3. Never declare naming complete from `reached_level` alone. A `complete: true`
   over an under-counted universe is a false pass.

### Naming module-less (entry-island) code

Module-less bindings have no `symbols` row, so they are NOT named through the
module/symbol channel. They are named through the **file-path-keyed binding
channel** (`rename_channel: "binding-names"` in the naming plan/progress):
recover names for those bindings keyed by `(file_path, original_name)` and
accept them through the binding-name path. The generator applies accepted
binding names back to the same emitted file. The same precision gates apply;
do not accept low-confidence island names just to move the ratio.

### P1 — minimize aggressively, but allow explicit leftovers when justified

- `mechanical_semantic_name` (module-level must be 0; symbol/global-level may have residuals)
- `unnamed_owned_global`

P1 leftovers are acceptable only when they are clearly understood as:
- canonical runtime/bundler helpers,
- public-surface aliases that would become less readable if forced,
- cascading residuals from large init-wrapper modules with hundreds of globals,
- or low-risk residuals queued for a later naming pass.

### P2 — may remain if public surface is readable

- `missing_type_annotation`

If any P0 condition fails, do not generate output yet.

## Phase 5: Output & Verification

1. `generate_app_decompiled_files(project_id, output_dir=$OUTPUT_DIR)`
2. If generation errors occur, go back to the control loop.
3. Spot-check key modules with:
   `decompile_status(project_id, verify_module="<module_name>", output_dir=$OUTPUT_DIR)`
4. Run the **post-output structural audit** (5.1a → 5.1b → 5.1c) before
   handing off to [reverts-decompile](../reverts-decompile/SKILL.md).
   Each audit produces a list of pipeline-issue candidates; do NOT hand-edit
   the generated `.ts` to silence them. File the finding, regenerate, re-audit.

### 5.1 Post-output structural audit

Run the three structural audits summarized below before validation handoff.
Each finding is a pipeline-issue candidate that needs a regression test and
regeneration, not a generated-output hand edit. Full procedures live in
[post-output-audits.md](references/post-output-audits.md).

| Audit | Trigger | Failure signal |
|---|---|---|
| Decl-vs-import collision | Every generated output | Same name is both imported and top-level declared in one file |
| Runtime-context isolation | Multi-context profiles such as browser-extension/electron | Generated file imports from a source unit in another runtime isolate |
| Package misclassification scan | Every generated output with package imports | App-owned symbol appears as a property of `__reverts_pkg_*` |
| Oversized module file | Every generated output | A generated file exceeds the line budget (`OversizedModuleFile` audit warning, budget 10k lines) |
| Dangling named import | Every generated output | A first-party `import { Orig }` has no matching export in its target module (`DanglingNamedImport` audit **error**, blocks output) — esbuild's `No matching export`, caught in-pipeline |

Use MCP/DB-backed metadata and AST or structured parsing for these audits.
Do not replace them with grep over generated `.ts` files.

**Module-size contract.** A recovered module must be a human-readable unit — no
generated file should exceed ~10k lines. The pipeline emits an
`OversizedModuleFile` audit warning for any file over budget; treat each as a
mechanism gap, not an acceptable output. Analyze the unsplit region (an eager
entrypoint island not drained into clusters/chunks, a vendored module the matcher
left whole, an un-clustered data blob) and implement the further split in ReverTS
(Louvain island clustering, then `chain_split_eager_body` size-bounded chunks for
the residual eager body), then add a regression test and regenerate. Never accept
an oversized file by hand-editing the output.

## Phase 6: Compilation + runtime handoff

This skill stops only after structurally-valid generated output is handed to
[reverts-decompile](../reverts-decompile/SKILL.md). Install, compile, runtime,
and UI validation are mandatory completion gates owned there.

If `tsc` or runtime smoke exposes a decompile-stage bug, add a pipeline issue
and regression test, fix ReverTS, regenerate, and rerun validation. A clean
`tsc` is not done; profile-specific smoke must pass with zero browser/runtime
errors. Never hand-patch generated output to make it run.

### Execution verification is a mandatory completion gate

Decompilation is NOT done when `tsc` is clean. After the output is generated and
the structural audits pass, you MUST **really install it, really compile it,
really run it, and exercise its actual user surface** — then gate on the result:

- **Real install** — `npm install` for real: resolve every dependency and **build
  native modules** (do NOT use `--ignore-scripts` as the final state; that is only
  a temporary shortcut for an interim typecheck). For an Electron app this means a
  runnable runtime is available (a real `electron` for the app's major version, or
  the original app's bundled runtime via the drop-in below). A project that only
  "installs" with scripts disabled and natives missing is not verified.
- **Real compile** — emit JavaScript, not just typecheck: `tsc -p
  tsconfig.runtime.json` (no `--noEmit`) producing `dist/`. A project that
  typechecks but fails to emit, or emits JS that won't load, is not verified.
- **Real execution** — run the **compiled `dist/` artifact** (not a bundle of the
  source) and observe its behavior; assert it matches the original.
- **Exercise the actual user surface** — the equivalence trace is the *floor*
  (main-process / module-init smoke), NOT the whole gate. If the app has a **GUI**,
  a **web UI**, or a **CLI**, you MUST drive that surface end-to-end too (table
  below). A decompile of a GUI app is not verified by a headless trace alone.

This gate runs on **every** decompile and must be re-run after any regeneration
(naming, module renames, externalization, layout changes) — those can change
emitted code, so a previously-green run does not carry over.

#### Surface-specific e2e (in addition to the trace floor)

| App surface | Required e2e (must actually run, not stub) |
|---|---|
| **Electron GUI desktop** | Build a runnable form of the recovered main (drop-in: esbuild the recovered `dist/` main to a CJS bundle, repack it into a copy of the original `.app`'s `app.asar`, fix the asar integrity header, **strip quarantine** (`xattr -dr com.apple.quarantine <App>`), re-sign (`codesign --force --deep --sign -`), launch with macOS `open <App>` so LaunchServices spawns the renderer tree). Assert the **renderer/GPU helper processes spawn** (renderers = it is drawing) and the window **renders the expected UI**, then drive **≥1 real interaction** (the human drives keyboard/mouse — synthesizing input from the terminal types into the wrong window; capture state with `screencapture -x -o -l<windowID>`, finding the id via `CGWindowListCopyWindowInfo`). Diff against the original `.app` launched the same way. |
| **Web UI (renderer / browser bundle)** | Headless browser (Playwright/CDP): load the built UI, assert **zero console/page errors**, and perform a representative **UI interaction** (click/navigate/submit), checking the resulting DOM/state. |
| **CLI / service** | Run the **built CLI** (`node dist/cli.js …` or the `bin`) with representative arguments; assert exit codes, stdout/stderr, and any produced files for the main commands. Include `--help`/`--version` plus at least one real subcommand path. |

Pick by the same source signals as the launch-detection table below. An Electron
app typically has BOTH a main-process surface (trace floor) AND a renderer GUI
(GUI e2e) — do both. If a surface genuinely cannot run in the environment (e.g.
no display for a GUI), say so explicitly and report it as an *unverified* surface
— do not silently downgrade the gate to the trace and call it done.

### Emit a directly-runnable e2e harness beside the generated source

The decompiled project must ship its own end-to-end validation so anyone can
prove the recovered code still behaves like the original. Create it under
**`<output>/e2e/reverts/`** (the generator preserves a pre-existing `e2e/`
across regeneration) — directly runnable via `npm test`, no ReverTS install
required.

1. **Detect how the app launches from its own source — do not assume.** Read the
   generated `package.json` and entry:

   | Source signal | Profile | e2e validation (floor + REQUIRED surface) |
   |---|---|---|
   | `electron` dep, or `main`+`preload`, or an Electron start script | electron desktop | main-process equivalence trace (floor) **+ GUI drop-in launch + render/interaction** (required) |
   | `bin`/shebang, `main` → node entry, no electron | node CLI/service | trace floor **+ run the built CLI with real args, assert output/exit** (required) |
   | `manifest.json` / service worker | browser-extension | Playwright/CDP load + console-error checks **+ a real UI interaction** (required) |
   | renderer `index.html` + bundle | web | headless browser load + console-error checks **+ a real UI interaction** (required) |

2. **For a main-process / Node entry, generate the equivalence-trace harness.**
   A full GUI launch is usually infeasible headless (display, sign-in, helper
   processes), so the runnable gate is the **original-vs-recovered interaction
   trace**: run the recovered main AND the original bundle under one identical
   instrumented `Module._load` stub (electron main model — `ipcRenderer`
   undefined, string-returning `app.getPath`/`getName`, `whenReady` resolves,
   `process.versions.electron` defined; native `.node` and externalized packages
   stubbed) and compare the **interaction multiset** + error set. The runner
   (`run.mjs`) must: (a) **compile** — `tsc -p tsconfig.runtime.json` emitting
   `dist/` (NOT `--noEmit`), and fail if the built entry is missing; (b) bundle
   the **compiled `dist/` entry** (not the source `.ts`) to CJS — wrap the
   top-level `await`, restore `import.meta.url` via banner, externalize every
   native/3rd-party dep — so the **actual build output executes**; (c) run/trace
   both programs; (d) assert **recovered ⊇ reference** — no original interaction
   is dropped or under-run, the recovered main loads without error, and the only
   extras are benign idempotent builtin `require`s introduced by modular
   re-emission (split clusters + ESM entry add a few `require('url')`/
   `require('module')` for the `import.meta` shim). Exit non-zero on any dropped
   interaction, any non-`require` extra, or a recovered load error. Keep the
   original main bundle at `e2e/reverts/reference/main-bundle.cjs` (it must use
   `.cjs`; the harness package is `type: module`). Offer `--no-build` only to
   skip the (slow) recompile when `dist/` is already fresh — the default path
   always compiles.

   **Derive everything from the project — hardcode nothing app-specific** (so the
   same harness works for any decompiled app): the **source root** from
   `tsconfig.runtime.json` `rootDir`; the **entry binding + module** by parsing
   the generated `cli.ts` (`import { <entry> } from '<module>.js'`); the
   **packages to stub** from the app `package.json`
   `dependencies`/`optionalDependencies` (every externalized package) — any other
   unresolved bare `require` still records the boundary instead of crashing. Make
   the Electron version and any extra stubs overridable by env
   (`ELECTRON_VERSION`, `REVERTS_STUB_PACKAGES`). A Claude/ws/`@scope`-specific
   list baked into the harness is a bug.

3. **Run it and gate on it.** `cd <output>/e2e/reverts && npm install && npm
   test`. A failing trace diff is a decompile-stage bug — file it, fix ReverTS,
   regenerate, re-run. Do not relax the harness to make it pass.

The trace floor above is necessary but NOT sufficient for a GUI/web/CLI app: it
verifies the main process, not the user-facing surface. The GUI drop-in (repack
`app.asar`, fix asar-integrity header, re-sign, launch with macOS `open <App>` so
LaunchServices spawns the renderer tree) — and the web/CLI equivalents in the
surface-e2e table above — are **required**, not optional, whenever that surface
exists. The drop-in procedure is documented in
[reverts-decompile](../reverts-decompile/references/runtime-validation-profiles.md).
It needs a real display and human-driven interaction; if the environment cannot
provide that, report the GUI surface as **unverified** rather than claiming the
decompile is done.

#### GUI drop-in troubleshooting (macOS)

| Symptom | Cause | Fix |
|---|---|---|
| Dialog *"Apple could not verify '<App>' is free of malware"* / *"Apple 无法验证…"*; `open` exits 0 but **no processes** survive | The modified app lost notarization and still carries the **quarantine** xattr (copied from a DMG/download), so Gatekeeper hard-blocks it | `xattr -dr com.apple.quarantine <App>` then re-`codesign --force --deep --sign -`; relaunch. (Do NOT disable Gatekeeper globally.) |
| Only the **main** process runs, no `Renderer`/`GPU` helpers, blank/no window | Drop-in main threw before `createBrowserWindow`, OR you launched the binary directly instead of via `open` | Launch with `open <App>` (only LaunchServices spawns renderers). If still main-only, capture the main's error from the unified log (`log show --last 40s --predicate 'process == "<App>"'`) — it is a recovery bug to fix in-pipeline. |
| *"app is damaged"* / integrity error at launch | asar integrity hash in `Info.plist` does not match the repacked `app.asar` | Recompute `sha256` of `getRawHeader(app.asar).headerString` and write it to `ElectronAsarIntegrity.'Resources/app.asar'.hash` (use `PlistBuddy`, not `plutil` — the key contains a dot), then re-sign. |

Confirm success by the **process tree** (main + `Helper (GPU)` + several
`Helper (Renderer)`) and a window **screenshot** (`screencapture -x -o
-l<windowID>`), then have the human drive one interaction.

### Write the project README (skill-owned, not generated)

The generator does NOT emit `README.md` — it carries decompilation provenance and
verification results that only the agent knows, so **write `<output>/README.md`
yourself** after the e2e gate passes (the generator leaves an existing one
untouched on regeneration, like `e2e/`). Build it from the *actual* generated
project, not a template:

- **Provenance** — state plainly that the project was produced by decompiling a
  JavaScript bundle with ReverTS, and is a behavior-equivalent reconstruction,
  not the original source; do not hand-edit recovered files.
- **Layout** — describe the real tree you generated: the source root, the entry,
  `modules/<domain>/…` (the semantic domains you actually named), the entry
  island (`modules/entrypoint.ts`) and its `modules/island/cluster-*.ts` split,
  `dist/`, `.reverts/`, `e2e/`. Use the counts/domains that are actually present.
- **Externalized packages** — list the third-party packages that were matched and
  externalized (the `dependencies` in the generated `package.json`).
- **Verification performed** — record what you actually ran and the result:
  `tsc` compile to `dist/`, real execution of the compiled main, and the
  original-vs-recovered equivalence trace (quote the numbers, e.g. "recovered N
  ⊇ reference M, 0 dropped, load-clean"). Point readers at `e2e/reverts` to
  reproduce.

Keep it accurate to this project; never copy stale numbers or domains from
another decompile.

## Guardrails

Do not hand-edit generated output, generated manifests, imports, exports, or
stubs as the final fix. This skill owns semantic naming, output generation, and
structural audits; install/compile/runtime validation belongs to
[reverts-decompile](../reverts-decompile/SKILL.md). See
[guardrails.md](references/guardrails.md) for the detailed out-of-scope list and
common mistakes.

## Tool Summary

| Tool | Purpose |
|------|---------|
| `decompile_status` | progress, diagnostics, verification |
| `query` | module/symbol search; AST-backed misclassification scan |
| `get_module` | metadata, symbols, dependencies |
| `get_source` | source inspection |
| `update_modules` | module path/category/package updates |
| `verify_package_attributions` | deterministically accept/reject LLM package attribution proposals against installed `node_modules` |
| `submit_module_decompilation` | symbol/global/type naming |
| `detect_runtime_helpers` | helper discovery |
| `submit_runtime_helpers` | helper confirmation |
| `generate_app_decompiled_files` | output generation |
