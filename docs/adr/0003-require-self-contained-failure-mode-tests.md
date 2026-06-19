# ADR 0003: Require Self-Contained Failure-Mode Tests

## Status

Accepted — 2026-05-14

## Context

Runtime checks through external programs can confirm end-to-end behavior, but
they are slow, environment-dependent, and poor at isolating the exact mechanism
that failed. The project needs tests that expose invalid output decisions before
they reach external execution.

## Decision

Core tests must use self-contained fixtures over in-memory data or temporary
directories. They must not depend on external programs, network access, real
package installations, real project databases, or state from prior runs.

## Consequences

- Historic failures are represented as minimal fixtures: callable shape,
  package subpath rejection, synthetic declaration audits, parse audits, and
  entry dispatcher behavior.
- External smoke tests may exist later, but they are not required for the core
  validation suite.
- New behavior should start with a failing unit or integration test that
  captures the structural failure mode.
- Tests remain fast, deterministic, and safe to run in parallel.

## References

- [decompilation-output-v2.md](../architecture/decompilation-output-v2.md)
  lists the historic runtime failures and the minimal fixtures that capture
  them.
- [module-boundaries.md](../architecture/module-boundaries.md) records which
  crates are allowed filesystem, network, or external-program access at all.
