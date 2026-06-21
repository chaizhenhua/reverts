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
  user when an input is missing or when a hard blocker is hit.
- The main agent is an orchestrator: status → dispatch → re-check status.
- Default to `cat:"app"`. Use `cat:"pkg"` only with exact npm package identity,
  npm-installable version (e.g. `"4.28.1"`, a concrete semver that
  `npm install pkg@ver` can resolve), and clear upstream-source confidence.
  A package classification is not complete until `update_modules` records
  `emit`, `subpath`/`specifier`, and evidence, then `verify_package_attributions`
  accepts it against the installed `node_modules` tree.
- **All public-surface symbols must have semantic names before output.** Public
  surface = exported symbols + owned globals. The `public_surface` ratio in
  `decompile_status` must reach 100%.
- Prioritize public surface first: exported symbols, owned globals,
  constructor/state fields, internal helpers, locals last.
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
| unnamed app modules > 0 | classify + name agents (combined) |
| `incomplete_decompilation > 0` | symbol naming agents for incomplete modules |
| `missing_semantic_name > 0` | symbol naming agents |
| `mechanical_semantic_name > 0` (module-level) | mechanical fix agents |
| `mechanical_semantic_name > 0` (symbol/global-level) | mechanical fix agents |
| `package_attribution_unverified > 0` | package attribution correction + verification agents |
| `non_existent_package > 0` | package reclassification agents |
| `public_surface` ratio < 100% | symbol naming agents for modules with unnamed public-surface symbols |
| path organization not reviewed | path organization agents |
| otherwise | proceed to output |

### Progress line

After each check, report:

```text
Status: public_surface={named}/{total} ({pct}%) | {unnamed} unnamed | {incomplete} incomplete | {mechanical} mechanical | {missing} missing -> dispatching {N} agents
```

The `public_surface` field (from `decompile_status`) tracks public-surface symbol naming progress and must reach 100% before output generation.

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

Public surface = exported symbols + owned globals + module semantic names.
Check with `decompile_status`: the `public_surface` field tracks public-surface naming progress
(e.g. `public_surface=2395/5609 (42.7%)`). This ratio must reach **100%** before proceeding to output.

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
emits it as a single **unmodularized recovered-code file** (an "entry island")
owned by no `module_id`. That file can hold the *majority* of the application's
declarations. If the naming progress reports a small denominator next to a large
generated entry-island file, the denominator is excluding it and a "100%" is
fake.

Before accepting the gate:

1. Confirm the naming universe includes module-less code. The naming progress
   report lists a per-file group with a null `module_id`
   (`rename_channel: "binding-names"`) for every unmodularized recovered-code
   file. Its symbol count must be non-zero whenever such a file was generated,
   and those symbols MUST be in the `full`-tier denominator.
2. If a generated entry-island file exists but contributes **zero** symbols to
   the denominator, that is a pipeline defect (symbol indexing not registering
   the island), not a naming-complete state. File it and fix the mechanism — do
   not proceed to output.
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

Use MCP/DB-backed metadata and AST or structured parsing for these audits.
Do not replace them with grep over generated `.ts` files.

## Phase 6: Compilation + runtime handoff

This skill stops only after structurally-valid generated output is handed to
[reverts-decompile](../reverts-decompile/SKILL.md). Install, compile, runtime,
and UI validation are mandatory completion gates owned there.

If `tsc` or runtime smoke exposes a decompile-stage bug, add a pipeline issue
and regression test, fix ReverTS, regenerate, and rerun validation. A clean
`tsc` is not done; profile-specific smoke must pass with zero browser/runtime
errors. Never hand-patch generated output to make it run.

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
