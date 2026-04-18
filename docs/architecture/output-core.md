# Output Core

The output core is organized around five invariants:

- Every read has a local definition, import, or explicit audit finding.
- Every synthetic reference is emitted atomically with its declaration or import.
- Every bare package import is backed by a package surface decision.
- Every unresolved binding is materialized according to its inferred shape.
- Entrypoint dispatchers do not statically import runtime-heavy modules.

Tests must extract failure modes into fixture-level data. They must not depend on
external programs, network access, real package installations, or real project
databases.

