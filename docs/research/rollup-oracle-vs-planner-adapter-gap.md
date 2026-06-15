# Why rolling up the `package_attributions` DB metric does not reduce
# emitted source-code size

Recorded after a session that built `reverts-analyze::rollup::{oracle,
projection, apply}` plus the `reverts-rollup-apply` binary and proved
they lift the DB-level "accepted external_import" ratio for project 1
("default") from **38 % to 98.61 %** — yet the actual `generate-project-v2`
output only shrinks from **3916 → 3904 files** and the on-disk size stays
at **53 MB**. The optimistic DB metric does not translate into emit
reduction because of a more conservative downstream check in the planner.
Keep this note before anyone tries to wire the oracle further into the
matcher / planner expecting source-size wins.

## The hollow path

1. The matcher writes `package_attributions` rows with
   `status = 'accepted'` and `emission_mode = 'external_import'` only
   when it can prove a module aligns with a package's public surface.
2. `reverts-analyze::rollup::oracle::build_oracle` extends this set: any
   closure-owned module of a package that *as a whole* has a top-level
   externalization hint (a row in `package_externalization_hints`) plus
   at least one accepted anchor attribution gets a verdict of
   `Externalizable`. The projection then flips those rows in the DB.
3. After this pass, `accepted_external_module_ids()`
   (`reverts-package/src/lib.rs:130`) returns **3257 of 3303** package
   modules for project 1. The DB metric reads ≥97 %.
4. `reverts-planner::PlannerAnalysis::from_program`
   (`reverts-planner/src/lib.rs:660`) then calls
   `adapter_required_package_modules`
   (`reverts-planner/src/lib.rs:15975`), which walks
   `module_dependencies` and `candidate_reads_by_module`. **For nearly
   every rolled-up module the planner finds that some other module in
   the bundle reads bindings whose names are not part of the package's
   public surface** (minified bundler-internal names like `Yj`, `Cz9`,
   `zz9`, `ZK` for a lodash module) and therefore marks the module as
   `adapter_required`. Adapter-required modules still emit their source
   verbatim, with relative-path imports to runtime helpers.
5. The emitted file at e.g.
   `modules/232501-lodash/_internal/root-wrapper.ts` contains no
   `from 'lodash'` import. It's the original module body with extra
   bindings exported under their compressed names so internal
   cross-module references stay resolvable. The "external import" verdict
   is invisible to the end-user output.

## Why the oracle's verdict is over-eager

The oracle answers "does this `(name, version)` pair have a public
externalization hint and at least one accepted anchor?" That is a
necessary but not sufficient condition for source elimination. The
*sufficient* condition that the planner checks is:

> Can every consumer of this module's bindings get the same bindings
> from `import { … } from '<top-specifier>'`?

For minified bundles the answer is almost always *no* for the deep
internal modules: the bundler renamed everything to one- or two-letter
identifiers (`Yj`, `eK`, `wA6`) that have no relationship to the
package's public surface (`merge`, `cloneDeep`). Importing the public
API alone cannot satisfy the existing internal call graph.

## What does work, and what doesn't

- **The DB metric (`accepted_external_import / package_modules`) is
  not a source-size metric.** Treat it as a measurement of "how many
  modules satisfy the externalizability prerequisites", not "how many
  modules will be source-eliminated".
- **`reverts-rollup-apply` remains useful as a reporting / scenario
  tool**: it answers "how would the DB read if we believed every
  closure-owned module of a hint-covered package was externalizable?"
  That number is meaningful in package-surface coverage research and
  for the per-package `reverts-emission-stats` probe.
- **Wiring the oracle into `match-packages` by default delivered no
  emit reduction** (verified: project 1 baseline = 3916 emitted files
  at 38 % DB; project 1 post-rollup = 3904 emitted files at 98 % DB).
  That integration was therefore reverted; the binary remains opt-in.

## What it would take to make source-size actually drop

Any of these is a multi-session effort:

- Teach the planner to *rewrite* internal-name reads at the
  `adapter_required` boundary: replace consumer reads of `Yj` etc. with
  the corresponding public-surface name (requires per-binding mapping
  evidence the matcher does not currently produce).
- Have the oracle ingest the bundle's actual cross-module read names
  and reject rollup candidates whose binding set is not entirely
  covered by the package's public surface — this would tighten the
  oracle so its verdict matches what the planner can act on.
- Move the entire externalization decision into the planner and
  drop it from the matcher / DB. The planner already has the consumer
  binding graph it needs; the matcher's per-module verdict is too
  context-free.

None of these are *audit* fixes; they are pipeline-strategy changes
that need their own spec and validation plan. Until one lands, do not
add code that assumes a high `accepted_external_import` ratio
translates to smaller emitted output.
