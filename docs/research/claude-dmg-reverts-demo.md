# Claude.dmg Reverts decompile demo (agent-driven)

This note records the **agent-driven** demo for Reverts as a decompilation skill. The
agent (Claude Code / Codex) *leads* every decision gate; the Reverts CLI is the
deterministic substrate the agent calls, and scripts only orchestrate repeatable
commands. The agent owns target triage, package judgment, **semantic naming**, and
validation — it never hand-edits generated output or the SQLite DB.

- Target: `/home/chaizhenhua/Downloads/Claude.dmg` (294 MB Electron app)
- Reference source tree (third-party identification only): `Codes/claude-code/src`

## Demo prompt (headline)

```text
Use the Reverts decompile skill. Decompile this DMG automatically, then read the
recovered code and give the minified module symbols real names:

/home/chaizhenhua/Downloads/Claude.dmg

Use this reference tree only to identify third-party packages (not to recover
first-party code):

/home/chaizhenhua/Codes/claude-code/src
```

The reference tree is used **only** for third-party package identification. First-party
structure is recovered from the bundle alone — the demo does not "cheat" by reading the
app's own source for first-party code.

## Agent-driven flow

The agent leads each gate and inspects results before proceeding. CLI commands are the
deterministic substrate (skill: `reverts-decompile`; unpack delegated to
`auto-unpack-target` → `unpack-electron-app`).

1. **Detect** target type → `electron-dmg`, confidence `1.0`. (agent decision)
2. **Unpack** the `.app` / `app.asar`; produce `reverts-import-evidence.json`. (delegated skill)
3. **Import** facts → `project.sqlite` (`import-unpacked`).
4. **Discover** package candidates from unpacked `node_modules`/manifests **and**
   reference-source imports. (agent triage — discovery precedes matching)
5. **Match + classify**: `module-classify --auto --apply`, fingerprint-based
   `match-packages --apply`; apply `package-surface-decisions` only through TSV gates.
   Safe public surfaces externalize; internal helpers stay package-owned. (agent judgment)
6. **Generate** the TypeScript project (`generate-project-v2 --output output`).
7. **Name (agent loop, the core step).** The agent reads each emitted module
   (imports, exports, call sites, members, string literals) and proposes semantic names
   for the minified worklist. Names are applied through `symbol-names --batch --apply`
   under an **evidence gate** (every name token must be justified by code-grounded
   evidence or the technical vocabulary — hallucinated tokens are rejected). After a
   batch: regenerate, re-check `naming-progress` (reuse `output/symbol-index.json` so the
   loop does not re-emit just to read a worklist).
8. **Validate**: `tsc` compile, then runtime smoke where the app type allows; record
   `full-inventory` / `identifier-inventory` / `coverage-ledger`.
9. **Report**: `decompile-session.json`, `reverts-decompile-report.json`, worklists.

## Validated run (real numbers, release binary)

### Structure recovery

```text
output/
  Contents/   59 files     (.app bundle metadata)
  assets/     960 files     (web/static assets)
  modules/    148 files     (structured TS modules — the decompiled application)
  scripts/    1 file
  sources/    2114 files    (raw recovered source files behind the modules)
Total: 3288 files
```

`modules/` are the structured, import-wired TS modules (the readable decompile);
`sources/` are the raw recovered source files those modules are projected from.

### Agent semantic-naming loop (the headline proof)

Before — module symbols are minified, `named = 0`:

```text
naming-progress: named=0  total=308  pending=300+  complete=false
```

The agent read two SDK modules and named them from code (gate-validated, code-grounded
evidence). Module `1075` is the Anthropic **Stream** module; module `1079` is the
**multipart upload** module:

| module | original | agent-assigned name |
|--------|----------|---------------------|
| 1075   | `vF`     | `Stream` (the SSE/ReadableStream class) |
| 1075   | `H1`     | `streamState` (private WeakMap) |
| 1079   | `Jlt`    | `validateFileGlobal` |
| 1079   | `Klt`    | `isAsyncIterable` |
| 1079   | `T3A`    | `toMultipartRequest` |
| 1079   | `TBr`    | `createFormData` |
| 1079   | `vBr`    | `isNamedBlob` |
| 1079   | `vLA`    | `appendFormDataValue` |

`symbol-names --batch --apply` → "8 change(s) written" (round 1).

**Round 2 — first-party git integration (module 1166).** The agent then named Claude's
own git module: `findGitRoot`, `findCanonicalGitRoot`, `getBranch`, `getDefaultBranch`,
`getRemoteUrl`, `getIsHeadOnRemote`, `hasUnpushedCommits`, `getIsClean`,
`getChangedFiles`, `getWorktreeCount`, `stashToCleanState`, the sentinels
`gitRootNotFound` / `remoteSlugNotFound`, and the memoized `cachedFindGitRoot` /
`cachedRemoteUrl` (23 accepted).

**Round 3 — classify bundled third-party out of the denominator.** Rather than
mislabel bundled libraries as first-party, the agent classified them third-party
(`module-classify --batch --apply`), removing them from the first-party naming
denominator (source is still emitted — classification never forces a bare import):

| module | library | targets removed |
|--------|---------|-----------------|
| 75, 231 | mermaid (state + ER diagram) | 21 + 10 |
| 158 | d3-color | 82 |
| 280 | monaco-editor 0.52.2 / d3 / react | 3 |
| 1137 | signal-exit | 29 |
| 1163 | lru-cache | 85 |

Across the rounds the headline climbs — naming real first-party symbols **and** shrinking
the denominator to genuine first-party code:

**Round 4 — the bundler-init tail.** Most of the remaining 47 targets are esbuild
`__esm` init-thunks (`Flt`, `x3`, `K3A`, … — lazy module-init wrappers) plus opaque
hoisted state. The agent named the init pattern honestly (`initStreamModule`,
`initUploadsModule`, `initGitModule`, `initCliModule`, generic `initModule`), the esbuild
memoizer (`_$l` → `lazyModuleInit`), `gitExe`, and the multipart `TLA` → `multipartCache`
(14 accepted).

```text
round 0 (baseline):       named=0   / 308   (0%)
round 1 (SDK names):      named=8   / 308   (3%)
round 2 (git names):      named=30  / 307   (10%)
round 3 (3p classified):  named=30  / 77    (39%)   pending=47, modules=11
round 4 (init-thunk tail): named=44 / 77    (57%)   pending=33
```

(230 third-party targets left the denominator; the count nudges by one as accepted names
re-classify the emitted universe.)

**Honest ceiling — and what per-symbol data-flow tracing reveals.** Tracing each of the
remaining 33 targets by its def/use (the `DefUseGraph` / `ResolvedSymbolGraph` signal:
reads, writes, member-access) shows they split into two groups:

- **~24 (≈73%) are dead esbuild-hoisted bindings.** Module 1166's `eke`/`tke`/`ike`/`Gfr`/
  `q3A` appear *exactly once* (declaration only, unexported) — definitively unused; the
  single-letter module hoists (`A`/`i`/`n`/`o`/`s`, `e`/`r`, `type`, …) are *never assigned
  at module scope* — they are shadowed by function-local `let`s of the same name. These
  have `read_count == 0 && write_count == 0`: exactly the `ResolvedSymbolGraph::unread_bindings`
  / `is_unread()` signal. They have no semantic role to recover; the right action is to
  **exclude them from the naming denominator** (or drop them as dead code), not to fabricate
  names.
- **~9 are live module state** (e.g. `sG = new Map()`, `vAA`/`lut = []`, exported
  constants `1000`/`60`/`100`). Naming these *correctly* needs cross-module alias tracing
  (who reads/writes them under their re-imported names) — exactly what the
  `ImportExportGraph` + alias-closure automates and what a by-hand pass cannot do reliably.

So the data-flow verdict is **not "name them all"** — it is "≈73% are dead and should leave
the worklist; the rest need the cross-module def-use closure." Wiring
`unread_bindings`/dead-binding detection into `naming-plan` would drop the dead targets
(denominator `77 → ~53`, coverage `57% → ~83%`) *correctly* — the proper next pipeline
change, and the real consumer of the P0–P2 graph work. The agent does **not** mechanically
rename dead code to reach 100%; a wrong name is worse than an honest exclusion.

The names land in the regenerated output and propagate to every call site, e.g. module
1079 now reads:

```ts
isAsyncIterable = (e) => e != null && typeof e == 'object'
  && typeof e[Symbol.asyncIterator] == 'function',
toMultipartRequest = async (e, A, t = true) => ({ ...e, body: await createFormData(e.body, A, t) }),
createFormData = async (e, A, t = true) => { /* ... */
  return await Promise.all(Object.entries(e || {}).map(([r, n]) => appendFormDataValue(i, r, n, t))), i; },
isNamedBlob = (e) => e instanceof Blob && 'name' in e,
```

The evidence gate is real: an early `streamPrivateState` proposal was **rejected**
("token `private` absent from evidence/technical vocabulary"); the agent re-grounded the
name to `streamState`. This is the guardrail that keeps agent naming honest.

> Scope: 30/307 is a validated demonstration proving the loop scales across rounds with
> *correct* first-party names. Full completion is the same loop continued — but see the
> finding below: most of the remaining worklist is bundled third-party code, so the right
> next lever is classification/externalization, not more first-party names.

### Finding: the worklist is dominated by bundled third-party libraries

Inspecting the worklist showed that the "first-party" naming targets are largely
third-party libraries bundled (and minified) into the app, which package-matching did not
externalize. The agent should **not** first-party-name these (per the skill: do not
rename third-party modules just to make them readable):

| where | actually is | correct handling |
|-------|-------------|------------------|
| public-surface 27 (modules 75/158/231/280, `ion-dist/assets`) | d3-color, monaco-editor 0.52.2, React icon components | classify third-party / externalize |
| module 1137 | `signal-exit` (npm) | classify third-party |
| module 1163 (85 targets) | `lru-cache` (npm) | classify third-party |
| modules 1075/1079 | `@anthropic-ai/sdk` (stream, multipart) | source-preserved; readability naming optional |
| module 1166 | **Claude's own git integration** | **first-party — named (round 2)** |

So the headline moves on two correct levers, **both exercised** (round 2 and round 3
above): (1) name the genuinely first-party application modules (git integration), and
(2) classify the bundled libs (mermaid/d3/monaco/lru-cache/signal-exit) out of the
first-party denominator. Force-naming the web-lib exports as first-party would be wrong
and is intentionally avoided; the result — `30/77 = 39%` — is honest.

### Compile

```text
tsc -p tsconfig.runtime.json --noEmit  →  exit 0, 0 errors
```

The regenerated project — including the agent's renames — type/syntax-checks clean
(`@ts-nocheck` suppresses inferred-type noise; 0 syntax errors confirms the rename and
emit are structurally valid).

### Worklist-reuse equivalence (fast loop)

`naming-progress` via `--symbol-index output/symbol-index.json` (reuse) and via full
re-emit both report `named=8` — identical. The reuse path lets the agent loop re-read the
worklist without paying for a re-emit each iteration.

## Third-party handling

Lead with a clean externalization, then the safe-by-default guardrail:

```json
{ "ws": "8.18.3",            // externalized: fingerprint-matched exact version
  "node-pty": "1.1.0-beta34",
  "@ant/claude-native": "0.0.0", "@ant/claude-swift": "1.0.0" }  // native, version unresolved
```

- `ws@8.18.3` — matched and externalized (the package matcher working).
- Modules `1015/1016/1017` matched `node-pty` but the evidence did not prove a *safe
  single external import*, so they are **kept as recovered source**, not forced into an
  unsafe import, and are **excluded from the first-party naming denominator** (confirmed
  absent from `naming-plan`). This is the guardrail, not a gap: Reverts never fabricates
  an unsafe import.

The denominator reduction is the user-visible win: package-owned and bundled third-party
code is removed, taking the first-party worklist from `target_count: 308 / 17 modules` to
`47 / 11 modules` after the round-3 classification — a small, bounded surface the agent can
finish naming.

## Demo proof points (live)

1. The small natural-language prompt (one host: Claude Code *or* Codex).
2. Auto-detect `electron-dmg`, confidence `1.0`.
3. **Before/after money shot**: original `cli.js` (single-line minified) vs a recovered
   `modules/*.ts` with real imports/structure; then `vF`→`Stream`, `vLA`→`appendFormDataValue`.
4. The agent naming loop across rounds: `named` climbing `0 → 8 → 30 → 44` (57% of the
   bounded first-party set), the evidence gate rejecting an unjustified token, the agent
   *declining* to first-party-name bundled third-party libs (d3/monaco/lru-cache), and
   *declining* to guess opaque bundler internals rather than gaming the count to 100%.
5. `tsc` exit 0 on the renamed output.
6. `ws@8.18.3` externalized; `1015/1016/1017` kept as source and absent from the
   first-party plan (safe-by-default).

## Status

The agent-driven flow is validated end to end: detect → unpack → import → discover →
match → generate → **agent-name (0 → 8 → 30 → 44, gate-enforced)** → **classify bundled
third-party out (denominator 307 → 77)** → **compile (tsc exit 0)**. First-party naming
coverage is **57% (44/77)** with correct, gate-grounded names; the remaining 33 are opaque
bundler internals deliberately left `pending` rather than guessed. Remaining work:
(1) per-symbol data-flow tracing to name the opaque tail, (2) fingerprint exact versions
for the classified libs to fully externalize them (not just denominator exclusion), and
(3) an Electron runtime smoke (heavier: needs Electron + display; tracked separately).
