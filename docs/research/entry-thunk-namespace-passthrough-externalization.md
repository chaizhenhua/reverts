# Externalizing esbuild package-entry thunks via whole-namespace passthrough

Recorded 2026-06-08. This is the implementable path to making cc-2.1.89's
bundled third-party packages (react, semver, scheduler, picomatch) externalize
to real `import … from "pkg"` — **without** solving the per-fragment re-minified
recognition floor that earlier notes ([[rollup-oracle-vs-planner-adapter-gap]],
`project_externalization_status` memory) treated as a hard blocker.

## The key reframing: the unit is the ENTRY THUNK, not the fragments

The matcher attributes **2435 modules** to react in cc-2.1.89. That number is a
trap: react's entire implementation lives in **one self-contained module**, and
the other ~2434 are `dependency_closure_ownership` neighborhood NOISE (37% of all
dependency edges are package-boundary-violating — e.g. `semver→react` 1028 edges,
`pkg→first-party` 3422 edges, both impossible for real packages).

Measured on `/tmp/cc89-ext7.sqlite` for `971-esbuild-Z6.ts`:

| metric | value |
|---|---|
| outgoing deps | **0** (fully self-contained IIFE closure) |
| incoming readers | 640 |
| function-signature matches vs `react@19.2.0/cjs/react.development.js` | **1288** |
| string-anchor matches | 0 |
| match strategy | `aggregate_function_signature_and_string_anchors` |
| source | `var Z6=(()=>{ let _$cached; return ()=>{…_$module.exports…} })()`, 11 KB, body holds `Symbol.for('react.transitional.element')` + all `useState/useEffect` defs |

So module 971 IS react. The 1288-function match is overwhelming; anchors=0 and
the aggregate strategy are why every current gate refuses it.

## Why per-member externalization is impossible here, and why it doesn't matter

esbuild `--minify` STRIPPED the interop helpers to short names — there are **0**
literal `__export(` / `__toESM` / `__commonJS` tokens in the 13 MB bundle. The
public-surface map `__export(exports,{useState:()=>…})` that the member-proof
path needs is **unrecoverable**.

BUT consumers never read react's internal bindings. They read the **namespace**:
`CG.useState` (661×), `VX_.useState`, where `CG=O6(Z6())` is the minified form of
`CG=__toESM(require_react())`. The internal symbols (`Ty1`, `AX_`, …) are sealed
inside the IIFE closure and never escape.

Therefore the sound externalization is **whole-namespace passthrough**, which
needs no per-member mapping:

```ts
// 971-esbuild-Z6.ts, externalized:
import * as __react from "react";
const Z6 = () => __react;          // was: () => { …11 KB react impl… }
export { Z6 };
```

Every `Z6().useState` now resolves to `__react.useState`. The 11 KB impl drops.

## Exactly what blocks it today (file:line)

1. **Matcher never accepts 971.** `public_export_member_external_package_source`
   (`reverts-package-matcher/src/proof/export_member.rs:210`) bails because
   `semantic_external_target_policies` (`proof/policy.rs:125`) returns empty for
   `AggregateFunctionSignatureAndStringAnchors`. So no accepted external
   attribution is ever written for 971.

2. **Planner CommonJsWrapper detection ALREADY fires for 971.**
   `external_package_adapter_kind` (`reverts-planner/src/external_adapters.rs`)
   returns `CommonJsWrapper` when the source contains `let_$cached;` +
   `return_$module.exports;` — exactly 971's shape. The detection side is done.

3. **But the adapter bails on unproven named exports.**
   `build_external_package_adapters` (`external_adapters.rs:229`): for a
   `CommonJsWrapper` with `member_proof.is_none() && !has_semantic_path_proof &&
   commonjs_wrapper_source_has_unproven_named_exports(...)` → returns `None` →
   the module stays full source. react's wrapper has named exports and no member
   proof, so it bails. This gate is correct for the per-member case but
   **over-conservative for namespace passthrough**, where named members are
   served by the imported namespace by construction.

## The planner side already works (proven by an existing test)

`reverts-planner` test `anonymous_bundle_external_attribution_uses_external_adapter`
emits a bare import and drops the body for a CJS-wrapper module
(`var packageThing=(()=>{let _$cached;return ()=>{…_$module.exports…}})()`,
body contains `exports.answer=42`) given an accepted attribution whose
`resolved_file = "forced-external:export-members:source-equivalent:packageThing:…"`
— i.e. a member proof on the THUNK BINDING itself. The CommonJsWrapper adapter's
return expression is already a namespace passthrough
(`<ns>.default ?? <ns>`). **So the planner needs NO change** — it already emits
exactly the form we want, keyed off an attribution carrying a member proof on
the thunk binding.

## The sole remaining gap is matcher-side, precisely scoped

Neither existing matcher pass produces that attribution for cc-2.1.89's thunks:
- `ownership/force_externalize.rs` requires `module.kind == ModuleKind::Package`
  + a `package_name`; cc-2.1.89 thunks are anonymous `Application` modules. Skipped.
- `ownership/importable.rs::promote_anonymous_bundle_external_imports` runs the
  member-proof resolver, which requires the module's EXPORTED MEMBERS ⊆ the
  package's PUBLIC members. A thunk exports `Z6` (the function), not `useState`,
  so `{Z6} ⊄ {useState,…}` → None.

Hard sub-problem: the matcher reads **raw esbuild-minified** source (it runs
before reverts normalizes the wrapper to the `let _$cached` form the planner
detects). So matcher-side detection must recognize esbuild's minified
`__commonJS` entry shape (`var Z6=<minHelper>((exports,module)=>{…})`) in raw
bytes, not the emitted form.

## Implementation plan (revised — matcher only)

### Step 1 — Matcher: accept self-contained CJS-wrapper entry thunks
New acceptance (own pass/module under `ownership/`), gated hard on ALL of:
- module is an esbuild CommonJS-wrapper thunk (`let _$cached; return _$module.exports;`);
- **zero (or only intra-package) outgoing module dependencies** — self-contained;
- overwhelming function-signature evidence against a single resolved package
  file (e.g. `function_signature_matches >= max(K, 0.6 * module_function_count)`
  AND a clear runner-up gap), reusing the `anonymous-function-axis-source`
  resolved_file already on the attribution;
- consumers reference it via **namespace access only** (no consumer destructures
  a non-exported internal binding of the thunk — checkable from the def-use /
  candidate-reads graph).

Emit an accepted external attribution carrying a new **NamespacePassthrough**
proof kind (distinct from member proof) + `export_specifier = "<package>"`.

### Step 2 — Planner: namespace-passthrough adapter emission
Teach `build_external_package_adapters` that a `CommonJsWrapper` carrying the
NamespacePassthrough proof does NOT need per-member proof: emit
`import * as __ns from "<pkg>"; <wrapper> = () => __ns` and replace the body.
Relax `commonjs_wrapper_source_has_unproven_named_exports` ONLY for this proof
kind. The existing `adapter_required_package_modules` fixpoint stays the safety
net for any consumer that reaches a non-namespace internal (keeps that thunk
source-preserved).

### Step 3 — Drop the neighborhood noise
Do NOT externalize the `dependency_closure_ownership` attributions (the 2434
react-labelled fragments etc.) — they are misattributed. Only entry thunks
externalize; the fragments that become unreferenced once the thunk is an import
are dead-eliminated, the rest stay first-party/package-owned source.

### Step 4 — Validate end-to-end (no-fallbacks)
Regenerate cc-2.1.89; assert `import … from "react"` (and semver/scheduler/
picomatch) appear; assert source-size drop; `tsc -p tsconfig.runtime.json` → 0
errors; `node dist/cli.js --version` and `-p "say hi"` work. Run golden-e2e +
workspace tests + clippy. A single consumer reaching a non-namespace internal
must keep the thunk inlined, never silently break.

## Per-package entry thunks (cc-2.1.89, from cc89-ext7)

| pkg | entry module | fn matches | out-deps | readers |
|---|---|---|---|---|
| react | 971 (esbuild:Z6) | 1288 | 0 | 640 |
| semver | 696 / 1004 | 629 | 7 | 11 |
| picomatch | 1231 | 562 | 9 | 1 |
| scheduler | (entry) | — | — | — |

The safety precondition (low/zero out-deps + namespace-only consumers) holds
cleanly for react; semver/picomatch have a few out-deps to validate per Step 1.

## Status — IMPLEMENTED (matcher-only)

Implemented as `reverts-package-matcher/src/ownership/cjs_wrapper_entry.rs`
(`CjsWrapperEntryThunkPass`, wired after `AnonymousImportablePass`). The planner
needed NO change — it already emits the namespace passthrough for a
CommonJsWrapper module carrying a semantic-path proof.

The pass promotes an anonymous, self-contained (0 outgoing module deps) esbuild
`__commonJS` entry thunk to an accepted external import with a
`forced-external:semantic-path:<pkg>@<ver>/index.js` proof, which makes the
planner emit `import * as ns from "<pkg>"; function <thunk>() { return ns; }` and
drop the body.

**Identity gate (the critical safety fix).** Function-signature attribution is
promiscuous: cc-2.1.89's ajv-codegen thunk (`<ex>.regexpCode=…,<ex>.str=…`)
function-matched react and, on a naive count-only gate, externalized as react →
`react.Name is not a constructor` at runtime. The airtight gate is
PUBLIC-SURFACE IDENTITY: the thunk must assign ≥4 of the matched package's REAL
public export names onto its exports object (`<exports>.<member>=`). A real react
entry assigns `useState`/`useEffect`/…; the ajv thunk assigns none of react's
members and is rejected. Package members are read from the loaded package source
(`ExternalImportSourceIndex::export_members`), precomputed once per (pkg,ver).

Measured first cut (before the identity gate): 34 entry thunks externalized
(react 23, picomatch 7, semver 4), react module 971 11 KB → 112 B, `tsc -p
tsconfig.runtime.json` 0 errors — but `node dist/cli.js` hit the ajv-as-react
misattribution. The identity gate fixes that class of false externalization.
Remaining: confirm the gated set runs end-to-end (`--version`, `-p`) and does not
regress the working node_modules-path externalization (Claude.dmg `ws`).
