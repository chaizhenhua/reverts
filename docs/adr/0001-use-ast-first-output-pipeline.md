# ADR 0001: Use an AST-First Output Pipeline

## Status

Accepted

## Context

Output defects are easier to detect before source text is written. Text-level
manipulation also makes it difficult to guarantee that imports, definitions,
exports, and usage sites remain consistent.

## Decision

The output pipeline uses explicit input records, graph facts, shape constraints,
package-surface decisions, emit plans, and AST-backed source emission. Source
transformations must use AST parsing and code generation whenever the AST API
supports the operation.

## Consequences

- Emission depends on structural data rather than directory sweeps.
- Missing definitions, invalid imports, and invalid binding shapes can be
  reported before writing files.
- The implementation complexity moves into graph construction, planning, and
  audit where it can be tested with small fixtures.
- Regex or string-based source manipulation is not acceptable for new output
  behavior when AST support is available.
