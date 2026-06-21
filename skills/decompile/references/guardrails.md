# Decompile Guardrails and Common Mistakes

Load this reference when a decompile run is blocked, when an agent proposes a
shortcut, or when output generation/validation fails repeatedly.

## Out of scope for `decompile`

- Hand-edit generated `.ts` output to make `tsc` pass. Repeated patterns belong
  in backend transforms; one-off bugs need pipeline issues with regression tests.
- Run `pnpm install`, `npm install`, `tsc`, startup, or UI smoke as the final
  validation loop. That belongs to `reverts-decompile` after structural audits.
- Detect bundle structure with grep over the output tree. Use MCP/DB/AST-backed
  queries and structured parsing.
- Rename bundler/runtime helpers into business terms just to clear warnings.
- Delete or rewrite ReverTS migrations to bypass schema mismatch; surface the
  blocker and run the migration path.

## Common mistakes

- Classifying known npm packages as `app` without checking package fingerprints.
- Splitting classification and symbol naming into separate passes, which wastes
  a full source read cycle.
- Using 8-10 parallel agents; SQLite lock contention makes 3-5 agents faster.
- Leaving init-wrapper export symbols unnamed instead of using the camelCase fast
  naming convention.
- Spending effort on low-value locals before exported/owned global public
  surface is readable.
- Generating output before `public_surface` reaches 100%.
- Treating `missing_semantic_name == 0` as sufficient; always verify the full
  public-surface ratio.
- Skipping any Phase 5.1 audit: package misclassification scan, decl/import
  collision audit, or source-partition evidence audit.
- Trusting low-confidence import bindings for init-wrapper dependency chains.
- Replacing AST-backed scans with grep.
- Hand-editing generated imports to satisfy audits; record the finding, fix the
  codegen path, regenerate, and re-audit.
