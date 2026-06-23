# Held-out naming — iteration 3: conservative tracing WINS, codified into SKILL

## Change tested
Refined the iter-2 lever after it regressed. Same convention rules, but tracing made
CONSERVATIVE: assign a role-specific name only if ALL call sites agree; shared/ambiguous
constants → generic magnitude name; magic numbers named by literal value/format, never a
guessed domain; governing principle "generic-but-true beats specific-but-false".

## Result — best of all three iterations, and it generalizes
| | GOOD | PARTIAL | WRONG |
|---|---|---|---|
| iter1 (3 train files) | 78.9% | 12.3% | 8.8% |
| iter2 train (unrestricted tracing) | 71.3% | 15.3% | 13.4% |
| **iter3 train (conservative)** | **83.8%** | 14.4% | **1.8%** |
| iter3 test (2 unseen files) | 85.2% | 10.0% | 4.8% |

WRONG fell 8.8% → 1.8% on train (~80% reduction) and GOOD rose 78.9% → 83.8%.
Holds on the held-out TEST files (GOOD 85.2%, WRONG 4.8%) → the rule generalizes,
not overfit to the 3 tuning files. Coverage stayed ~99–100% throughout.

## Remaining WRONG tail (32 cases, now small)
- A few magic numbers still mislabeled as exit codes (gzip/zlib magic → exitCode…); rule 3 needs to bite harder.
- A handful of confident-wrong specific roles still slipped (maxRedirectCount→"topThree", tool-name limits → OTel "baggage").
- Identity swaps inside a related pair (cuLockActive / sidePanelState both → "remoteTools*").
- Output hygiene: some names kept the minified prefix (`uIr16777228`) — added as rule 6.

## Shipped
Codified the validated discipline into `skills/decompile/SKILL.md` → new subsection
"Agent naming discipline: generic-but-true beats specific-but-false" (6 rules, with the
measured 8.8%→1.8% evidence). Production agent naming now follows the rule the harness proved.

## Method note
Three iterations: baseline (8.8% wrong) → intuitive fix that regressed (13.4%) → refined
fix that won (1.8%), each decided by gold-scored train/test measurement, not opinion. The
regression in iter-2 is the proof the loop is worth running: the obvious change was wrong.
