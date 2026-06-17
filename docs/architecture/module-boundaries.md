# Module Boundaries

This document defines the crate boundaries for Reverts Next. It is the
authoritative map for where new output-v2 code should live and which dependency
directions are allowed.

## Current Workspace

The workspace currently contains these output-v2 crates:

| Crate | Status | Responsibility |
| --- | --- | --- |
| `reverts-js` | existing | OXC parsing, code generation, source-type selection, and identifier utilities |
| `reverts-ir` | existing | Shared domain primitives: modules, bindings, binding shapes, package surfaces, def-use basics, and lightweight flow/dependence records |
| `reverts-observe` | existing | Audit reports, finding codes, severity, and telemetry event types |
| `reverts-input` | existing | In-memory input bundle and row conversion contract |
| `reverts-graph` | existing | Graph, def-use, import/export, and lightweight control-flow construction from input bundles |
| `reverts-package` | existing | Package surface index construction from input attributions and import specifier resolution |
| `reverts-package-matcher` | existing | AST-fingerprint matching of bundle modules against cached npm package sources, persisting accepted attributions |
| `reverts-model` | existing | Program and enriched-program handoff records |
| `reverts-analyze` | existing | Semantic naming, package-decision enrichment, shape-solution wiring, and compiler-profile detection |
| `reverts-planner` | existing | Emit planning for imports, declarations, and exports |
| `reverts-emitter` | existing | Parseable emitted project generation from plans |
| `reverts-pipeline` | existing | Pure in-memory minimal decompilation loop |
| `reverts-cli` | existing | CLI argument contract for output v2 commands |
| `reverts-rollup-adapter` | existing | SQLite-backed rollup probe/apply tools and bins; adapter around pure `reverts-analyze::rollup` projections |
| `reverts-fixtures` | existing | Self-contained test fixture builders |

## Target Crate Map

The target architecture separates pure domain logic, graph construction,
planning, emission, and command orchestration.

| Crate | Layer | Responsibility |
| --- | --- | --- |
| `reverts-js` | foundation | Parse and format JavaScript/TypeScript through OXC and validate generated source |
| `reverts-ir` | foundation | Own shared IDs, binding names, module records, binding shapes, package surfaces, def-use records, and graph-neutral flow/dependence records |
| `reverts-observe` | foundation | Own structured findings, audit reports, telemetry records, and acceptance diagnostics |
| `reverts-input` | input | Convert database rows, inline fixtures, and other source records into `InputBundle` |
| `reverts-graph` | analysis | Build `RevertsGraph`, `DefUseGraph`, `ImportExportGraph`, and lightweight `ControlFlowGraph` from input and AST facts |
| `reverts-model` | analysis | Hold `ProgramModel`, `SemanticNameMap`, and `EnrichedProgram` as the typed handoff from analysis to planning |
| `reverts-analyze` | analysis | Enrich the program model with semantic names, binding-shape solutions, and package-import decisions |
| `reverts-package` | analysis | Resolve package names, builtins, exports, subpaths, and build the package-surface index from input attributions without emitting source |
| `reverts-package-matcher` | analysis | Match bundle modules to cached npm package sources via AST fingerprints; produce accepted attribution rows the pipeline can later read |
| `reverts-rollup-adapter` | adapter/tool | Own SQLite loading/apply logic and command-line probes for rollup externalization projections; depends on pure rollup analysis but is not part of core generation |
| `reverts-planner` | planning | Produce file-level import, export, local binding, and synthetic binding plans |
| `reverts-emitter` | emission | Convert accepted plans into AST-backed emitted files and `EmittedProject` |
| `reverts-pipeline` | orchestration | Connect input, model enrichment, planning, emission, and parse audit for the core library loop |
| `reverts-cli` | orchestration | Provide `generate-project-v2` and user-facing command orchestration |
| `reverts-fixtures` | test support | Provide self-contained fixture builders for tests without production dependency edges |

## Dependency Direction

Production dependencies must flow downward through the architecture. Lower
layers must not depend on higher layers.

```text
reverts-cli
  -> reverts-pipeline
      -> reverts-analyze
          -> reverts-model
              -> reverts-graph
                  -> reverts-input
          -> reverts-package
              -> reverts-input
      -> reverts-planner
          -> reverts-model
          -> reverts-package
      -> reverts-emitter
          -> reverts-planner
      -> reverts-observe
  -> reverts-package-matcher
      -> reverts-input
      -> reverts-js
      -> reverts-observe

reverts-rollup-adapter
  -> reverts-analyze
  -> reverts-input
  -> reverts-package
```

`reverts-rollup-adapter` is intentionally outside the core output loop: it may
use SQLite, filesystem paths, and command-line process exits because it is an
adapter/tool crate, while `reverts-analyze::rollup` remains pure projection and
report logic.

The foundation crates can be used by every production crate:

```text
reverts-js
reverts-ir
reverts-observe
```

`reverts-fixtures` is test-only support. Production crates must not depend on
it as a normal dependency.

The `InputBundle` field-level contract is documented in
[input-data-model.md](input-data-model.md).

```text
crate tests / integration tests
  -> reverts-fixtures
      -> reverts-input
      -> reverts-ir
      -> reverts-js
      -> reverts-observe
```

## Boundary Rules

- `reverts-js` owns parsing, formatting, source-type selection, and identifier
  sanitization. Other crates should not duplicate JavaScript syntax rules.
- `reverts-ir` owns shared domain types only, including graph-neutral control
  flow and data-dependence records. It should not know about file
  writing, CLI arguments, databases, package registries, or audit policy.
- `reverts-observe` owns structured diagnostics only. It should not decide how
  to recover from a finding.
- `reverts-input` owns conversion into `InputBundle`. It may validate input
  shape, but it must not plan imports, infer binding shapes, or emit source.
- `reverts-graph` owns graph construction and graph invariants. It constructs
  AST facts, def-use, import/export, control-flow, and dependence views; it
  reports unresolved reads, duplicate definitions, and dependency
  inconsistencies through downstream audit.
- `reverts-model` owns stable program snapshots and enriched analysis records.
  It must not inspect databases, fetch packages, or emit source.
- `reverts-analyze` owns pure enrichment from `ProgramModel` to
  `EnrichedProgram`: semantic names, package decisions, shape solutions, and
  pure rollup projection/oracle/report logic. It must not own SQLite adapters
  or command-line binaries.
- `reverts-package` owns package-surface decisions and constructs the
  `PackageSurfaceIndex` from input attributions. It may accept, reject, or
  classify package imports, but it must not generate import statements.
- `reverts-package-matcher` owns AST-fingerprint matching between bundle modules
  and cached package sources. Its results land in `package_attributions` rows
  that the pipeline reads as input — it must not call the planner or emitter.
- `reverts-planner` owns output plans. It decides which imports, exports, local
  declarations, and synthetic bindings are needed before emission.
- `reverts-emitter` owns AST-backed source generation and emitted project
  assembly. It must not repair invalid plans after generation.
- `reverts-pipeline` owns the in-memory core loop. It coordinates crates but
  must keep filesystem, network, and external-program access out of core tests.
  Its post-emission work is modelled as explicit pre-accept transforms that
  produce `PreAcceptProject`; only audit-clean output becomes `AcceptedProject`.
- `reverts-cli` owns command orchestration, argument parsing, paths, and process
  exit behavior. Core behavior should remain testable without invoking the CLI;
  project writing is isolated behind the CLI `project_writer` adapter and
  consumes `AcceptedProject` rather than unaudited bytes.
- `reverts-fixtures` owns fixture builders used by tests. It must not become a
  source of production behavior.

## Data Flow

The main data flow is:

```text
InputBundle
  -> ProgramModel
  -> EnrichedProgram
  -> EmitPlan
  -> ValidatedEmitPlan
  -> EmittedProject (+ EmittedAssets)
  -> PreAcceptProject
  -> AuditReport
  -> AcceptedProject (only when no errors)
```

The planner and emitter receive already-validated structural data. If a required
definition, import, package subpath, or binding shape is missing, the pipeline
reports a structured finding before writing files.


## Planner Pass Pipeline

`reverts-planner` exposes `ImportExportPlanner` as a facade. Internally it now
builds a `PlannerContext`, prepares immutable `RuntimePlanPreparation`, and
runs named passes over `PlanningState`:

```text
PlanModulesPass
  -> EmitPackageRuntimePass
  -> MarkEntrypointRuntimePass
  -> EmitRuntimeHelpersPass
  -> EmitCliEntrypointPass
```

Mutable helper bookkeeping is split into `RuntimeHelperUsageAccumulator` and
`PackageRuntimeAccumulator`. Immutable runtime/package preparation lives in
`runtime_plan_preparation.rs`, and the module loop is mediated by
`ModulePlanningContext`. The legacy `compute_modules::plan_one_module` no longer
accepts a long positional parameter list; callers pass a typed
`ModulePlanInput` plus `ModulePlanAccumulators` bundle. New planner work should
be introduced as a named pass, context, or accumulator before adding cross-module
`pub(crate)` plumbing.

## Pre-Accept Output Stage

Pre-accept transforms are explicit, ordered, in-memory transforms that run after
emission and before acceptance audit. They are not post-write repair passes.
The current order is:

1. `canonicalize_source_locations`
2. `rewrite_asset_references`
3. `fold_static_template_literals`

The transform names and changed-file counts are recorded in
`PreAcceptTransformReport`. `OutputRun.project` is a `PreAcceptProject` so
callers cannot mistake it for accepted output; project writers must write
`AcceptedProject`, not raw `EmittedProject`.

## Source Surgery

Text/byte-level edits are centralized in `reverts-planner::source_surgery` for
the remaining cases where AST-first output is not yet practical. The module owns
the shared edit applier and line-removal newline policy, with parse and
delimiter-boundary tests. Passes that still scan source bytes must document why
they cannot use AST-first rewriting and should use `source_surgery`/`byte_lexer`
helpers rather than ad hoc string repair.

## Compiler Lowering Pipeline

`reverts-js::CompilerLowering` enumerates the per-compiler AST transforms
applied during emit. Each variant maps from a
`reverts-planner::CompilerRecoveryAction` and is dispatched inside
`format_source_with_module_items`. Lowerings run on the **emitted module
source** between the parse pass and the codegen pass, so audits operate on
the lowered text.

| Compiler | Lowering rules in `reverts-js` |
| --- | --- |
| `Babel` | Strip `Object.defineProperty(exports, "__esModule", ...)`; rewrite `_interopRequireDefault(require(X))` → `{ default: require(X) }`; rewrite `_interopRequireWildcard(require(X))` → `require(X)`; strip dead `_interopRequireDefault` / `_interopRequireWildcard` helper definitions when no references remain |
| `Esbuild` | Strip unreferenced runtime helper var declarations (`__commonJS`, `__defProp`, `__export`, ...); descend into a top-level IIFE wrapper to apply the same rule to IIFE-internal helpers |
| `Webpack` | Strip `__webpack_require__.r(...)` no-op marker calls; strip unreferenced webpack runtime helper var/function declarations (`__webpack_require__`, `__webpack_modules__`, ...); descend into a top-level IIFE wrapper |
| `None` | No transforms — used for `DirectModuleSource` (compiler classified as `Unknown`) |

Lowering rules never strip helper definitions that still have at least one
`IdentifierReference` somewhere in the parsed program — the strip pass is
governed by a global reference counter (`program_references_named_identifier`)
that walks the entire AST including IIFE-internal scopes.

## Asset Subsystem

Static assets referenced by application source (native bindings, wasm,
vendor binaries) are first-class pipeline outputs:

- `reverts-input::AssetInput` carries logical path, output path, raw bytes,
  and executable flag. `validate_package_surfaces` rejects unsafe
  `output_path`s (paths starting with `..` or absolute paths).
- `reverts-js::collect_static_resource_specifiers` /
  `collect_path_builder_calls` / `collect_file_url_source_location_rewrites`
  extract asset references from emitted source as AST facts.
- `reverts-pipeline::collect_required_asset_references` resolves the union
  of static-import, `require()`, `new URL(spec, import.meta.url)`, and
  dynamic `path.resolve`-style references against `InputBundle.assets`.
- `reverts-pipeline::audit_required_assets` raises
  `FindingCode::MissingRequiredAsset` when a referenced asset has no
  matching `AssetInput`.
- `reverts-pipeline::rewrite_emitted_asset_references` rewrites the literal
  spec inside the emitted source to the relative path from the module to the
  asset's `output_path`.
- `reverts-cli::extract-assets` discovers assets from Bun-embedded binaries
  or vendor directories and persists `AssetRow`s back into the SQLite input.

## Filesystem and External Access

Filesystem and external access are intentionally narrow:

| Crate | Filesystem access | Network access | External program access |
| --- | --- | --- | --- |
| `reverts-js` | no required access | no | no |
| `reverts-ir` | no | no | no |
| `reverts-observe` | no | no | no |
| `reverts-input` | adapter-specific only; core tests use in-memory rows | no required access | no |
| `reverts-graph` | no | no | no |
| `reverts-model` | no | no | no |
| `reverts-analyze` | no | no | no |
| `reverts-rollup-adapter` | yes, SQLite paths and optional JSON output | no | no |
| `reverts-package` | optional offline cache adapter only | no required access | no |
| `reverts-package-matcher` | no required access; reads package source rows from input | no | no |
| `reverts-planner` | no | no | no |
| `reverts-emitter` | no required access; returns `EmittedProject` | no | no |
| `reverts-pipeline` | no required access; returns `EmittedProject` | no | no |
| `reverts-cli` | yes, for reading input and writing output | optional only when explicitly configured | optional smoke workflows only |
| `reverts-fixtures` | temporary directories in tests only | no | no |

Required unit and integration tests must not depend on `node`, `npm`, network
access, real package installations, real project databases, or prior run state.

## Output-V2 Crate Split

The temporary `reverts-output-core` host has been retired. Output-v2 mechanisms
now live only in their final owner crates:

| Mechanism | Final owner |
| --- | --- |
| `InputBundle` and row conversion (incl. `AssetInput` / `AssetKind`) | `reverts-input` |
| `RevertsGraph`, `DefUseGraph`, `ImportExportGraph`, and `ControlFlowGraph` construction | `reverts-graph` |
| Program snapshots and enrichment records | `reverts-model` |
| Semantic naming, package-decision enrichment, shape-solution wiring, compiler-profile detection, and CFG-based audits (e.g. `UnreachableTopLevelCode`) | `reverts-analyze` |
| Package name, builtin, exports, and subpath resolution | `reverts-package` |
| Bundle-to-package AST-fingerprint matching | `reverts-package-matcher` |
| Import/export/local/synthetic binding planning | `reverts-planner` |
| Per-compiler AST lowerings (`CompilerLowering`), AST/codegen plumbing | `reverts-js` |
| AST emission and `EmittedProject` assembly | `reverts-emitter` |
| In-memory core pipeline orchestration | `reverts-pipeline` |
| CLI command wiring | `reverts-cli` |
| Self-contained builders for tests | `reverts-fixtures` |

New code must be added to the owner crate for its responsibility. Reintroducing
a parallel output-core host would create a second implementation path and is not
allowed.

## Test Placement

- Unit tests live next to the code that owns the behavior.
- Cross-crate behavior tests belong in the highest crate that coordinates the
  behavior being tested.
- Fixture builders shared by multiple crates belong in `reverts-fixtures`.
- Failure-mode tests should encode the structural problem directly: missing
  definition, invalid package subpath, callable shape, synthetic declaration,
  unparseable output, or entry dispatcher behavior.
- Smoke tests that execute external programs may exist separately, but they must
  not be required for core validation.

## Creation Order

The crate creation order should follow the data flow:

1. `reverts-input`
2. `reverts-graph`
3. `reverts-package`
4. `reverts-model`
5. `reverts-analyze`
6. `reverts-planner`
7. `reverts-emitter`
8. `reverts-pipeline`
9. `reverts-cli`
10. `reverts-fixtures` when shared fixture builders become duplicated across
   crate tests

Each crate should be introduced with at least one self-contained test that
captures its boundary contract.
