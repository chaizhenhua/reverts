# Module Boundaries

This document defines the crate boundaries for Reverts Next. It is the
authoritative map for where new output-v2 code should live and which dependency
directions are allowed.

## Current Workspace

The workspace currently contains the foundation crates:

| Crate | Status | Responsibility |
| --- | --- | --- |
| `reverts-js` | existing | OXC parsing, code generation, source-type selection, and identifier utilities |
| `reverts-ir` | existing | Shared domain primitives: modules, bindings, binding shapes, package surfaces, and def-use basics |
| `reverts-observe` | existing | Audit reports, finding codes, severity, and telemetry event types |
| `reverts-output-core` | existing | Early output-v2 mechanisms: shape solving, package decisions, entry dispatching, parse audit, and emitter fixtures |

## Target Crate Map

The target architecture separates pure domain logic, graph construction,
planning, emission, and command orchestration.

| Crate | Layer | Responsibility |
| --- | --- | --- |
| `reverts-js` | foundation | Parse and format JavaScript/TypeScript through OXC and validate generated source |
| `reverts-ir` | foundation | Own shared IDs, binding names, module records, binding shapes, package surfaces, and graph-neutral domain records |
| `reverts-observe` | foundation | Own structured findings, audit reports, telemetry records, and acceptance diagnostics |
| `reverts-input` | input | Convert database rows, inline fixtures, and other source records into `InputBundle` |
| `reverts-graph` | analysis | Build `RevertsGraph`, `DefUseGraph`, and `ImportExportGraph` from input and AST facts |
| `reverts-package` | analysis | Resolve package names, builtins, exports, subpaths, and package surfaces without emitting source |
| `reverts-planner` | planning | Produce file-level import, export, local binding, and synthetic binding plans |
| `reverts-emitter` | emission | Convert accepted plans into AST-backed emitted files and `EmittedProject` |
| `reverts-cli` | orchestration | Provide `generate-project-v2` and user-facing command orchestration |
| `reverts-fixtures` | test support | Provide self-contained fixture builders for tests without production dependency edges |

## Dependency Direction

Production dependencies must flow downward through the architecture. Lower
layers must not depend on higher layers.

```text
reverts-cli
  -> reverts-emitter
      -> reverts-planner
          -> reverts-package
          -> reverts-graph
              -> reverts-input
                  -> reverts-js
                  -> reverts-ir
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
- `reverts-ir` owns shared domain types only. It should not know about file
  writing, CLI arguments, databases, package registries, or audit policy.
- `reverts-observe` owns structured diagnostics only. It should not decide how
  to recover from a finding.
- `reverts-input` owns conversion into `InputBundle`. It may validate input
  shape, but it must not plan imports, infer binding shapes, or emit source.
- `reverts-graph` owns graph construction and graph invariants. It reports
  unresolved reads, duplicate definitions, and dependency inconsistencies.
- `reverts-package` owns package-surface decisions. It may accept, reject, or
  classify package imports, but it must not generate import statements.
- `reverts-planner` owns output plans. It decides which imports, exports, local
  declarations, and synthetic bindings are needed before emission.
- `reverts-emitter` owns AST-backed source generation and emitted project
  assembly. It must not repair invalid plans after generation.
- `reverts-cli` owns command orchestration, argument parsing, paths, and process
  exit behavior. Core behavior should remain testable without invoking the CLI.
- `reverts-fixtures` owns fixture builders used by tests. It must not become a
  source of production behavior.

## Data Flow

The main data flow is:

```text
InputBundle
  -> RevertsGraph
  -> DefUseGraph
  -> ImportExportGraph
  -> PackageSurfaceResolution
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
| `reverts-package` | optional offline cache adapter only | no required access | no |
| `reverts-planner` | no | no | no |
| `reverts-emitter` | no required access; returns `EmittedProject` | no | no |
| `reverts-cli` | yes, for reading input and writing output | optional only when explicitly configured | optional smoke workflows only |
| `reverts-fixtures` | temporary directories in tests only | no | no |

Required unit and integration tests must not depend on `node`, `npm`, network
access, real package installations, real project databases, or prior run state.

## Output-V2 Crate Split

`reverts-output-core` currently hosts several early output-v2 mechanisms while
the workspace is small. New work should follow these final ownership rules:

| Mechanism | Final owner |
| --- | --- |
| `InputBundle` and row conversion | `reverts-input` |
| `RevertsGraph`, `DefUseGraph`, and `ImportExportGraph` construction | `reverts-graph` |
| Package name, builtin, exports, and subpath resolution | `reverts-package` |
| Import/export/local/synthetic binding planning | `reverts-planner` |
| AST emission and `EmittedProject` assembly | `reverts-emitter` |
| CLI command wiring | `reverts-cli` |
| Self-contained builders for tests | `reverts-fixtures` |

When a final owner crate exists, new code for that responsibility should be
added there instead of expanding `reverts-output-core`.

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
4. `reverts-planner`
5. `reverts-emitter`
6. `reverts-cli`
7. `reverts-fixtures` when shared fixture builders become duplicated across
   crate tests

Each crate should be introduced with at least one self-contained test that
captures its boundary contract.
