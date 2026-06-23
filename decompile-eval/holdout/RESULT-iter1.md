# Held-out naming experiment — iteration 1 (baseline)

Faithful held-out of the current SKILL's binding naming, scored against the
curated Claude-app gold.

## Setup
- gold + bundle pair: **v2** (`claude-decompiled-v2`) — DB names are the v1 gold port; bundle sidecar byte-matches the DB.
- held-out files (stable semantic paths, names stripped, source regenerated minified):
  - `ipc/filesystem-bridge-constants.ts` (176)
  - `misc/config-settings-and-json-utils.ts` (198)
  - `account/account-details.ts` (233)
- naming: 1 agent per file, sees ONLY minified source + worklist + SKILL methodology; never the gold.
- scoring: normalized token match → string-misses adjudicated by an LLM judge.

## Result (607 gold bindings)
| metric | value |
|---|---|
| coverage (named/gold) | **99.2%** (602/602 attempted) |
| GOOD names (exact+fuzzy+judge-equivalent) | **78.9%** (475/602) |
| PARTIAL (value right, role lost) | 12.3% (74/602) |
| WRONG (misleading) | **8.8%** (53/602) |
| credit (good + ½ partial) | 85.0% |

String-only accuracy was 32.7% exact / 68.4% lenient — but the judge found 63 of
the 190 string-misses are semantically equivalent (Pattern↔Regex etc.), so the
harsh string number understates real quality. WRONG rate (8.8%) is the number to drive down.

## SKILL improvement levers (from judge failure patterns)
1. **Numeric constants named by value, not role** (biggest WRONG bucket ~25): `thirtyCount`←`defaultRetryCount`, `threeHundred`←`defaultThumbnailPx`. Fix: when a numeric literal is consumed as an argument (retries/limit/size/timeout/status), infer role from the call site; never name by the digit. Needs cross-file usage, not just the declaring file.
2. **Duration constants by magnitude, role lost** (~15 partial): `twoThousandMs`←`dispatchDebounceMs`. Fix: normalize ms→human unit AND pull role word (timeout/delay/debounce/poll) from usage.
3. **Regex vs Pattern suffix** (~20, scored equivalent): make the *scorer's* normalizer treat Regex≡Pattern, Ms≡Milliseconds, Dir≡Directory so they stop polluting misses; optionally add a SKILL convention (prefer `Pattern`).
4. **Domain hallucination over literal evidence**: `macErrorCodeRegex`←`ntStatusCodePattern`, `geminiStreamingMethods`←`genaiTrackedMethods`. Fix: prefer literal regex/string content over plausible-sounding domain guesses; don't invent a vendor/platform unless the value proves it.
5. **Dispatcher→"Bridge" drift & predicate mis-scope**: lock predicate names to the field actually tested; stop defaulting IPC objects to "Bridge" (gold uses "Dispatcher").

## Next
- Apply lever 3 to the scorer (free accuracy correction), levers 1/2/4/5 to the SKILL.
- Re-run on the SAME 3 files (train) + a fresh held-out set (test partition) → measure WRONG-rate delta.
- Lever 1 likely needs giving the namer cross-file call-site evidence (the `name plan` worklist already has evidence_tokens — check it's being surfaced).
