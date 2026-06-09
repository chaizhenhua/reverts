# Decompilation Output V2 Architecture

This document defines the target architecture for Reverts Next. It turns the
research basis and observed failure modes into module boundaries, invariants,
failure-mode tests, and an implementation plan.

Crate ownership and allowed dependency directions are defined separately in
[module-boundaries.md](module-boundaries.md).
The input-side data model is defined in
[input-data-model.md](input-data-model.md).

## Background

The previous output path exposed several classes of defects only after files had
already been written and external programs tried to execute them:

- callable bindings could be emitted as non-callable values;
- absent package subpaths could become bare package imports;
- synthetic references could be used without a declaration or import;
- entry files could statically import unavailable runtime-heavy modules;
- generated files could be syntactically invalid.

Reverts Next moves these failures before writing files. The output path must
build explicit input, graph, shape, package, emit, and audit records. A project
writer only writes files that have passed the structural checks owned by the
upstream pipeline.

## Goals

- Represent decompilation input as stable in-memory data before output planning.
- Use AST parsing and code generation for source handling whenever the AST API
  supports the operation.
- Model definitions, reads, imports, exports, calls, member access, and package
  surfaces explicitly.
- Infer binding shapes before materialization so emitted values match how they
  are used.
- Reject or audit invalid imports, missing definitions, duplicate bindings, and
  unparseable files before accepting output.
- Keep core tests self-contained and independent of external programs, real
  databases, package installations, network access, and prior run state.
- Implement the system in vertical slices, validating and committing each slice.

## Non-Goals

- Do not include post-write repair, final sweep, rescue, or string-rewrite
  passes in the output core.
- Do not hide upstream emit defects behind silent fallback behavior.
- Do not make `node`, `npm`, a real SQLite database, or `node_modules` part of
  required unit or integration coverage.
- Do not optimize readability before correctness. Name recovery can be layered
  only after structural invariants are enforced.

## Research Mapping

The detailed bibliography lives in
[../research/decompilation-references.md](../research/decompilation-references.md).
The architectural consequences are:

| Research direction | Project decision | Expected effect |
| --- | --- | --- |
| Reverse compilation IR and data-flow recovery | Normalize raw project data into `InputBundle`, `RevertsGraph`, and `DefUseGraph` before emission | Output defects become graph or constraint failures instead of late runtime failures |
| Semantics-preserving structuring | Emit through an AST-oriented planner and code generator | Reduces text-level fixes and keeps transformations tied to program structure |
| Compiler-aware recovery | Treat bundlers, minifiers, TypeScript, Babel, and package managers as compilation targets | Bundler artifacts feed explicit decisions rather than generic repairs |
| Unified program graphs | Store AST facts, module dependencies, definitions, reads, calls, imports, exports, and package decisions in queryable records | Audits can report missing definitions, duplicate bindings, and unresolved package imports directly |
| Type and shape constraints | Solve a binding-shape lattice before materializing placeholders | Prevents call sites from receiving plain objects and preserves enum/class/namespace intent |
| Readability prediction | Run naming and readability improvements after correctness | Avoids readable output that breaks imports, exports, or binding shapes |
| AST clone matching | Use normalized AST/function signatures for package and function candidates | Improves matching under minified names without coupling matching to final emission |

## Target Pipeline

```text
InputBundle
  -> AstFactExtractor
  -> RevertsGraph
  -> DefUseGraph
  -> ProgramModel
  -> BindingShapeSolver
  -> PackageSurfaceResolver
  -> EnrichedProgram
  -> ImportExportPlanner
  -> AstEmitter
  -> ParseAudit / SynthesisAudit
  -> ProjectWriter
```

### InputBundle

`InputBundle` is the clean boundary between data sources and the output core. It
contains project metadata, source files, modules, symbols, dependencies, package
attribution, and optional raw source fragments. It does not perform output
repair. Its detailed field and validation contract is documented in
[input-data-model.md](input-data-model.md).

### AstFactExtractor

The extractor parses source with `reverts-js` and emits facts such as top-level
definitions, references, call sites, member access, exports, and wrapper
regions. Failures become structured findings rather than panics.

### RevertsGraph

The graph connects modules, files, symbols, package attribution, imports,
exports, and AST facts. It is the query surface for audit and planning.

### DefUseGraph

The def-use graph records binding definitions, imports, reads, writes, and usage
constraints. Missing definitions, duplicate definitions, and unresolved reads
are graph-level findings.

### ProgramModel

The model is the immutable handoff from raw input and graph construction into
analysis. It owns the `InputBundle` plus `RevertsGraph`, so downstream crates
can query modules, symbols, dependencies, definitions, and package attribution
without reading from a database or reparsing external state.

### BindingShapeSolver

The solver maps usage constraints to `BindingShape` decisions. A call site must
produce a callable-compatible binding; enum initializers must produce initialized
objects; constructors and class-like usage must not collapse into plain values.

### PackageSurfaceResolver

The resolver converts package attribution into `ImportDecision` records. A bare
import is allowed only when the package name is valid and the requested subpath
is present in the package surface. Otherwise the planner must choose a local
module, local shim, or rejected decision with an audit finding.

### EnrichedProgram

`EnrichedProgram` is the complete analysis output consumed by planning. It
contains semantic module paths, semantic binding names, package import
decisions, and binding-shape decisions. A planner should not reach back into the
input source to rediscover these facts.

### ImportExportPlanner

The planner produces an emit plan before files are created. Imports,
declarations, exports, and usage sites are planned atomically so the emitter
cannot reference a synthetic binding without also declaring or importing it.

### AstEmitter

The emitter materializes the plan into AST-backed source. It should not patch
files after writing them. If the plan is incomplete, emission fails or reports
an audit finding before `ProjectWriter` runs.

### ParseAudit and SynthesisAudit

Audits validate emitted files and planner invariants:

- each emitted file must parse with OXC;
- every read must have a definition, import, or finding;
- every synthetic reference must have a matching same-file declaration or import;
- every bare import must have a package-surface decision;
- duplicate top-level bindings must be reported;
- any allowed fallback marker must remain visible as an audit signal.

### ProjectWriter

The writer only persists accepted emitted files and manifests. It does not infer
missing declarations, patch imports, or run repair passes.

## Core Data Structures

| Type | Responsibility | Key invariant |
| --- | --- | --- |
| `InputBundle` | Complete in-memory input for one output run | Contains enough project/module/package context to plan output without live database reads |
| `ProjectInput` | Project-level metadata | Stable project identity and output target metadata |
| `SourceFileInput` | Source file identity and optional text | File paths are normalized before graph construction |
| `ModuleInput` | Bundle module identity and ownership | Every module belongs to a project and optionally to a source file |
| `SymbolInput` | Symbol identity, module ownership, and source range metadata | Symbol ownership is explicit, not inferred during emission |
| `ModuleDependencyInput` | Module-to-module and module-to-package edges | Dependency target kind is explicit |
| `PackageAttributionInput` | Package name, version, and subpath evidence | Package name and subpath can be validated before bare import emission |
| `ProgramModel` | Input plus constructed graphs for one output run | Downstream analysis never depends on live database reads |
| `SemanticNameMap` | Module-path and binding-name recovery results | Sanitized names are deterministic and valid identifiers |
| `EnrichedProgram` | Analysis handoff to planning | Package decisions, semantic names, and shapes are available before emission |
| `DefUseGraph` | Definitions, imports, reads, and constraints | Unresolved reads are observable findings |
| `BindingConstraint` | Usage-derived shape evidence | Call/member/construct/class/enum usage is not lost |
| `BindingShapeSolution` | Solved shape per binding | Materialization follows the strongest required shape |
| `PackageSurface` | Known legal package subpaths | Bare imports are accepted only against this surface |
| `ImportDecision` | External, local, shim, or rejected import choice | Rejected imports cannot be emitted as bare imports |
| `EmitPlan` | File-level declarations, imports, exports, and body plan | Usage and declaration/import are atomic |
| `AuditReport` | Structured acceptance and diagnostic output | Audit findings decide whether output is accepted |

## Output Invariants

- A binding used as a function is emitted as callable or reported before write.
- A binding used as a constructor is emitted as constructor/class-like or
  reported before write.
- An enum-like initializer emits an initialized object binding.
- A bare package import requires a valid package name and accepted subpath.
- A synthetic reference has a same-file declaration or import.
- An import and its usage are planned as one unit.
- A generated entry dispatcher does not statically import runtime-heavy modules.
- Every emitted source file parses with OXC.
- The writer never introduces semantic repairs after audit.

## Failure Modes and Minimal Tests

Each historic runtime failure should become a small fixture-level test.

| Failure mode | Extracted test | Required behavior |
| --- | --- | --- |
| Called binding emitted as object | Def-use graph contains a call constraint for an unresolved binding | Shape solver returns callable and emitter materializes callable source |
| Missing package subpath | Package surface lacks the requested subpath | Resolver rejects the bare import instead of emitting it |
| Invalid package name | Package name contains illegal package syntax | Resolver rejects it before planning |
| Synthetic reference without declaration | Emit plan contains a synthetic usage without source binding | Audit reports `SyntheticReferenceWithoutDeclaration` |
| Static runtime import in entry | Entry dispatcher references runtime-heavy behavior | Dispatcher uses dynamic import only |
| Syntax-broken output | Emitted source contains invalid JavaScript/TypeScript | Parse audit reports `UnparseableOutput` |

These tests must be pure Rust tests over in-memory data. Optional smoke tests
may execute external programs later, but they must not be required for the core
validation suite.

## Implementation Plan

### Slice 1: Input Bundle

- Add `reverts-input`.
- Define project, file, module, symbol, dependency, and package attribution
  input records.
- Add pure row-to-input conversion tests using inline fixtures.
- Validate invalid package names, absent attribution, and module ownership.

### Slice 2: Graph Construction

- Build `RevertsGraph` and `DefUseGraph` from `InputBundle`.
- Add tests for unresolved reads, duplicate definitions, imports, calls, and
  member access.

### Slice 3: Shape Solving

- Extend binding-shape constraints for callable, constructor, class-like,
  namespace, enum, and value usage.
- Add conflict and precedence tests.

### Slice 4: Package Surface Resolution

- Separate package-surface decisions from network or filesystem access.
- Add tests for accepted subpaths, absent subpaths, invalid names, and local
  module fallback decisions.

### Slice 5: Emit Planning

- Introduce `EmitPlan` as the only input accepted by the emitter.
- Add tests that import/declaration and usage are produced atomically.

### Slice 6: AST Emission and Audit

- Emit source through AST/codegen boundaries.
- Run parse audit on every emitted file.
- Add synthesis audit tests for missing synthetic declarations and fallback
  visibility.

### Slice 7: Project Writer

- Persist only accepted emitted files and manifests.
- Keep writer tests inside temporary directories.
- Confirm the writer performs no repair or inference.

## Validation and Commit Policy

Each vertical slice must follow this sequence:

1. Add or update the smallest self-contained failing test for the behavior.
2. Implement the production mechanism.
3. Run:

   ```bash
   cargo fmt --check
   cargo clippy --workspace --locked -- -D warnings
   cargo test --workspace --locked
   ```

4. Commit the working slice with a single-line English conventional commit.

Documentation-only slices should still validate formatting-sensitive docs where
possible and run the workspace checks when the Rust code or workspace manifests
are touched.

Real database or real project validation is a temporary smoke workflow, not a
required test fixture. Required tests must not read a live database, checked-out
project, package installation, network resource, or prior output directory. If
manual validation against a real project exposes a defect, reduce the defect to
the smallest in-memory `InputRows`, `DatabaseRows`, or in-memory SQLite fixture
that reproduces the same failure mode, then keep that fixture in the normal
test suite.

## Risks and Open Questions

- The exact database-to-`InputBundle` contract must be audited before adding
  real database adapters.
- Package-surface data needs an offline representation before any network-backed
  verifier is reintroduced.
- Shape conflicts need a policy for reporting ambiguous usage without emitting
  misleading code.
- Function and package matching may need separate crates to avoid coupling
  candidate search to output planning.
- Readability normalization must wait until structural correctness is stable.
