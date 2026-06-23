# Decompile SKILL eval harness

Use the heavily-named Claude-app decompile output as the **gold answer-key**, run
the current SKILL fresh, and score the fresh output against gold. Drives the
SKILL iteration loop with real precision/recall/accuracy numbers.

## DBs
- **gold (v1, curated):** `/Users/chaizhenhua/Codes/claude-decompiled/project.sqlite`
  — 10352 real binding names (2409 human + 7943 agent), 1491 cluster names.
- **v2:** `/Users/chaizhenhua/Codes/claude-decompiled-v2/project.sqlite`
  — ⚠️ a **naming port of v1** (binding names join 100%), so it is NOT an
  independent fresh run. Useful only as a scorer sanity check (expect ~100% naming).

## Join keys (verified bundle-stable across v1↔v2)
| entity | key | note |
|---|---|---|
| clusters | `island_cluster_names.fingerprint` | 1491/1491 |
| bindings | `(file_path, original_name, binding_key)` | strict; falls back to `(original_name, binding_key)` when `file_path` drifts (CJS relocation moved entrypoint.ts bindings to per-module files) |
| vendored pkg | `node_modules` path (`module_original_name`) | stable |
| island pkg | `(binding_name, function_span_start, function_span_end)` | byte spans stable |

## Run
```bash
python3 extract_gold.py --db <gold>/project.sqlite --out gold   # once
python3 score.py --gold gold --fresh <fresh-run>/project.sqlite --out report
```
Outputs `report/summary.md`, `report/grading.json` (skill-creator assertion
format), `report/fuzzy_samples.json` (name disagreements for LLM-judge / review).

## Metrics & gates (see score.py assertions)
- **Join coverage** (health) — if low, fingerprint/partition unstable; trust nothing else.
- **Package** precision/recall, two channels (vendored + island); precision must not drop while chasing recall.
- **Naming** coverage (fresh/gold) + accuracy (exact / fuzzy / miss via normalized token overlap).
- `fuzzy` bucket = where fresh disagrees with gold but is related → LLM judge adjudicates (incl. "fresh better than gold", which must NOT count as a miss).

## What's NOT done yet (next step)
The scorer is validated, but a **genuinely fresh run** is still needed — v2 is a
port. Cheapest real experiment = scoped held-out test:
1. Pick a held-out set of clusters/feature-dirs (≈40%).
2. Strip their names in a copy of the DB.
3. Re-run the current SKILL's naming/package pass on only those.
4. Score vs gold → that's the SKILL's true ability, with perfect ground truth.
Optimize SKILL rules on the train partition; validate on the held-out test partition.
