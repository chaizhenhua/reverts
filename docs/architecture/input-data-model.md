# Input Data Model

This document defines the input-side data model for Reverts Next. The model is
implemented in `reverts-input` and is the boundary between source data adapters
and the output-v2 analysis pipeline.

## Meaning of InputBundle

`InputBundle` is a complete, validated, in-memory input snapshot for one
decompilation output run.

It represents what the output pipeline is allowed to know before graph
construction starts:

- which project is being generated;
- which source files belong to that project;
- which bundle modules exist;
- which symbols are known inside those modules;
- which module and package dependencies are known;
- which package attribution contracts are available;
- which accepted package surfaces are already available for bare-import resolution.

`InputBundle` is not a database schema, an output plan, an audit report, or a
repair layer. It is the normalized input contract consumed by later crates.

```text
DatabaseRows / fixture rows / future adapters
  -> InputRows
  -> InputBundle
  -> RevertsGraph
  -> DefUseGraph / ImportExportGraph
  -> PackageSurfaceResolution
  -> EmitPlan
```

## Model Layers

The input model has two layers:

| Layer | Type | Purpose |
| --- | --- | --- |
| Adapter rows | `DatabaseRows`, `ProjectRow`, `SourceFileRow`, `ModuleRow`, `SymbolRow`, `ModuleDependencyRow`, `PackageAttributionRow` | Capture data as loaded from a database or fixture without requiring the rest of the pipeline to know the storage backend |
| Pipeline input | `InputRows`, `InputBundle`, `ProjectInput`, `SourceFileInput`, `ModuleInput`, `SymbolInput`, `ModuleDependencyInput`, `PackageAttributionInput` | Store validated, graph-ready input records with stable IDs and explicit relationships |

Adapters are allowed to know about storage layout. The graph, package, planner,
and emitter crates should depend only on `InputBundle` and the pipeline input
types.

## Core Entities

### ProjectInput

`ProjectInput` identifies the output run scope.

| Field | Meaning |
| --- | --- |
| `id` | Stable project ID for this run |
| `name` | Human-readable project name |

Invariant: `name` must not be empty.

### SourceFileInput

`SourceFileInput` identifies a source file that may provide raw source text for
AST fact extraction.

| Field | Meaning |
| --- | --- |
| `id` | Stable source file ID |
| `path` | Source file path or logical path |
| `source` | Optional source text loaded by an adapter |

Invariant: `path` must not be empty. Source text is optional because some input
adapters may provide module slices or AST facts separately.

### ModuleInput

`ModuleInput` identifies one bundle module or package module.

| Field | Meaning |
| --- | --- |
| `id` | Stable `ModuleId` |
| `kind` | `Application`, `Package`, or `Builtin` |
| `original_name` | Original bundle/module key |
| `semantic_path` | Planned semantic identity used by graph and output layers |
| `source_file_id` | Optional owning source file |
| `package_name` | Optional package identity for package modules |
| `package_version` | Optional package version |

Invariants:

- `original_name` must not be empty.
- `semantic_path` must not be empty.
- `source_file_id`, when present, must reference an existing source file.
- package modules must have a package attribution contract.

### SymbolInput

`SymbolInput` records a known top-level or module-owned symbol.

| Field | Meaning |
| --- | --- |
| `module_id` | Owning module |
| `name` | Original or known binding name |

Invariants:

- `module_id` must reference an existing module.
- `name` must not be empty.

`SymbolInput`/the `symbols` table is **not** the source of truth for which
bindings are nameable. The semantic-naming universe is derived from the graph
and the emitted symbol index; see [naming-universe.md](naming-universe.md).

### ModuleDependencyInput

`ModuleDependencyInput` records dependency edges discovered before graph
construction.

| Field | Meaning |
| --- | --- |
| `from_module_id` | Module that owns the dependency |
| `target` | Either another module or a package specifier |

Invariants:

- `from_module_id` must reference an existing module.
- module targets must reference existing modules.
- package targets must be syntactically valid bare package specifiers.

### PackageAttributionInput

`PackageAttributionInput` records the explicit package contract for a module.
It exists so output planning does not infer package imports from path strings or
post-write scans.

| Field | Meaning |
| --- | --- |
| `module_id` | Module receiving the attribution |
| `package_name` | npm package name |
| `package_version` | Optional selected version |
| `subpath` | Optional package subpath evidence |
| `export_specifier` | Legal import specifier for external imports |
| `emission_mode` | How the package attribution may be materialized |
| `status` | Proposed, accepted, or rejected |
| `rejection_reason` | Required reason for rejected attribution |

Invariants:

- `module_id` must reference an existing module.
- `package_name` must be a valid package name.
- accepted attributions must include `package_version`.
- accepted `ExternalImport` attributions must include `export_specifier`.
- rejected attributions must include `rejection_reason`.

### PackageSurfaceInput

`PackageSurfaceInput` records accepted project-level import surfaces such as
`ws` or `rxjs/operators`. These rows are consumed by package-surface resolution
and must not be inferred from emitted strings after writing output. Agent
`accept_surface` proposals may create these rows only through the
`package-surface-decisions --apply` CLI gate. Agent `reject_surface` and
`block_surface` proposals remain in the `package_surface_decisions` ledger and
are consumed by `match-packages` before new surfaces are persisted. See
[package-surface-decisions.md](package-surface-decisions.md).

Invariants:

- package names must be valid.
- accepted surfaces must include an exact package version.
- `export_specifier` must be a valid bare package specifier whose package
  segment matches `package_name`.
- rejected/blocked Agent decisions do not become `PackageSurfaceInput` rows.

## Package Emission Modes

| Mode | Meaning | Runtime dependency |
| --- | --- | --- |
| `ExternalImport` | Planner may emit an import from the package | yes |
| `VendoredAsset` | Preserve concrete package asset/source in output | no |
| `ApplicationSource` | Treat as first-party source | no |
| `RuntimeGlue` | Treat as bundler/runtime support code | no |

Only `ExternalImport` requires a runtime dependency entry.

## Package Attribution Status

| Status | Meaning |
| --- | --- |
| `Proposed` | A package identity exists, but deterministic acceptance has not happened |
| `Accepted` | The attribution is allowed to participate in planning |
| `Rejected` | The attribution is invalid and must remain visible with a reason |

The planner should treat accepted attribution as usable contract data. Proposed
or rejected attribution should remain observable and should not silently become
bare imports.

## Validation Boundary

`InputBundle::from_database_rows` and `InputBundle::from_rows` are fail-closed
constructors. They reject structurally incomplete input before graph
construction.

Current validation covers:

- empty project/module/symbol/source fields;
- invalid row IDs;
- duplicate source file IDs;
- duplicate module IDs;
- module references to missing source files;
- symbol references to missing modules;
- dependency references to missing modules;
- invalid package specifiers;
- invalid package names;
- missing package attribution for package modules;
- accepted package attribution without package version;
- accepted external import attribution without export specifier;
- accepted package surface without package version;
- unsafe or mismatched package-surface specifiers;
- rejected package attribution without rejection reason;
- unsupported stored module kind.

## Relation to Existing Database Data

Existing database data can be used through a read-only adapter:

```text
SQLite rows
  -> DatabaseRows
  -> InputBundle::from_database_rows
```

The rest of the pipeline should not call storage APIs directly. This keeps old
database layout, future databases, and inline fixtures behind the same input
contract.

## What InputBundle Does Not Contain

`InputBundle` deliberately does not contain:

- output file contents;
- import/export plans;
- emitted source;
- audit findings;
- post-write repair state;
- runtime execution logs;
- package registry/network fetch state.

Those belong to later stages or separate adapters.

## Testing Contract

Input data model tests must be self-contained:

- build `InputRows` or `DatabaseRows` inline;
- do not open a real SQLite database;
- do not read `node_modules`;
- do not invoke `node`, `npm`, or `tsc`;
- assert specific failure modes directly.

The purpose of these tests is to prove that invalid input is rejected before the
graph, package, planner, or emitter layers can turn it into invalid output.
