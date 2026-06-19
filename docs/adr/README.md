# Architecture Decision Records

Each ADR captures one accepted decision: its context, the decision, and the
consequences. They are append-only — supersede an ADR with a new one rather than
rewriting history.

| ADR | Decision | Status |
| --- | --- | --- |
| [0001](0001-use-ast-first-output-pipeline.md) | Use an AST-first output pipeline | Accepted 2026-05-14 |
| [0002](0002-reject-post-write-repair.md) | Reject post-write repair | Accepted 2026-05-14 |
| [0003](0003-require-self-contained-failure-mode-tests.md) | Require self-contained failure-mode tests | Accepted 2026-05-14 |
| [0004](0004-bundler-aware-module-extraction.md) | Bundler-aware module extraction as a dedicated stage | Accepted 2026-05-17 |
| [0005](0005-enforce-single-direction-crate-layering.md) | Enforce single-direction crate layering | Accepted 2026-05-27 |
| [0006](0006-typestate-output-gating.md) | Gate output with typestate | Accepted 2026-05-27 |
| [0007](0007-skill-vs-reverts-code-boundary.md) | Split responsibilities between Skills and Reverts code | Accepted 2026-05-27 |
| [0008](0008-allow-in-memory-pre-accept-transforms.md) | Allow audited in-memory pre-accept transforms | Accepted 2026-05-27 |

For the broader picture see
[../architecture/module-boundaries.md](../architecture/module-boundaries.md)
(authoritative crate map and dependency rules) and
[../architecture/decompilation-output-v2.md](../architecture/decompilation-output-v2.md)
(target pipeline).
