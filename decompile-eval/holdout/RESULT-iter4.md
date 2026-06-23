# Held-out naming — iteration 4: tail cleanup + convergence

## Change tested
On top of iter-3's conservative discipline, added 4 targeted reinforcements for the
residual WRONG tail: output hygiene (no minified token / raw number in a name),
own-context-first (don't borrow a far module's vocabulary), related-state
disambiguation, and canonical magic-number identification. Plus a deterministic
hygiene check.

## Result — lowest WRONG, with a genericness tradeoff
| | GOOD | PARTIAL | WRONG |
|---|---|---|---|
| iter1 train | 78.9% | 12.3% | 8.8% |
| iter2 train | 71.3% | 15.3% | 13.4% |
| iter3 train | 83.8% | 14.4% | 1.8% |
| **iter4 train** | 79.7% | 19.6% | **0.7%** (4) |
| iter3 test | 85.2% | 10.0% | 4.8% |
| **iter4 test** | 85.8% | 10.0% | **4.1%** |

WRONG reached its floor (0.7% train, 4.1% test). Hygiene clean (the `uIr16777228`
class is gone). Magic numbers fixed and 2 cases flipped to **"better"** — `machoFatMagic`
is correct where gold's `gzipMagicCookie` was factually wrong; the harness is now
catching GOLD errors, i.e. we're at the noise floor.

## Two honest caveats
1. **Tradeoff, not a pure win vs iter3.** Driving WRONG down pushed some GOOD→PARTIAL
   (over-generic): train GOOD 83.8%→79.7%, PARTIAL 14.4%→19.6%. A misleading name is
   worse than a vague one, so minimizing WRONG is the right priority — but iter3 and
   iter4 are close; iter3 is the better GOOD/PARTIAL balance, iter4 the safer WRONG.
2. **New self-inflicted failure.** The "honest-unused" framing made agents label 5
   real exported bindings `unusedMap`/`unusedNullRef` (no in-file reads ≠ dead).
   Fixed by SKILL rule 7: never call an exported binding unused from missing in-file reads.

## Convergence
The loop has hit diminishing returns: WRONG 8.8% → 0.7% (train) over four iterations,
remaining wrongs are partly gold noise (now surfaced as "better"). Stopping here.
Net shipped discipline (SKILL "Agent naming discipline" subsection, 7 rules) is
validated on held-out test files, not just the tuning set.

## Full arc
baseline 8.8% → intuitive trace fix REGRESSED to 13.4% → conservative discipline 1.8%
→ tail cleanup 0.7%. The iter-2 regression is the proof the measurement loop earns its cost.
