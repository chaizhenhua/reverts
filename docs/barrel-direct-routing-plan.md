# Entrypoint-island barrel direct-routing plan

## Problem

The recovered project has one giant re-export barrel at `modules/entrypoint.ts`
(the "entrypoint island hub"). After the chain-split drains the eager island into
per-cluster files, the hub is left as a star-topology aggregator: ~1630
`import { … } from './owner.js'` back-imports + a ~2431-name `export { … }`
re-export wall, with only 3 lines of real code (the ESM↔CJS shims). ~465 consumer
files import island bindings *through the hub* (`consumer → hub → owner`) instead
of directly (`consumer → owner`).

This hurts readability two ways: the hub itself is an unreadable 1600-line wall,
and every consumer's imports point at the opaque hub instead of the file that
actually defines the binding.

## Why this belongs in the planner (not the emitter)

Per **ADR 0005**, the planner is the *decision* layer (it already does
cluster→cluster direct routing, runtime-barrel rerouting, and export completion);
the emitter is a *rendering* layer that must accept a typed, pre-decided plan
(**ADR 0002**, `plan.rs`). Import-graph routing is a graph decision, so it stays
in `reverts-planner`. Doing the reroute in the emitter would leak domain logic
into the rendering context and bloat it — rejected.

## Root cause that blocked the first attempt

A file's *export surface* is represented **three ways** in the planner, which
violates single-source-of-truth and made an ad-hoc owner map miss most bindings:

1. structured `PlannedFile.exports` (the planner's own unaliased emissions);
2. **body text** `export { local as Wire };` appended by the completion passes
   (these carry the wire-name aliases — and the plain-export parser rejects `as`);
3. import-backs the **emitter synthesises at render time** from (1)+(2), so they
   are absent from the hub's plan body entirely.

Each owner-map source fixed in isolation exposed the next gap. The fix is to stop
scanning ad-hoc and introduce ONE canonical projection.

## Design

### 1. `BindingOwnerIndex` — a read-model (domain service)

A pure projection over the plan: `BindingOwnerIndex::from_plan(&plan)` →
`binding (exported name) → unique owner file path`.

- Collects from BOTH structured `exports` AND body-text `export { … };`
  statements, normalising aliases to the **exported** name (the name consumers
  import), since the hub re-exports owners transparently.
- Ignores `export { … } from '…'` re-export statements (the file does not own
  those) and the hub file itself.
- Excludes eager-ordered chunk files (see below).
- A name exported by more than one eligible file is *ambiguous* → no owner →
  never rerouted (safe).

This index is the single seam. Today it reads both representations as an
**anti-corruption layer**; the long-term convergence is to make the completion
passes write structured `add_export` so `exports` becomes the lone truth and the
index reads only it. Migrating completion passes is incremental and does not
change the index's callers.

### 2. `eager_ordered_chunk` marker (domain attribute)

`PlannedFile.eager_ordered_chunk: bool`, set on chain-split eager-body chunk
files. A chunk holds order-sensitive eager statements the entry loads in source
order; a consumer importing a chunk binding directly could run those statements
out of order. Cluster files (drained functions/classes/pure consts) are
side-effect-free and safe. The index excludes chunks, so they always stay on the
hub.

### 3. `reroute_entrypoint_island_barrel` — a thin pass

Runs after the import/export-completion passes (final settled graph), before
`flag_wire_safe_export_renames`. For every non-hub file, rewrite each
`import { … } from '<hub>'` line: repoint each specifier whose name the index
owns to a direct import from that owner; leave the rest on the hub. No
bundle-scope guard is needed — every rerouted binding is one the consumer already
imports externally from the (main-bundle) hub, so pointing it at its real owner
re-internalises nothing into the consumer's bundle.

Hub shrink follows naturally: once consumers no longer import a binding from the
hub, the emitter has no reason to synthesise the hub's re-export/back-import for
it, so the wall collapses to what genuinely remains hub-resident (ambiguous,
chunk-owned, or the entry callee).

## Testing (ADR 0003: self-contained)

Unit tests build minimal in-memory `EmitPlan`s — no DB, no external programs:

- `BindingOwnerIndex`: structured-only export, body-text aliased export
  (`a as Wire`), ambiguous (two owners → none), re-export-from ignored, chunk
  excluded.
- reroute pass: consumer hub-import → direct owner import; mixed line (some
  owned, some not) splits correctly; chunk-owned binding stays on hub; no
  reroute when no owner; idempotent on a second run.

## Scope and current result (phase 1)

The pass reroutes the **side-effect-free** subset and is correct/safe for it:
on the Claude app decompile, consumer hub-imports drop 465 → 349 (≈116 files now
import their bindings directly), every relative import resolves (0 real dangling),
`generate` exits 0.

`entrypoint.ts` itself does **not** shrink much yet, because the dominant share of
its re-exports are owned by **38 eager-ordered chunk files** — the chain-split
fragments of the island's eager body (e.g. inlined `zod-v4`, whose bindings are
genuinely eager-initialised: `var BjA = createSchema(...)` runs at load). A
consumer importing a chunk binding directly could run that chunk's eager body out
of order relative to other chunks (the entry currently loads all chunks in source
order to preserve implicit side-effect ordering the binding-DAG does not capture),
so the index excludes chunk files and those bindings correctly stay on the hub.

## Phase 2 (to actually collapse the wall)

The remaining bulk needs one of:

1. **Cross-chunk eval-order proof** — reroute a chunk binding only when its chunk
   has no implicit (non-binding) side-effect dependency on an earlier chunk. This
   makes the eager-order safety analysis explicit per binding instead of the
   blanket "all chunks load in order" guard.
2. **Upstream reduction** — the eager-chunk bulk is largely inlined third-party
   (zod inlined more than once). Deduplicating / externalising that vendor code
   removes the bindings from the island entirely, which shrinks the barrel far
   more than routing ever could. This is the higher-leverage lever.

Phase 1 (this pass + `BindingOwnerIndex` + `eager_ordered_chunk` marker) is the
correct foundation both build on.
