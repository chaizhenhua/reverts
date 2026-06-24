# Init-Shim Classification & Half-Residue Recovery

Distilled from a real cleanup pass on projects 1, 9659, 11585, 11979, 13495 (Apr-May 2026). Covers two related anti-patterns that the auto pipeline leaves behind: **half-residue records** (`cat` says one thing, `package_name`/`semantic_name` says another) and **init-shim mislabels** (tiny esbuild lazy-init wrappers landing on the wrong package or staying `unknown`).

## What an init shim is

esbuild's `__esm()` lazy initializer produces standalone "modules" that are pure delegation:

```js
// Common shapes (size ≤ 50 bytes, exactly 1 symbol, wrapper_kind = pure/composite_init_wrapper):
var X = O(() => { Y = Z; });        // identifier alias
var X = O(() => { Y(); });          // chain into another init
var X = O(() => { Y = {}; });       // namespace placeholder
var X = O(() => { Y(); Z(); ... }); // barrel of init calls
```

**SQL fingerprint:**
```sql
module_type='esm_lazy'
AND wrapper_kind IN ('pure_init_wrapper','composite_init_wrapper')
AND byte_end - byte_start <= 50
AND symbol_count <= 2
```

Shims have **no independent semantic identity** — their "ownership" is the package or app domain that owns the global they reference (`Y` / `Z` in the patterns above). Treat shim classification as inheriting from an external authority, never as standalone analysis.

## Half-residue shapes to scan for

These contradict the storage contract (`cat≠unk requires sem`; `cat=pkg requires pkg + ver`) and break downstream propagation. Scan with:

```sql
-- Shape A: cat=pkg but pkg_name missing
WHERE module_category='package' AND (package_name IS NULL OR package_name='')

-- Shape B: cat=pkg but semantic missing
WHERE module_category='package' AND (semantic_name IS NULL OR semantic_name='')

-- Shape C: cat=pkg but version missing (less harmful, but matcher's
-- recover_pending_tasks needs (pkg, ver) to enqueue)
WHERE module_category='package' AND package_name<>'' AND (package_version IS NULL OR package_version='')

-- Shape D: cat='' empty string (escape from the unk/app/pkg/builtin contract)
WHERE module_category=''

-- Shape E: sem='pkg/loader/X' synthetic placeholder
WHERE semantic_name LIKE 'pkg/loader/%'

-- Shape F: cat=app + package_name set (contradiction)
WHERE module_category='application' AND package_name IS NOT NULL AND package_name<>''
```

P1/P9659 baseline expectation after a full-pipeline run: every shape should be 0 (except Shape C in pre-matcher state).

## Recovery protocol

For each Shape-A/B/C/D module, work in this order. **Stop at the first authority that fires** — never escalate to a weaker source if a stronger one is available.

### 1. Source-content fingerprint (strongest)

Read the module body and any non-shim it transitively wraps. Look for **structural fingerprints** — strings, symbols, constants that the package emits verbatim:

| Package | Fingerprint |
|---|---|
| `@anthropic-ai/sdk` | `class * extends Error { ... }` with `status` + `headers` + `error.message` formatter, `requestID` from `'request-id'` header |
| `@azure/core-client` | `Symbol.for('@azure/core-client original request')`, `MapperType` enum (`Base64Url`/`Sequence`/`UnixTime`...), `QueryCollectionFormat` enum (`CSV`/`SSV`/`Multi`/`TSV`/`Pipes`) |
| `@azure/core-rest-pipeline` | `class extends Transform { _transform() { ... progressCallback ...} }`, `'TYPESPEC_RUNTIME_LOG_LEVEL'` env var |
| `@azure/identity` | `jn('identity')` = `@azure/logger.createClientLogger('identity')` |
| `@azure/msal-common` | error classes inheriting with `errorDescription` field, `name = iC3` style msal error name |
| `@anthropic-ai/sdk` AbortError | `class * extends Error { name = 'AbortError' }` (also matches several other libs — disambiguate with siblings) |
| `axios` | `o.inherits(*, Error, { toJSON: ... config, code, status })` = `AxiosError` |
| `date-fns` | `Math.pow(10, 8) * 24 * 60 * 60 * 1e3` (max safe distance) + `Symbol.for('constructDateFrom')` |
| `domino` | DOM Node constants `ELEMENT_NODE = 1, ATTRIBUTE_NODE = 2, TEXT_NODE = 3` |
| `is-wsl` | `__IS_WSL_TEST__` env var |
| `lodash` (v4) | `1 / setToArray(new Set([, -0]))[1] == -INFINITY` (sparse + negative zero detection); `getTag = (function() {...})()` IIFE producing function|null |
| `parse5` | `NAMESPACES = { HTML: 'http://www.w3.org/1999/xhtml', MATHML: ..., SVG: ... }` |
| `zod` core | `Symbol.for('constructDateFrom')`, primitive ranges `safeint/int32/uint32/float32/float64`, `BigInt('-9223372036854775808')` |

When a fingerprint is present, the classification is authoritative regardless of what the parent or semantic name claims.

### 2. Parent-chain inheritance

Define a shim's parent as: any module that depends on this shim (i.e., consumer side, reverse direction of `module_dependencies`).

```sql
-- Get parent voters
SELECT par.package_name, COUNT(*) AS votes
FROM modules shim
JOIN module_dependencies md ON md.dependency_id = shim.id
JOIN modules par ON par.id = md.module_id
WHERE shim.id = ? AND par.package_name <> ''
GROUP BY par.package_name;
```

**Rule of thumb (validated empirically):**
- **Unanimous + parent has SPECIFIC semantic name** (e.g. `lodash/_internal/base-clone`, `parse5/html-constants`): inheritance is reliable. ~96% of P1's Class-A candidates.
- **Unanimous + parent has GENERIC semantic name** (e.g. `<pkg>/init-wrapper-NN`, `init-loader-N`): unreliable. The generic sem itself may have been propagated from another project's mismatched module.
- **Split votes** (2+ different `package_name`): never trust the majority blindly. Read the source — the *minority* answer may be the correct one (e.g. AIA had 4 msal-node parents and 1 @azure/identity parent; correct answer was @azure/identity, because its symbol literal `'identity'` is azure-identity-specific).

### 3. Demote to `app`

If parents are exclusively `cat=application` (no pkg parent at all), the shim is an app-internal init artifact. Demote with `cat=app`, leave `pkg`/`ver` null, leave `sem` either null or `_init/<descriptive>-<byte_start>`.

### 4. Reset to `unknown`

If none of 1–3 apply (no source fingerprint, contaminated parent cluster, no parent at all), set `cat=unknown` and clear pkg/ver/sem. **This is the honest default** — better than guessing wrong and contaminating cross-project propagation.

## Cross-project mislabel cascades

The most dangerous failure mode: a small mistake in one project's classification, propagated by `cross_project_propagate`, becomes a large contamination cluster. Real example from P1:

1. Some other project (P11585? P1's earlier state?) had a module with byte-span signature similar to P1's `TvB`.
2. That other module was correctly tagged `highlight.js + highlight-js/init-wrapper-50`.
3. Cross-project propagation copied the label to P1's `TvB`.
4. P1's `TvB` is actually `@azure/core-client` (its body calls `UMA` which is `Symbol.for('@azure/core-client original request')`).
5. The wrong label spread to TvB's wrappers/dependents within P1.
6. Now P1 has dozens of @azure/core-client modules sitting under the highlight.js bucket.

**Detection:** when a "highlight.js" module's **body content** (not just its sem) names another package's symbols, you've found a contamination.

**Symptom in the data:** generic init-wrapper sems (`<pkg>/init-wrapper-NN`, `<pkg>/init-loader-N`) are higher-risk than specific function/feature sems (`<pkg>/_internal/has-unicode-word`, `<pkg>/locale/_lib/buildFormatLongFn`).

**Fix:** don't try to repair the cluster from inside the same project. Run signature matching against the real npm package (`module_level_matcher`) and let the matcher rewrite labels for whichever modules' signatures actually match the package's real submodules. Modules that don't match are mislabeled — rip out their wrong label and reclassify by source content.

## Anti-patterns: what NOT to do

### Don't infer pkg from the **first segment of `semantic_name`**

The path `lodash/_internal/has` looks like authoritative evidence that the module is part of `lodash`, but the semantic name was set by **decompilation output organization** (which is itself a guess in the reference project), not by analysis of code content. Using sem-prefix as a permanent classification rule produces:

1. **Self-amplification** — one bad label feeds future inferences.
2. **App-vs-pkg collisions** — user code organized under `core/` accidentally collides with `@aws-sdk/core`.
3. **Scoped-package degradation** — `@scope/pkg` flattened to `pkg/` loses the scope.

This rule was attempted and removed in commit `830b8517`'s revert. The codified replacement: classify only via real evidence (source fingerprint, signature match, LLM analysis, structural parent chain).

### Don't equate "init shim" with "package"

A pure-init-wrapper module's category is **whatever owns its target global**, not "package by virtue of being a shim". Many init shims are app-domain (initialization scaffolding for the user's own modules) and must be `cat=app`.

### Don't bulk-promote from majority parent vote

For shims with multiple parents disagreeing on package, majority-vote inheritance has been shown to pick the wrong answer (AIA case). When parents disagree, **always source-verify** before assigning.

## Schema-level guards in place

### Layer 3A — signature-overlap floor (commit `d94ec4a7`)

`engine::propagation::orchestrator::persist_module_correspondence` computes
the Jaccard similarity of source/target function-signature hash sets before
forwarding any labels. Below `PROPAGATION_SIG_OVERLAP_FLOOR = 0.3` the
forward is skipped entirely. Defends against the classic mislabel cascade:
two modules with similar byte-spans but unrelated bodies (the highlight.js
↔ azure-core-client failure mode) no longer share labels just because the
correspondence engine paired them.

### Layer 3C — package.json whitelist (commits `84ec89ad` + `6f40141b` + `7dfccf1a`)

`engine::project_packages::read_declared_packages(root_path)` reads the
project's `package.json` (or `<root>/out/package.json`, or
`<root>/../package.json`) and returns the union of `dependencies`,
`devDependencies`, `peerDependencies`, `optionalDependencies`. This set is
the **authoritative whitelist** of npm packages the project may legally
use.

`SqliteStore::strip_undeclared_package_labels(project_id, declared)` then
resets every module whose `package_name` is *not* in that whitelist back to
`cat=unknown` with pkg/ver/sem cleared. Hooked into
`cross_project_propagate_impl`'s tail — every cross-project run now
self-cleans the target. Response field `undeclared_labels_stripped`
reports the count.

### Layer 3B — sibling-cluster outlier detection (commit `97cbaf49`)

`SqliteStore::find_pkg_cluster_outliers(project_id, min_siblings, threshold)`
groups every `cat=pkg + package_name` cluster, computes pairwise Jaccard of
function-signature hash sets within each cluster, and flags modules whose
average overlap with siblings is below `threshold`. Surfaces contamination
even when L3A/L3C didn't catch the original mislabel — a contaminator
inside a healthy cluster will have ~0 overlap with the rest. Returns
`Vec<PkgClusterOutlier>`, ready to feed into `validation_issues` or a
human-review queue.

### Layer 3 SQL guard (older, commit `830b8517`)

`propagate_module_metadata_by_id_with_origin`:

- `application` → `package` upgrade allowed *only* when source authoritatively says `package` AND carries a non-empty `package_name`.
- `package` → `application` downgrade is **never** allowed (use `reverts-cli module-classify --input <db> --project-id <id> --batch <TSV> --apply` to demote explicitly).
- `package_name`, `package_version`, and `semantic_name` are overwritten when target value is NULL **or empty string**.

### Layer 2 — matcher widens scan (commit `ad770014`)

`engine::package_matcher::background::recover_pending_tasks` no longer
filters on `category='package'` — any module with both `package_name` and
`package_version` set enters the matching queue, regardless of category.
This way half-residue rows (e.g. `cat=application + pkg=lodash + ver=4.x`
left over from migrations or imperfect propagation) get verified by the
real npm signature pipeline instead of remaining stuck.

### Layered effect

These guards mean: you can run `cross_project_propagate` repeatedly without producing new half-residues, **and** any pre-existing contamination is actively pruned. Cluster-outlier detection (L3B) lets ops surface stale contamination via the validation_issues queue.

## Operational checklist for a project's classification health

For any project, expect these counts to all reach 0 after a full pipeline + cleanup pass:

```sql
-- All in one query (parameterise project_id)
WITH proj AS (SELECT m.* FROM modules m JOIN project_files pf ON pf.file_id=m.file_id WHERE pf.project_id=?)
SELECT 'pkg + no name', COUNT(*) FROM proj WHERE module_category='package' AND (package_name IS NULL OR package_name='')
UNION ALL SELECT 'pkg + no ver', COUNT(*) FROM proj WHERE module_category='package' AND package_name<>'' AND (package_version IS NULL OR package_version='')
UNION ALL SELECT 'pkg + no sem', COUNT(*) FROM proj WHERE module_category='package' AND (semantic_name IS NULL OR semantic_name='')
UNION ALL SELECT 'cat empty string', COUNT(*) FROM proj WHERE module_category=''
UNION ALL SELECT 'pkg/loader/ placeholder', COUNT(*) FROM proj WHERE semantic_name LIKE 'pkg/loader/%'
UNION ALL SELECT 'app + has pkg_name (contradiction)', COUNT(*) FROM proj WHERE module_category='application' AND package_name IS NOT NULL AND package_name<>''
UNION ALL SELECT 'sem=<pkg>/X but cat<>pkg', COUNT(*) FROM proj p
                                              WHERE p.semantic_name LIKE '%/%'
                                                AND p.module_category<>'package'
                                                AND substr(p.semantic_name,1,instr(p.semantic_name,'/')-1)
                                                    IN (SELECT DISTINCT package_name FROM modules WHERE package_name IS NOT NULL AND package_name<>'');
```

`cat=unknown` is acceptable in the residue (uncle to the matcher) but should be small (<5% of total modules); large unknown counts indicate the pipeline didn't converge.

## Field-by-field truth source

| Field | Authority |
|---|---|
| `module_category` | source content fingerprint > matcher signature match > parent-chain (with sibling agreement) > LLM analysis. Never sem-prefix. |
| `package_name` | same as category, must be a real npm package name |
| `package_version` | matcher signature match > sibling rows in same project > package.json declared range |
| `semantic_name` | LLM analysis of the module's content > path-derived from manually decompiled reference > generated `<pkg>/_init/<orig>` for shims. The semantic name has no truth-bearing role for category. |

When a field has multiple competing authorities, the upper one wins. The lower one only fills in gaps when the upper one is unavailable.

## L1 — Operational data fix (manual, post-MCP-rebuild)

For projects whose contaminated cluster cannot be repaired by L2 + L3 alone (the source of truth — npm signatures — needs to be locally available), the operator runs:

1. `cargo build --release` then `/mcp` to reconnect.
2. `npm pack <pkg>@<ver>` for each suspect package (e.g.
   `@azure/core-client@1.9.2`, `@azure/core-rest-pipeline@1.17.0`,
   `@azure/msal-common@1.7.2`, `@azure/msal-node@5.0.4` for P1's hljs
   contamination).
3. Untar each into a per-package directory.
4. `mcp__reverts__ingest_reference_sources(project_id, base_path,
   file_paths=[...])` to load the npm sources into the project as
   reference modules.
5. For each unknown bundle module, call
   `mcp__reverts__match_functions(project_id, original_module=<npm-source>,
   bundled_module=<unknown>, min_confidence=0.8, use_call_graph=true)`.
6. Use the matcher's confidence to write final classifications via
   `reverts-cli module-classify --input <db> --project-id <id> --batch <TSV> --apply`.
7. Run a final `cross_project_propagate` so newly-classified rows fan
   out to siblings; the L3 guards prevent regression.
