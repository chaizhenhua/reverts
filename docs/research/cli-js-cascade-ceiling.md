# cli.js cascade-matching ceiling investigation

Recorded during the iterative recall-improvement session that took
`cli.js` from 124 → 155 cascade attributions (+25.0 %). Captures why
further normalize-pass / scope-analysis work no longer moves the cli.js
number, so future contributors don't burn time on the same dead end.

## Setup

- Bundle: `/home/chaizhenhua/Codes/reverts/cli.js` — Claude Code's
  ~13 MB minified bundle (esbuild `__esm` wrappers, 4448 modules).
- Cache database: `/home/chaizhenhua/Codes/reverts/reverts-output/.reverts.db`
  project id 2 ("cli"). The `package_source_cache` table holds 13
  package families.
- Match command: `match-packages --input <db> --project-id 2 --apply`.

## Final tier breakdown (snapshot of session)

```
structural_only              60
structural_anchored          38
structural_only_alternate    20   ← introduced this session
feature_similarity           14
exact                        12
structural_anchored_alternate 5   ← introduced this session
feature_similarity_alternate  4   ← introduced this session
exact_alternate               2
-------- TOTAL                155
```

zod 134, localforage 21.

## Why cli.js is at its ceiling

Inspecting `package_source_cache` reveals the immediate bottleneck:

| package      | version  | cached entry                  | bytes  |
|--------------|----------|-------------------------------|--------|
| zod          | 3.22.5+  | `lib/index.mjs`               | ~140 KB |
| localforage  | 1.10.0   | `dist/localforage.js`         | ~94 KB |
| **rxjs**     | 7.8.x    | **`dist/esm5/index.js`**      | **~10 KB** |

The zod and localforage cache entries are bundled-single-file builds
that include every internal helper, so the fingerprint index covers
all of the function bodies cli.js's bundle re-emits. `rxjs`, by
contrast, caches **only the entry-point file** — a thin re-export
shim. The actual `Observable`, `Subject`, `Subscriber`, scheduler, and
operator implementations live under `dist/esm5/internal/*.js`, which
the cache does not contain.

cli.js contains 240 bundle modules tagged `rxjs` (output category from
the heuristic detector). None of them match cascade because there are
no rxjs **function fingerprints** in the index to compare against —
the entry-point file is structural glue, not implementation.

`package_attributions` (the binary-search-source matcher) confirms:
all 240 rxjs rows are rejected with `package version search found no
usable evidence`. No bytes-level overlap either.

## The 31-attribution gain in this session

The +31 cascade attributions came from infrastructure improvements,
not new strict-equivalent rewrites:

1. **Closure-scope `callee_set` filter** (universal-locals): +3
2. **`StructuralAnchoredAlternate` tier** (weight 500): +4
3. **`FeatureSimilarityAlternate` tier @ Jaccard 0.5**: +4
4. **`StructuralOnlyAlternate` tier** (weight 5): +20

Each of these unlocks matches by letting an alt-source fingerprint
(produced by a normalization pass) pass through the cascade when the
primary fingerprint missed. They benefit zod and localforage modules
where some helper function had a minifier-vs-source axis divergence.

## Why further normalize passes don't move cli.js

The remaining cli.js mismatches are not minifier-pattern issues —
they're cache-content issues. Adding more spec-equivalent rewrites
(`for(;cond;) → while(cond)`, `if(x==null) x=v → x??=v`, etc.) is
still correct work, but it lands on functions that don't have a
candidate in the cache, so the cascade can't match them either way.

## What WOULD move cli.js further

| Approach | Expected gain | Constraint |
|----------|--------------|------------|
| Expand cache to fetch `rxjs/dist/esm5/internal/**/*.js` | High (+50–200) | User explicitly forbade cache expansion |
| Fetch rxjs UMD bundle (`dist/bundles/rxjs.umd.js`) | High | Same constraint |
| Add more packages to cache (highlight.js, lodash, react, semver, ajv, undici, axios) — cli.js has 300+ modules each | Very high | Same constraint |
| Detect bundled-package boundaries and synthesize per-export fingerprints from entry-point file (would need the re-export tree resolved) | Medium | Engineering complexity |
| `--explain <fn_id>` debugging tool to surface which axes nearly-matched on unmatched functions | None directly; enables future targeted improvements | Substantial UX work |

## Recommendation

For users matching against richer caches (or against caches that
include full package implementations), the 33-pass / 7-tier
infrastructure delivered this session should pay off. For cli.js
specifically, the answer is to either expand the cache or accept the
~155-attribution ceiling.
