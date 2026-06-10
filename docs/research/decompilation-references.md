# Research References

This document records research directions that shape Reverts Next. The intent is
not to copy a specific decompiler, but to preserve the useful mechanisms:
explicit IR, graph invariants, constraint solving, package-surface decisions, and
semantics-preserving emission.

## Reverse Compilation Foundations

- Cristina Cifuentes, "Reverse Compilation Techniques", 1994.
  - Useful mechanisms: intermediate representation, control-flow graph,
    data-flow analysis, type recovery, and structured source regeneration.
  - Reverts Next effect: raw module rows should be normalized into a stable
    input bundle and graph before emission. Output code should be the result of
    planned definitions, imports, exports, and constraints rather than directory
    sweeps after writing files.

## Semantics-Preserving Structuring

- Edward J. Schwartz, JongHyup Lee, Maverick Woo, and David Brumley, "Native x86
  Decompilation Using Semantics-Preserving Structural Analysis and Iterative
  Control-Flow Structuring", USENIX Security 2013.
  - Useful mechanisms: structuring is valid only when the transformation
    preserves semantics; iterative recovery is guided by correctness conditions.
  - Reverts Next effect: output passes must expose pre-write invariants. For
    example, an import planner must emit usage and import atomically, and audit
    must reject missing definitions before code is accepted.

- "No More Gotos: Decompilation Using Pattern-Independent Control-Flow
  Structuring and Semantics", NDSS 2015.
  - Useful mechanisms: avoid relying only on syntactic patterns; derive regions
    from graph properties and semantic constraints.
  - Reverts Next effect: bundler wrappers, initializer regions, and enum-like
    IIFEs should be detected through AST/graph shape instead of text fragments.

## Compiler-Aware Recovery

- "Ahoy SAILR! There is No Need to DREAM of C: A Compiler-Aware Structuring
  Algorithm for Binary Decompilation", USENIX Security 2024.
  - Useful mechanisms: identify structures introduced by the compiler and
    reverse them according to their origin.
  - Reverts Next effect: treat bundlers and minifiers as compilation targets.
    Webpack, Rollup, esbuild, TypeScript, Babel, and terser patterns should feed
    explicit planner decisions instead of generic repair passes.

## Unified Program Graphs

- Yamaguchi et al., "Modeling and Discovering Vulnerabilities with Code Property
  Graphs", IEEE Symposium on Security and Privacy 2014.
  - Useful mechanisms: combine AST, CFG, and data-dependence information in a
    single queryable graph.
  - Reverts Next effect: build a lightweight Reverts graph with modules,
    bindings, reads, writes, call sites, imports, exports, and package-surface
    decisions. Audit findings become graph invariant failures:
    - read without definition or import;
    - synthetic reference without declaration;
    - duplicate top-level binding;
    - unresolved bare package import.

## Type and Shape Recovery

- Noonan, Loginov, and Cok, "Polymorphic Type Inference for Machine Code",
  PLDI 2016.
  - Useful mechanisms: recover types by collecting constraints and solving over
    a type lattice.
  - Reverts Next effect: binding recovery should use a shape lattice. Call sites
    produce callable constraints, member accesses produce namespace/object
    constraints, enum IIFEs produce enum-object constraints, and constructors
    produce constructor constraints. This directly prevents callable bindings
    from being materialized as plain objects.

- Anderson, Giannini, and Drossopoulou, "Towards Type Inference for JavaScript",
  and later static JavaScript type-inference work.
  - Useful mechanisms: JavaScript object, function, prototype, and property
    flows need dedicated shape reasoning rather than scalar types only.
  - Reverts Next effect: constraints should model callable values, namespace
    objects, class-like bindings, enum objects, and plain values separately.

## JavaScript Deobfuscation and Readability

- Raychev, Vechev, and Krause, "Predicting Program Properties from Big Code",
  POPL 2015.
  - Useful mechanisms: statistical prediction can improve names and type
    annotations from usage context.
  - Reverts Next effect: readability can be layered after correctness. Name
    suggestions must not alter import/export or binding-shape invariants.

- Fass et al., "HideNoSeek: Camouflaging Malicious JavaScript in Benign ASTs",
  CCS 2019, and related static JavaScript obfuscation/minification detection
  work.
  - Useful mechanisms: AST-level features can classify minification and
    obfuscation transformations.
  - Reverts Next effect: input classification can select specialized
    transformations for minified bundles without guessing late in the emitter.

## Clone and Package Matching

- Jiang, Misherghi, Su, and Glondu, "DECKARD: Scalable and Accurate Tree-Based
  Detection of Code Clones", ICSE 2007.
  - Useful mechanisms: normalize AST subtrees into vectors for scalable clone
    candidate retrieval.
  - Reverts Next effect: package and function matching can use AST-vector
    candidate retrieval before deeper equivalence checks. This should improve
    matching under minified names while keeping exact emit decisions separate.

## Direct Engineering Consequences

Reverts Next should implement these mechanisms as code-level boundaries:

- `reverts-js`: AST parsing, codegen, identifier handling, and source-type
  selection.
- `reverts-ir`: module, symbol, def-use, data-dependence, control-flow,
  import-export, package-surface, and binding-shape primitives.
- `reverts-graph`, `reverts-analyze`, `reverts-planner`, `reverts-emitter`,
  and `reverts-pipeline`: AST fact/control-flow construction,
  audit/enrichment, emit planning, AST-backed emission, and in-memory
  orchestration. The pipeline accepts in-memory input and returns emitted files
  plus structured findings.
- `reverts-observe`: structured telemetry and audit codes. Logs are diagnostic;
  audit reports decide acceptance.

Default tests should extract failure modes into small fixtures:

- unparseable output is an audit finding;
- a called unresolved binding is materialized as callable;
- an enum/IIFE initializer creates an initialized enum-object binding;
- absent package subpaths are not emitted as bare imports;
- illegal package names are rejected before emission;
- entry dispatchers do not statically import runtime-heavy modules.

Tests must not depend on external programs, npm installations, network access,
real package trees, or real project databases.
