# Held-out naming — iteration 2: cross-file tracing REGRESSED. Measurement caught it.

## Change tested
Hypothesis: iter-1's top WRONG bucket (numeric constants named by value, not role)
comes from the namer seeing only the declaring file. Fix = let the namer Grep the
whole generated tree to trace each constant's call site and name by ROLE, plus
convention levers (Pattern suffix, Dispatcher not Bridge, literal-over-domain).

## Result — WORSE on the same 3 train files
| | GOOD | PARTIAL | WRONG |
|---|---|---|---|
| iter1 (3 files) | **78.9%** | 12.3% | **8.8%** |
| iter2 train (same 3) | **71.3%** | 15.3% | **13.4%** |
| iter2 test (2 new) | 79.7% | 10.1% | 10.1% |

WRONG went UP 8.8% → 13.4%. Coverage stayed ~99%.

## Why it backfired (judge analysis)
Cross-file tracing on **shared** constants overfits to ONE call site and assigns a
confident-but-wrong hyper-specific role. ~103 of 125 WRONG verdicts were over-tracing:
- `zlibMagicCookie` → `ntstatusStackBufferOverrunExit` (magic byte relabeled as a crash code)
- `gzipMagicCookie` → `machoFatMagic` (right category, wrong file format)
- `defaultRetryCount` → `tokenRefreshThresholdMs` (a count presented as a millisecond threshold)
- `tenSecondTimeoutMs` → `maxLineNumber` (timeout relabeled as a text bound)
- `cuLockActive` → `remoteToolsEnabled` (guard meaning inverted)

A generic value/magnitude name (gold's conservative choice) beats a confidently-wrong
specific one. The "smart" namer lies more.

## Lesson → refined lever for iteration 3
1. Trace cross-file, but assign a specific role ONLY if ALL call sites agree. If a
   constant is used in multiple unrelated contexts, keep the generic magnitude name.
2. NEVER reassign the identity of a magic-byte/value constant from a single guessed
   domain — name magic numbers by their literal value/format, not a traced usage.
3. Conservatism wins on WRONG-rate. The SKILL should bias toward "generic but true"
   over "specific but possibly false." This is the opposite of the iter-1 hunch.

## Methodological takeaway
This is the whole point of the harness: an intuitively-good SKILL change was net
negative, and the train/test measurement caught it before it shipped. Without the
gold-scored loop we'd have written the cross-file-tracing rule into SKILL.md and
degraded naming quality in production.
