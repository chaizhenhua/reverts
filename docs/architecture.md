# Reverts architecture

Reverts is organized as a layered compiler pipeline. The core rule is: data types
are stable and low-level, mechanisms transform those data types, and strategies
choose which mechanisms to run.

This page is the conceptual entry point. The **authoritative** crate map,
dependency directions (with machine-enforced layer ranks), data flow, and
filesystem/network access rules live in
[architecture/module-boundaries.md](architecture/module-boundaries.md). The
target pipeline is in
[architecture/decompilation-output-v2.md](architecture/decompilation-output-v2.md),
and accepted decisions are recorded in [adr/](adr/README.md).

## Crate layers

The full crate set (~17 crates) and its ranked dependency DAG are in
[module-boundaries.md](architecture/module-boundaries.md). At a glance, from the
foundation up:

- **Foundation** — `reverts-ir` (shared IDs, binding names, module/shape
  records; no `reverts-*` deps), `reverts-observe` (findings/telemetry),
  `reverts-js` (OXC parsing/codegen), `reverts-input` (input model + row
  conversion), `reverts-package-index` (fingerprint-index primitives).
- **Analysis** — `reverts-package` (package-surface policy), `reverts-graph`
  (graph/def-use/control-flow), `reverts-model` (program/enriched handoff),
  `reverts-analyze` (semantic names, shapes, package decisions), `reverts-bundle`
  (bundler-wrapper extraction), `reverts-package-matcher` (AST-fingerprint
  matching; delegates surface policy to `reverts-package`).
- **Planning / emission** — `reverts-planner`, `reverts-emitter`.
- **Orchestration / adapters** — `reverts-pipeline` (in-memory core loop),
  `reverts-rollup-adapter` (SQLite tool), `reverts-cli` (use cases, persistence,
  command parsing, reports). CLI composes lower layers; lower layers must not
  depend on CLI.
- **Test support** — `reverts-fixtures` (dev-dependency only).

## Data, mechanism, and strategy separation

- **Data**: `reverts-ir`, `reverts-input`, and package-domain structs in
  `reverts-package` define facts and evidence. These crates should avoid hidden
  orchestration or IO side effects.
- **Mechanisms**: AST normalization, graph extraction, source-cache surface
  resolution, package-surface indexing, and planner edits are deterministic
  transformations over explicit input data.
- **Strategies**: package matching, ownership promotion, externalization
  decisions, and CLI workflows decide which mechanisms run and in what order.
  Strategy code should be explicit about context and state and must not promote
  unproven import or source-suppression decisions.

## Package matching pipeline

`reverts-package-matcher::match_packages_with_pipeline` is a pass pipeline with
three explicit objects:

- `PackageMatchContext`: immutable inputs (`InputRows`, package sources, package
  filter) and shared limits.
- `PackageMatchState`: mutable pipeline state (versioned package report,
  per-module function fingerprints, function-level diagnostics).
- `PackageMatchPass`: individual passes such as versioned matching, function
  fingerprinting, cascade matching, structural bag ownership, dependency
  ownership, importable promotion, proven external-import target promotion, and
  cache-anchored surface finalization.

This makes pass order, shared state, and timing boundaries visible while keeping
matcher strategies separate from package public-surface policy.

## Boundary rules

- Domain policy that answers “is this package specifier public/importable?” lives
  in `reverts-package`, not `reverts-package-matcher` or CLI.
- Package-cache evidence is version scoped. Manifest, entry-path, and root-index
  proofs must be keyed by `(package_name, package_version)` before any
  external-import decision uses them.
- CLI use cases may load SQLite/cache evidence and apply policies, but should
  isolate repository loading, policy decisions, and mutation/application.
- Lower crates must not depend on higher orchestration crates. Architecture
  boundary tests enforce the most important crate dependency rules.

## Reference architecture patterns

- Compiler pipeline: parse/normalize/analyze/plan/emit phases with explicit pass
  ordering ([ADR 0001](adr/0001-use-ast-first-output-pipeline.md)).
- Functional core, imperative shell: pure domain mechanisms in lower crates;
  side effects and persistence in CLI/workflow adapters
  ([ADR 0005](adr/0005-enforce-single-direction-crate-layering.md)).
- Hexagonal boundaries: package cache, SQLite, network materialization, and CLI
  arguments are adapters around core mechanisms.
- Strategy / chain-of-responsibility: matching and ownership promotion are
  explicit passes rather than hidden unproven promotion branches.
- Typestate gating: validation boundaries are distinct types
  (`EmitPlan → ValidatedEmitPlan`, `EmittedProject → PreAcceptProject →
  AcceptedProject`) so skipping a check is a compile error
  ([ADR 0006](adr/0006-typestate-output-gating.md)).
