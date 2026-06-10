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
| `reverts-package` | existing | Package surface and import specifier resolution |
| `reverts-model` | existing | Program and enriched-program handoff records |
| `reverts-analyze` | existing | Semantic naming, package-decision enrichment, and shape-solution wiring |
| `reverts-planner` | existing | Emit planning for imports, declarations, and exports |
| `reverts-emitter` | existing | Parseable emitted project generation from plans |
| `reverts-pipeline` | existing | Pure in-memory minimal decompilation loop |
| `reverts-cli` | existing | CLI argument contract for output v2 commands |
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
| `reverts-package` | analysis | Resolve package names, builtins, exports, subpaths, and package surfaces without emitting source |
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
      -> reverts-planner
          -> reverts-model
          -> reverts-package
      -> reverts-emitter
          -> reverts-planner
      -> reverts-observe
```

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
  `EnrichedProgram`: semantic names, package decisions, and shape solutions.
- `reverts-package` owns package-surface decisions. It may accept, reject, or
  classify package imports, but it must not generate import statements.
- `reverts-planner` owns output plans. It decides which imports, exports, local
  declarations, and synthetic bindings are needed before emission.
- `reverts-emitter` owns AST-backed source generation and emitted project
  assembly. It must not repair invalid plans after generation.
- `reverts-pipeline` owns the in-memory core loop. It coordinates crates but
  must keep filesystem, network, and external-program access out of core tests.
- `reverts-cli` owns command orchestration, argument parsing, paths, and process
  exit behavior. Core behavior should remain testable without invoking the CLI.
- `reverts-fixtures` owns fixture builders used by tests. It must not become a
  source of production behavior.

## Data Flow

The main data flow is:

```text
InputBundle
  -> ProgramModel
  -> EnrichedProgram
  -> EmitPlan
  -> EmittedProject
  -> AuditReport
```

The planner and emitter receive already-validated structural data. If a required
definition, import, package subpath, or binding shape is missing, the pipeline
reports a structured finding before writing files.

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
| `reverts-package` | optional offline cache adapter only | no required access | no |
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
| `InputBundle` and row conversion | `reverts-input` |
| `RevertsGraph`, `DefUseGraph`, `ImportExportGraph`, and `ControlFlowGraph` construction | `reverts-graph` |
| Program snapshots and enrichment records | `reverts-model` |
| Semantic naming, package-decision enrichment, and shape-solution wiring | `reverts-analyze` |
| Package name, builtin, exports, and subpath resolution | `reverts-package` |
| Import/export/local/synthetic binding planning | `reverts-planner` |
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
