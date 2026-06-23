# Held-out naming — iteration 5: generalization test (verify rule 7 + 9 unseen files)

## Setup
Two goals: (a) verify iter-4's rule 7 (no "unused" labels) actually fixed the 5
mislabels in main-view-window-wiring; (b) test the SHIPPED SKILL discipline on a
broad set of unseen files. Agents were given ONLY the SKILL "Agent naming
discipline" subsection (no ad-hoc reinforcement) — the honest test of what shipped.
9 files, 2236 bindings, all stable-path island modules.

## Result
| | coverage | GOOD | PARTIAL | WRONG |
|---|---|---|---|---|
| iter1 baseline (3 files) | 99% | 78.9% | 12.3% | 8.8% |
| iter3/4 test (2 curated files) | 99% | ~85% | ~10% | 4.1–4.8% |
| **iter5 (9 unseen, shipped rules)** | 100% | **77.7%** | 15.9% | **6.4%** |

Directionally the shipped rules generalize (6.4% vs 8.8% baseline) but WRONG is
higher than the curated 2-file test. The gap is concentrated, not diffuse:

WRONG by file: wake-scheduler **43**, image-handling **39** (= 57% of 143), then
settings-ext 19, main-view 11, oauth-org 10, others ≤9.

## Two systematic failures the broad set exposed
1. **Hygiene rule (6) violated systematically.** Agents append the minified token as
   a uniqueness suffix when many constants share a value: `fourThousandAKr`,
   `mapKdt`, `weakFieldGU`, `oneMinuteMsBzr` (~36 cases in one batch). Prompt rules
   don't stop this — it needs deterministic enforcement in the `binding-names` accept
   path (reject any semantic name containing its own original_name).
2. **Rule 7 ("unused") still ignored.** The 5 main-view bindings (aKe/bG/cJA/eT/KJe)
   came back as `unusedAkeMap`/`unusedBgRef` — token-embedded AND falsely "unused".
   Root cause: read-less exports need cross-file *tracing*, and the agent punted.
3. **Wrong-domain on un-externalized vendor code.** wake-scheduler/image-handling are
   full of inlined zod/sentry/p-queue/xmlbuilder internals. Per the SKILL these
   should be externalized BEFORE naming; naming them at all is the wrong activity and
   is where most WRONG comes from.

## Actions taken
- Strengthened SKILL rule 6 (no token-as-suffix; disambiguate with context word or
  numeric index) and rule 7 (trace, never punt to "unused").
- Added a "vendor-heavy regions are an externalization signal" callout.
- Flagged a deterministic pipeline guard: `binding-names` should reject a semantic
  name that contains its own `original_name` substring (removes the whole hygiene
  bucket without trusting agent compliance).

## Takeaway
The discipline is net-positive and generalizes, but two of the three residual
failure classes are NOT prompt problems — they're (1) hygiene best enforced in code,
and (2) an externalize-first process gap. Further prompt iteration has low headroom;
the next real wins are the CLI hygiene guard and a stricter externalize-before-name
gate on vendor-dense files.
