# Reverts architecture

Reverts is organized as a layered compiler pipeline. The core rule is: data types
are stable and low-level, mechanisms transform those data types, and strategies
choose which mechanisms to run.

## Crate layers

- `reverts-ir`: shared identifiers, binding names, module kinds, and other core
  data structures. It has no `reverts-*` dependencies.
- `reverts-input`: SQLite/project input model and conversion into rows/bundles.
  It may depend on `reverts-ir`, but not on matcher, planner, emitter, or CLI.
- `reverts-js`: JavaScript AST parsing/normalization helpers and JS-specific
  mechanisms.
- `reverts-package`: package-domain data and policy: accepted attribution checks,
  package surface indexing, public export/member proof kinds, Node builtin and
  package public-specifier rules.
- `reverts-package-matcher`: package matching strategies. It orchestrates passes
  over input rows and package source cache, but delegates package public-surface
  policy to `reverts-package`.
- `reverts-planner` / `reverts-emitter` / `reverts-pipeline`: generation
  planning, emission, and end-to-end pipeline mechanisms.
- `reverts-cli`: use cases, persistence wiring, command parsing, network/cache
  adapters, and user-facing reports. CLI can compose lower layers; lower layers
  must not depend on CLI.

## Data, mechanism, and strategy separation

- **Data**: `reverts-ir`, `reverts-input`, and package-domain structs in
  `reverts-package` define facts and evidence. These crates should avoid hidden
  orchestration or IO side effects.
- **Mechanisms**: AST normalization, graph extraction, source-cache surface
  resolution, package-surface indexing, and planner edits are deterministic
  transformations over explicit input data.
- **Strategies**: package matching, ownership promotion, externalization
  decisions, and CLI workflows decide which mechanisms run and in what order.
  Strategy code should be explicit about context and state rather than relying on
  fallback recovery paths.

## Package matching pipeline

`reverts-package-matcher::match_packages_with_pipeline` is a pass pipeline with
three explicit objects:

- `PackageMatchContext`: immutable inputs (`InputRows`, package sources, package
  filter) and shared limits.
- `PackageMatchState`: mutable pipeline state (versioned package report,
  per-module function fingerprints, function-level diagnostics).
- `PackageMatchPass`: individual passes such as versioned matching, function
  fingerprinting, cascade matching, structural bag ownership, dependency
  ownership, importable promotion, forced externalization, and cache-anchored
  surface finalization.

This makes pass order, shared state, and timing boundaries visible while keeping
matcher strategies separate from package public-surface policy.

## Boundary rules

- Domain policy that answers “is this package specifier public/importable?” lives
  in `reverts-package`, not `reverts-package-matcher` or CLI.
- CLI use cases may load SQLite/cache evidence and apply policies, but should
  isolate repository loading, policy decisions, and mutation/application.
- Lower crates must not depend on higher orchestration crates. Architecture
  boundary tests enforce the most important crate dependency rules.

## Reference architecture patterns

- Compiler pipeline: parse/normalize/analyze/plan/emit phases with explicit pass
  ordering.
- Functional core, imperative shell: pure domain mechanisms in lower crates;
  side effects and persistence in CLI/workflow adapters.
- Hexagonal boundaries: package cache, SQLite, network materialization, and CLI
  arguments are adapters around core mechanisms.
- Strategy / chain-of-responsibility: matching and ownership promotion are
  explicit passes rather than hidden fallback branches.
