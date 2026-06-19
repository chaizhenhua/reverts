# Architecture refactor roadmap

Captures the architectural debts in reverts-next and a phased plan for
addressing them. Each phase lists concrete extractions, expected line moves,
test-gate impact, and what it unblocks. Phases are independent — each can
land on its own and unblocks downstream work, but they're ordered by
combined "leverage × tractability".

## Current state (2026-05-27)

The big-three monolith split is **substantially complete**. Actual `lib.rs`
sizes today versus the 2026-05-23 baseline that this roadmap was written
against:

| crate | `lib.rs` then | `lib.rs` now | total | files | status |
|---|---:|---:|---:|---:|---|
| `reverts-planner` | 33,900 | **8,222** | 41,927 | 60 | 🟢 Phase 3 largely done; remaining work is incremental |
| `reverts-package-matcher` | 13,907 | **95** | 20,236 | 55 | 🟢 Phase 2 done (lib.rs is now re-exports) |
| `reverts-cli` | 13,810 | **852** | 27,271 | 39 | 🟢 Phase 1 done (target was ~600) |
| `reverts-js` | 8,671 | **148** | 16,277 | 57 | 🟢 |
| `reverts-pipeline` | 3,753 | **2,959** | 4,443 | 7 | 🟡 Phase 4 partially done |
| `reverts-graph` | 3,918 | 3,932 | 6,920 | 16 | 🟢 |

Status correction: earlier session notes below say crate-boundary rules were
**not** machine-enforced ("except machine-enforced crate-boundary tests"). That
is now **out of date** — the full layering DAG is enforced by
`crates/reverts-cli/tests/architecture_boundaries.rs` (see
[ADR 0005](../adr/0005-enforce-single-direction-crate-layering.md)). Treat the
phase narratives below as a historical log of how the split happened, not as the
current backlog.

Remaining live debt (small, incremental): the planner's `compute_modules` per-
module passes can still be split further; `reverts-pipeline` Phase 4 file
extraction is unfinished; CLI package-source/cache use-cases are still
`reverts-cli`-local module seams that could become dedicated crates
([ADR 0007](../adr/0007-skill-vs-reverts-code-boundary.md) treats that promotion
as optional). The byte-surgery debt is tracked in
[module-boundaries.md](module-boundaries.md#byte-surgery-debt-registry).

## Historical baseline — monolith sizes (2026-05-23)

| crate | `lib.rs` lines | total | files | severity |
|---|---:|---:|---:|---|
| `reverts-planner` | **33,900** | 33,900 | 1 | 🔴 single-file crate |
| `reverts-cli` | **13,810** | 14,388 | 7 | 🔴 (was 14,173 before `args.rs` extract; now smaller) |
| `reverts-package-matcher` | **13,907** | 18,611 | 8 | 🔴 |
| `reverts-js` | 8,671 | 15,435 | 37 | 🟡 split but file still big |
| `reverts-graph` | 3,918 | 6,614 | 14 | 🟢 |
| `reverts-pipeline` | 3,753 | 3,753 | 1 | 🟡 single-file |
| others | < 3000 | | | 🟢 |

The 3 red crates account for **~62k of 75k total lines** — 83% of code in
3 monolithic files. CLAUDE.md flags `reverts-cli` as "temporary host"; the
matcher and planner monoliths are organic accumulation that didn't get
refactored as they grew.

## Phase 0 (DONE)

### Session of 2026-05-23 (collaborative — concurrent with matcher Phase 2)

`reverts-cli` Phase 1 — 7 commits, `lib.rs` 14,173 → 13,069 (-1,104 lines, -7.8%):

1. `a1ace2b` `args.rs` — 6 arg structs + parsers + helpers (384 lines)
2. `13f1200` `commands/runtime_inventory.rs` — runner + 2 print helpers (218 lines)
3. `f4aa3d9` `commands/{package_cache,extract_assets}.rs` — 4 runners + 1 print
4. `453fbb8` `commands/match_packages.rs` — 2 runners + blocker print
5. `40cd251` `persistence/{synthetic_modules,function_attributions}.rs`
6. `e6f8cff` `persistence/package_surfaces.rs`

In parallel the user landed `reverts-package-matcher` Phase 2 (`9d763a9`,
`02c3674`, `8ebf4c7`, `c63c1c3`, `375298e`, `6fdc187`, `0a02072`) — 7
matcher strategies extracted into `strategies/` modules. `reverts-pipeline`
also got `runtime_dependencies.rs` factored out (`5271087`). All 1,401
tests stayed green throughout.

### Session of 2026-05-23 (continuation — matcher tail + pipeline + planner kickoff)

`reverts-package-matcher` Phase 2 tail (1 commit, lib.rs ~13,907 → ~12,700):

- `a119c53` `force_externalize.rs` — last-resort externalization pass +
  several `pub(crate)` bumps so the index/cache helpers stay reachable

`reverts-pipeline` Phase 4 (4 commits, `lib.rs` 3,753 → 2,699 = -28%):

- `82094d9` `audit.rs` — 3 audit passes + 7 helpers (parse + binding-
  shape + namespace-member consistency)
- `3e8768d` `assets.rs` — asset reference collection / audit / rewrite
  (including ripgrep-vendor dynamic detector)
- `178b735` `source_rewrites.rs` — `import.meta.url` canonicalization,
  static template-literal folding, string-literal value rewriter
- `12cef62` `output_paths.rs` — `module_output_paths` +
  `relative_asset_specifier`

`reverts-planner` Phase 3 kickoff (8 commits, `lib.rs` 33,930 → 32,998 = -2.7%):

- `3732f04` `runtime_setter_migration_blocker.rs` — public diagnostic
  surface (`RuntimeSetterMigrationBlockerReport` + sub-types)
- `4028f2f` `compiler_recovery.rs` — `SourceCompilerStrategy`,
  `CompilerRecoveryAction`, `CompilerRecoveryDecision`
- `c183cd6` `statements.rs` — pure JS statement formatters (named import,
  default+named import, namespace import, named export/reexport,
  variable declaration, runtime helper setter, lazy module/value helper
  source, node_require_prelude)
- `70172c9` `relative_paths.rs` — POSIX relative-import-specifier
  computation (`relative_import_specifier` + segment helpers)
- `251334e` `plan_error.rs` — `PlanError` (Display + Error impls)
- `20cbeba` `byte_lexer.rs` — byte-walking JS lexer helpers
  (`skip_ws`, `skip_quoted`, `skip_template_literal`,
  `skip_regex_literal`, `looks_like_regex_literal`, `find_matching_*`,
  `find_byte`, `expect_arrow`)
- `16c9526` `identifiers.rs` — `is_identifier_like`, `parse_identifier`,
  `parse_identifier_after_keyword/function_keyword`, `keyword_at`
- `1520b0e` `statement_parsers.rs` — reverse-parsers
  (`parse_generated_named_import/default_import/named_reexport/named_export_statement`)
  paired with `coalesce_consecutive_uninitialized_var_declarations`

All workspace tests stayed green throughout (84 pipeline + 290 planner +
376 matcher + analyze/observe/etc).

### Session of 2026-05-23 (continuation — planner phase 3-A starts)

Continuing the planner extraction with Phase 3-A "pure subsystem"
extractions (4 commits, `lib.rs` 32,998 → 32,278 = -720 lines):

- `29aa813` `runtime_namespace_rewrite.rs` —
  `RuntimeNamespaceMemberAccessRewrite` + `rewrite_runtime_namespace_member_accesses`
  + `runtime_namespace_member_access_site_is_read_only` (148 lines).
  Bumps `apply_text_edits`, `previous_non_ws`, `collect_member_access_only`
  to `pub(crate)`.
- `991f36b` follow-up: qualifies the affected unit-test path to
  `super::runtime_namespace_rewrite::…` so the lib-side `use` doesn't
  warn in non-test build profile.
- `a717af5` `pure_reexport_bypass.rs` — `PureReexportBypassPlan` +
  `pure_reexport_bypass_plan` + `folded_stub_modules_with_internal_consumers`
  (140 lines). Bumps `SourceModuleWiring`, `RuntimeLazyFoldPlan` and
  their nested types/fields to `pub(crate)`; `pure_named_barrel_reexports`
  similarly.
- `50dff95` `runtime_helper_writes.rs` — `UpdateOperator`,
  `UpdatePosition`, `update_operator_at`, `is_simple_update_target`,
  `find_assignment_rhs_end`, `runtime_helper_update_expression`,
  `rewrite_runtime_helper_writes` (280 lines). Bumps
  `variable_declaration_binding_starts`,
  `rewrite_object_destructuring_helper_writes`,
  `rewrite_array_destructuring_helper_writes` to `pub(crate)`.
- `22e7e76` `destructure_writes.rs` —
  `object_destructuring_assignment_writes`,
  `array_destructuring_assignment_writes`,
  `rewrite_object_destructuring_helper_writes`,
  `rewrite_array_destructuring_helper_writes`,
  `bracket_starts_member_access`, `split_top_level_properties`,
  `parse_object_pattern_bindings`, `parse_array_pattern_bindings`,
  `parse_pattern_binding_identifier`, `property_access_source`
  (260 lines).

Phase 3-A remaining (6 of 10 done):

- `eager_safe_analysis.rs` (needs 5 helper-fn `pub(crate)` bumps)
- `runtime_prelude_imports.rs` (entangled with `BindingOwnerPlan` etc.)
- `runtime_setter_migration.rs` (needs A-9 first — depends on
  `compute_runtime_var_migration_plan`)
- `runtime_singleton_inline.rs` (~500 lines, depends on
  `SourceModuleWiring`, `LoweredRuntimeModuleSource`,
  `RuntimeLazyFoldPlan`, `RuntimeVarMigrationPlan`)
- `runtime_var_migration.rs` (~600 lines — central piece)
- `package_runtime.rs` (~750 lines — biggest single subsystem)

The remaining six all sit on top of `PlannerAnalysis` internals
(`SourceModuleWiring`, `LoweredRuntimeModuleSource`,
`BindingOwnerPlan`, `RuntimePreludeDirectImport`,
`RuntimeVarMigrationPlan`, `PackageRuntimeIslandPlan`,
`RuntimeSingletonInlinePlan`), which means each extraction needs
either:

1. Multiple `pub(crate)` bumps on the analysis types **and** the
   middle-tier helpers they call, or
2. Doing Phase 3-B (`analysis.rs`) first so those types have a stable
   home before the subsystems reference them.

Recommended next move: do Phase 3-B before continuing Phase 3-A so the
later A-tier extractions all import `super::analysis::*` cleanly
instead of relying on long `pub(crate)` chains scattered through
lib.rs.

### Session of 2026-05-23 (continuation 2 — phase 3-B kickoff + utility consolidation)

8 commits, `lib.rs` 32,278 → 31,358 = -920 lines (-2.8%):

- `f84cede` move `skip_non_code_at` + `arg_text_is_single_expression`
  into `byte_lexer.rs` (shared byte-walking utilities)
- `9a5a54e` move `inline_internal_setter_calls`* into
  `runtime_helper_writes.rs` (logical pairing with the write rewriter)
- `fba5c07` extract `SourceModuleFacts` analysis bus into
  `source_module_facts.rs` (3 indexes + `from_program`)
- `3286cde` move `runtime_namespace_export_statement` +
  `noop_function_statement` + `property_key_source` into `statements.rs`
- `136ab3f` extract `BindingOwnerPlan` / `BindingOwner` /
  `RuntimeOwnerImportPartition` into `binding_owner.rs` (175 lines).
  Required `pub(crate)` field bumps on `RuntimeVarMigrationPlan`,
  `RuntimeVarMigration`, `RuntimeOwnedSnippetMigration`,
  `PackageRuntimeOwner`, `PackageRuntimeIslandPlan`,
  `RuntimePreludeDirectImport`, `RuntimePreludeDirectImportKind`.
- `259aa14` extract runtime-helper body strip helpers
  (`classify_migratable_var_declaration`,
  `strip_runtime_var_declarations`, `strip_runtime_snippet_sources`,
  `strip_runtime_namespace_export_sources`,
  `find_runtime_source_chunk`) into `runtime_helper_strip.rs`
- `7d95c4d` extract top-level import-declaration coalescing
  (`coalesce_top_level_import_declarations` + 8 supporting parsers and
  `MergeableImportDeclaration`) into `import_coalesce.rs` (430 lines —
  biggest single extraction this session)

Phase 3-A: 4 of 10 still pending (eager_safe_analysis,
runtime_prelude_imports, runtime_setter_migration computation,
runtime_singleton_inline, runtime_var_migration, package_runtime). Most
have heavy dependency chains; the remaining work belongs in future
sessions where each extraction can stand alone as a focused commit.

### Session of 2026-05-23 (continuation 4 — big-subsystem extractions)

4 commits, `lib.rs` 31,131 → 28,078 = **-3,053 lines (-9.8%)**:

- `b707503` `eager_safe_analysis.rs` — full cross-module eager-safety
  pipeline (815 lines) — `EagerSafeAnalysis` + `compute_eager_safe_analysis`
  + `compute_consumer_usage_scopes` + `compute_consumer_call_forms` +
  `singleton_scc_modules` + `compute_thunk_wrapped_exports` +
  `predict_delazifiable_exports` (with fixpoint + classifier) +
  `consumer_eagerified_imports` + `rewrite_eagerified_call_sites`.
- `2ae1a1c` `runtime_var_migration.rs` — entire Phase-10 migration
  subsystem (1,135 lines): `RuntimeVarMigrationPlan` + 22-method impl +
  `RuntimeVarMigration` + `RuntimeOwnedSnippetMigration` +
  `compute_runtime_var_migration_plan` (the 412-line core algorithm).
- `8e296ab` `runtime_singleton_inline.rs` — singleton consumer inline
  subsystem (560 lines): `RuntimeSingletonInlineSnippet` /
  `…SnippetSource` / `…Plan` / `…Context` / `…EmitContext` +
  `runtime_singleton_inline_plan` +
  `resolve_runtime_singleton_inline_snippet` +
  `emit_runtime_singleton_inline_helpers` +
  `partition_runtime_singleton_inline_bindings` +
  `runtime_singleton_inline_consumer_has_name_conflict` etc.
- `b90d17d` `package_runtime.rs` — package-runtime island planning +
  emission (780 lines): `PackageRuntimeOwner` /
  `PackageRuntimeHelperKey` / `PackageRuntimeHelperUsage` /
  `PackageRuntimeIslandPlan` / `PackageRuntimeClosureGate` /
  `PackageRuntimeImportEmitter` + `package_runtime_island_plan` +
  `package_runtime_closure_is_safe` +
  `partition_package_runtime_bindings` +
  `emit_package_runtime_helper_import` +
  `inline_package_runtime_helper_into_single_consumer` +
  `emit_package_runtime_helper_files` +
  `push_packed_runtime_helper_imports`.

**Phase 3-A is now essentially complete.** All 10 target subsystems
have their own modules:

| Module | Lines | Subsystem |
|---|---|---|
| `byte_lexer.rs` | 357 | JS byte-walking lexer helpers |
| `identifiers.rs` | 134 | Identifier shape / keyword helpers |
| `statements.rs` | 227 | Pure JS statement formatters |
| `statement_parsers.rs` | 152 | Reverse parsers + var coalescer |
| `relative_paths.rs` | 64 | POSIX import specifiers |
| `plan_error.rs` | 62 | PlanError type |
| `compiler_recovery.rs` | 129 | Compiler recovery decision types |
| `runtime_setter_migration_blocker.rs` | 195 | Blocker report types |
| `runtime_namespace_rewrite.rs` | 168 | Namespace member-access rewriter |
| `pure_reexport_bypass.rs` | 146 | Barrel re-export bypass |
| `destructure_writes.rs` | 275 | Destructuring assignment rewriters |
| `runtime_helper_writes.rs` | 415 | Write-to-setter + inline-setter rewriters |
| `runtime_helper_strip.rs` | 188 | Helper-body strip helpers |
| `source_module_facts.rs` | 70 | SourceModuleFacts bus |
| `binding_owner.rs` | 189 | BindingOwnerPlan canonical owner table |
| `import_coalesce.rs` | 422 | Top-level import coalescing |
| `runtime_source_read.rs` | 225 | RuntimeSourceReadIndex builder + queries |
| `eager_safe_analysis.rs` | 816 | Cross-module eager-safety pipeline |
| `runtime_singleton_inline.rs` | 563 | Singleton inline subsystem |
| `runtime_var_migration.rs` | 1,135 | Runtime-var migration subsystem |
| `package_runtime.rs` | 781 | Package-runtime island plan + emission |
| `lib.rs` | **28,078** | EmitPlan / PlannedFile / ImportExportPlanner + per-module loop |

### Session of 2026-05-23 (continuation 10 — source-refs computation chain)

5 commits, method body 1,499 → 1,424 lines (-75 lines):

- `0acea3b` `adjust_remaining_runtime_helpers` +
  `adjust_written_runtime_helpers` — two single-purpose set arithmetic
  helpers that previously inlined union/difference chains.
- `ea3fa80` `compute_namespace_member_rewrite` +
  `compute_node_builtin_require_helpers` +
  `compute_node_builtin_require_rewrite` — three pure functions over
  the lowered source / planned-binding state that compute the
  namespace-member and node-builtin-require rewrites.
- `b0cba0a` `compute_source_runtime_refs` — gathers runtime identifier
  refs + exports + named-export bindings into a single set.
- `95d7b32` follow-up: drop redundant `into_iter()` on `exports_for`
  return (clippy `useless_conversion`).
- `b709a68` `filter_remaining_helpers_by_write_rewrite` — the
  conditional-rewrite-with-refs walk that filters
  `remaining_runtime_helpers` against the post-rewrite identifier set.

Cumulative on `plan_enriched_program`: 2,155 → 1,424 lines
(-731 lines / -34%).

### Session of 2026-05-23 (continuation 9 — OwnerMigrationState builder)

1 commit, method body 1,522 → 1,499 lines (-23 lines):

- `9e1f4ab` `OwnerMigrationState` struct + `from_plan` builder. Collapses
  15 sequential `runtime_var_migrations.X_for_owner(module.id)` calls
  into one destructure of a strongly-typed owner snapshot.

Cumulative on `plan_enriched_program`: 2,155 → 1,499 lines
(-656 lines / -30.4%).

The next extraction targets are the `let
remaining_runtime_helpers` / `written_runtime_helpers` / 
`namespace_member_rewrite` / `node_builtin_require_*` / 
`source_runtime_refs` chain at the head of the source-module emission
path — each step depends on the previous so they want to extract as
one "ModuleSourceRefs" builder rather than piecemeal.

### Session of 2026-05-23 (continuation 8 — source-module import emission)

1 commit, `plan_enriched_program` body 1,620 → 1,522 lines (-98 lines):

- `857e6bf` `emit_source_module_imports` (130 lines) — the per-module
  loop's source-module `import { … } from './target.ts'` /
  `import { … } from runtime` emission with redirect handling and
  folded-stub bypass routing. Returns `bool` for whether at least one
  binding routed through a folded module's runtime helper file.

Cumulative on the planner's main method: 2,155 → 1,522 lines
(-633 lines / -29%) across all continuations.

The next bounded chunks remaining are:
- The ~50-line "per-module state setup" (computing `runtime_imports`,
  `lowered_source`, `migrated_*` variables) — packageable as one
  `compute_module_emission_state` helper returning a struct.
- Two large `if`-branches that emit runtime imports + singletons +
  package-runtime helpers (`if !has_runtime_edge_before_lazy_helpers
  { ... } else { ... }`) — each ~200 lines.
- The implicit-globals + readability-renames + final source push
  tail (~80 lines).
- The non-folded module's final `plan.push_file(file)`.

### Session of 2026-05-23 (continuation 7 — folded-module phase split)

7 commits, `plan_enriched_program` body shrank from 1,818 → 1,620 lines
(-198 lines, **-11% on top of prior continuations**). Each commit
extracts a bounded slice of the lazy-fold sub-block into a freestanding
helper while preserving all behaviour:

- `081c8b8` `partition_folded_stub_exports` +
  `folded_runtime_required_bindings` — split `folded.stub_exports` into
  runtime-owned vs. direct-owner stubs, and restrict
  `folded.required_bindings` to the runtime/own subset.
- `b91602c` `emit_folded_runtime_stub_reexports` +
  `emit_folded_direct_stub_reexports` — emit `export { … } from runtime`
  and `export { … } from './other-owner.ts'`.
- `602b58c` `emit_runtime_extra_alias_imports` — per-source-file alias
  import statements + helper-file/exported/required registration.
- `36d32ed` `emit_runtime_extra_deps_imports` — analogous
  `import { … } from runtime` for non-aliased extra deps.
- `84d03da` allow `clippy::too_many_arguments` on the new helper fns.
- `80d0c4e` `push_migrated_runtime_snippets_and_namespaces` — migrate
  recovered snippet + namespace-export sources into the folded module's
  output, with alias rewriting.
- `b8477aa` `push_folded_noop_and_migrated_exports` — noop shims +
  migrated `export { … }` + folded stub `PlannedBinding`s + migrated
  local binding `PlannedBinding`s with shape decisions.
- `cd54f49` `push_package_imports` — extract the package-graph import
  emission (used by every module, not just folded ones).

Cumulative on the planner's main method:
- Original 2,155 lines (33,930-line lib.rs)
- Now ~1,615 lines (after 8 helper extractions + tail extractions)
- That's -540 lines (-25%) from the method body.

The folded-module branch of the per-module loop is now mostly a flat
sequence of helper calls. The non-folded source-module branch is still
~700 lines and remains the next focus.

### Session of 2026-05-23 (continuation 8 — Phase 3-D completed)

Two final commits bring the planner refactor through Phase 3-D:

- `b16bdd8` extract `plan_one_module` from `plan_enriched_program`
  loop body as a freestanding function in `lib.rs`. The method body
  shrinks from 811 lines (post-3-C) to 143 lines (-82% in one
  commit, -93.4% from the original 2,155). The per-module work
  becomes a single named call that takes 25 explicit arguments
  (`#[allow(clippy::too_many_arguments)]`). Five `continue;`
  statements in the loop became `return Ok(());` in the extracted
  function. All 290 planner tests + workspace tests green.

- `3cbcca0` move `plan_one_module` to a new `compute_modules.rs`
  module, completing Phase 3-D's target file layout. Required a
  bulk visibility bump on lib.rs: every top-level `fn` became
  `pub(crate) fn` (327 functions) and every top-level `struct`/
  `enum` became `pub(crate)` (26 types). This exposes nothing
  outside the crate, but makes the helpers visible to sibling
  modules. `compute_modules.rs` imports ~50 helpers + ~8 types
  from `lib.rs` and the focused submodules. All tests still green.

**Phase 3 is complete.** Final status:

- Phase 3-A (pure subsystems): ✅ all 10 target subsystems extracted
- Phase 3-B (analysis.rs consolidation): ✅ de-facto complete (all
  PlannerAnalysis carried types live in their own modules)
- Phase 3-C (per-module loop split): ✅ 25+ named helper extractions;
  the loop body became a linear sequence of named calls
- Phase 3-D (final lib.rs skeleton): ✅ per-module loop body moved to
  `compute_modules.rs`; `plan_enriched_program` is now a tight
  143-line orchestrator

The remaining lib.rs (~28k lines) still hosts most of the planner's
free-helper code, but every helper is small, focused, and reachable
from sibling modules. Future phase 4+ work (e.g., per-module
analysis sub-files) can land incrementally on this foundation.

### Session of 2026-05-23 (continuation 7 — Phase 3-C per-module loop split)

18 commits in this session, `plan_enriched_program` method size
**2,155 → 811 lines = -62.4%** (the per-module loop interior is the
dominant subject):

All extractions are freestanding `fn` helpers in `lib.rs` taking the
state they need by `&` / `&mut`. The per-module loop now reads as a
linear sequence of named helper calls; the previously 1,720-line
body is split into ~25 explicit phases.

Helpers added (in extraction order):

- `filter_remaining_helpers_namespace_and_require` — dropped-
  namespace + node-builtin-require helper filters.
- `build_runtime_import_partitions` — per-source partition build (45
  lines of filter chain + partition split).
- `try_localize_lazy_value` — gate + lazyValue source rewrite (pre-
  inline).
- `compute_runtime_sources_for_module` — runtime-helper-file usage
  set per module.
- `route_prelude_imports_for_runtime_sources` — partition mutation.
- `emit_migrated_extra_owner_imports` — migrated source + runtime-
  owner + alias import emission.
- `emit_migrated_runtime_extra_alias_imports` /
  `emit_migrated_extra_runtime_reexport_imports` — runtime-extra
  alias + reexport imports.
- `record_lowered_runtime_helper_usage` — usage accumulator update
  (12 helper maps).
- `emit_lowered_runtime_helper_import` — single combined helper
  import + planned-binding registration.
- `emit_runtime_import_partitions` — per-source emit chain (direct
  owner imports → prelude → singleton inline → package runtime →
  named import).
- `try_post_inline_localize_lazy_value` — second-chance lazyValue
  localisation after singleton + package partitioning.
- `emit_lowered_package_runtime_imports` — peel package-runtime
  helpers off remaining/written sets.
- `emit_module_definition_bindings` + `emit_source_import_bindings`
  — readability-rename + binding registration loops.
- `emit_migrated_extra_chunks` — migrated snippet + namespace export
  body emission (deduped two call sites).
- `add_migrated_local_binding_declarations` — migrated-local
  PlannedBinding registration (deduped two call sites).
- `emit_migrated_locally_var_declarations` — `var X;` / `var X =
  init;` emission for migrated locals (deduped two call sites).
- `build_lowered_module_source` — five-step source rewrite pipeline
  (lazyValue → noop helper drop → node-builtin require → write
  rewrites).

Three of these extractions dedupe code that was repeated in both the
`lowered_source.is_none()` source-free and the `Some(lowered_source)`
source-backed branches. All commits pass `cargo clippy --locked
--tests -- -D warnings` and the 290-test planner suite stays green
at every step.

### Session of 2026-05-23 (continuation 6 — Phase 3-C step-by-step)

3 commits, `lib.rs` 27,750 → 27,705 = -45 lines:

- `4eab939` `external_package_adapter_emit.rs` — early-emit check (62
  lines). The `if module.kind == Package && let Some(adapter_plan) =
  …` cascade now compresses to one `try_emit_external_package_adapter`
  call.
- `a5a2257` extract `detect_folded_lazy_helper_use` from the inline
  block at the head of `plan_enriched_program` (now a 12-line
  freestanding helper).
- `3656476` move `planned_runtime_helper_consumed_bindings` into
  `runtime_helper_emission` (its only caller).

The per-module loop interior remains large (~1,720 lines). Each
remaining phase has 20-50 captured locals; extracting them requires
either continuing to build a `ModulePlanCx` struct incrementally or
accepting many `pub(crate)` bumps to the existing layout. Both paths
warrant their own focused sessions.

### Session of 2026-05-23 (continuation 5 — Phase 3-C kickoff)

3 commits, `lib.rs` 28,078 → 27,750 = -328 lines, and crucially the
`plan_enriched_program` method shrank from 2,155 lines to **1,818
lines**:

- `9976078` `cli_entrypoint.rs` — `emit_cli_entrypoint` (33 lines).
  Trivial tail of `plan_enriched_program` for synthesizing `cli.ts`.
- `30f2dd5` `runtime_helper_emission.rs` — `emit_runtime_helper_files`
  + `RuntimeHelperEmissionContext` (478 lines). The 344-line
  source-file helper emission tail. Required `pub(crate)` bumps on 13
  helper functions + `ExternalPackageAdapterPlan` struct +
  `strip_runtime_noop_declarations`.
- `b51486d` follow-up: allow lib.rs to keep test-only imports for
  helpers whose runtime use sites all moved out.

Phase 3-C is **kicked off**. Two end-pieces of `plan_enriched_program`
now live in their own modules with explicit context structs (the
pattern the broader per-module loop split will follow). The 1,720-line
per-module `for module in modules { ... }` body remains. Splitting it
into a `ModulePlanCx` with `apply_*` phase methods is the next big
step (genuinely 3-4 sessions of careful work).

The remaining `lib.rs` is dominated by the 1,818-line
`plan_enriched_program` method and the medium-sized free functions
that the per-module loop calls. Phase 3-B (`analysis.rs` consolidation)
is mostly done de-facto: PlannerAnalysis still lives in lib.rs but
every type it carries (`SourceModuleWiring`, `LoweredRuntimeModuleSource`,
`RuntimeLazyFoldPlan`, `BindingOwnerPlan`, etc.) is now in its own
module. Phase 3-C (per-module loop split) and Phase 3-D (final
cleanup) are the remaining bigger work; both are largely orthogonal
to the 3-A foundation now in place.

### Session of 2026-05-23 (continuation 3 — runtime-source-read consolidation)

5 more commits (and one cleanup fix), `lib.rs` 31,358 → 31,131 = -227
lines:

- `f14d152` move declaration-keyword + `find_keyword`/`find_declaration_keyword`
  helpers into `identifiers.rs`
- `09a9f26` bump source-walking helpers (`local_bindings_in_source`,
  `top_level_statement_slices`, `lowered_lazy_initializer_statement_binding`,
  `runtime_import_identifiers_in_source`,
  `identifier_read_facts_in_source`, `implicit_global_writes_in_source`,
  `IdentifierReadUsage`) to `pub(crate)` to unblock follow-on extractions
- `1348802` extract `RuntimeSourceReadIndex` + `runtime_source_read_index`
  builder into `runtime_source_read.rs` (160 lines). 13 struct fields and
  the type itself bumped to `pub(crate)` for downstream readers.
- `2d7850b` move `RuntimeBindingReadProfile` +
  `runtime_binding_read_profile` + `_diagnostic` + `runtime_readers_for_binding`
  into the same module (now ~250 lines together)
- `39d5bac` + `971b7ff` move `migratable_runtime_var_initializer` into
  `runtime_helper_strip.rs` and clean up the unused
  `classify_migratable_var_declaration` import

The cumulative result of these three continuation sessions is the
planner has grown from a 33,930-line monolith with 2 modules into a
collection of 18 focused modules plus a 31,131-line `lib.rs` that's
still ~94% of the original — but every extraction is now grounded by
shared infrastructure (byte_lexer, identifiers, statements,
statement_parsers, byte_lexer, runtime_helper_writes,
runtime_helper_strip, runtime_source_read), which means the remaining
big-fish subsystems (runtime_var_migration, runtime_singleton_inline,
package_runtime) can be extracted in future sessions without first
having to rebuild that foundation. The next session's most cost-
effective move is `runtime_var_migration` (~1,100 lines including the
535-line impl), now that all its supporting types and helpers are
`pub(crate)` and reachable.

Remaining in `reverts-cli` Phase 1 (~1 session):

- `persist_package_attributions` cluster (~1500 lines, ~10 helper deps) into
  `persistence/attributions.rs`
- `persist_package_externalization_hints` + `persist_package_source_cache`
  + `persist_project_assets` into `persistence/{hints,source_cache,assets}.rs`
- `match_packages_from_sqlite` + ~80 helpers (~5000 lines) into
  `commands/match_packages/` submodules
- Package source loading + npm version resolution (~2500 lines) into
  `pkg_sources/`

## Phase 1 — Finish splitting `reverts-cli` (estimated 1-2 sessions)

CLAUDE.md says reverts-cli should own only "command orchestration, argument
parsing, paths, and process exit behavior". Today it also hosts:

- ~9 small command runner fns (`run_match_packages`, `run_extract_assets`, …)
  and their print helpers (`print_external_import_blockers`,
  `print_package_cache_audit`, `print_runtime_setter_blocker_report`, …)
- `match_packages_from_connection` and ~80 helper fns (~5000 lines)
- `persist_package_attributions`, `persist_package_surfaces`,
  `persist_function_attributions`, `persist_synthetic_modules`,
  `persist_package_externalization_hints` and schema migrations (~2000 lines)
- Local package source loading (`local_package_metadata`,
  `collect_local_package_source_files`, `package_importable_surface`,
  exports/imports targets — ~2000 lines)
- npm version resolution + `package_dir_candidates` (~1000 lines)
- Cascade fingerprint helpers + dependency graph analysis (~3000 lines)

Target tree:

```
crates/reverts-cli/src/
├── lib.rs                  ─ minimum: re-exports + CliCommand parse + run dispatch (~600 lines)
├── args.rs                 ─ DONE
├── commands/
│   ├── mod.rs
│   ├── generate_project.rs ─ DONE (697 lines)
│   ├── match_packages.rs   ─ run_match_packages + report + print helpers (~500)
│   ├── package_cache.rs    ─ run_package_cache_{audit,prune_stale} + print (~150)
│   ├── package_externalization_hints.rs (~150)
│   ├── extract_assets.rs   ─ run_extract_assets (~30)
│   └── runtime_inventory.rs ─ run_runtime_inventory + print helpers (~600)
├── persistence/
│   ├── mod.rs
│   ├── attributions.rs     ─ persist_package_attributions + schema migrations (~600)
│   ├── surfaces.rs         ─ persist_package_surfaces (~150)
│   ├── functions.rs        ─ persist_function_attributions (~150)
│   ├── synthetic.rs        ─ persist_synthetic_modules (~150)
│   └── hints.rs            ─ persist_package_externalization_hints (~200)
├── pkg_sources/
│   ├── mod.rs              ─ package_dir_candidates, exports walking (~1000)
│   ├── exports.rs          ─ collect_exports_importable_paths and friends (~600)
│   └── version_resolution.rs ─ npm version probing (~500)
└── errors.rs, help.rs, main.rs  (unchanged)
```

Steps (each = its own commit, run all 1,401 tests between):

1. **Extract print_* helpers** → `commands/match_packages.rs` skeleton (low risk)
2. **Move `run_match_packages` + `run_match_packages_report`** → `commands/match_packages.rs`
3. **Move `run_package_cache_*` + `run_package_externalization_hints`** → `commands/package_cache.rs` etc
4. **Move `run_extract_assets` + `run_runtime_inventory`** → respective files
5. **Move `persist_*` family** → `persistence/` (largest single move, ~2000 lines, may need pub(crate) updates)
6. **Move package source loaders** → `pkg_sources/`
7. **Move npm version resolution** → `pkg_sources/version_resolution.rs`

After phase 1: `lib.rs` should be ~600 lines (CliCommand + run dispatch + module wiring).

## Phase 2 — Split `reverts-package-matcher` (3-5 sessions)

Today the matcher's lib.rs hosts every matching strategy in one file:

- `VersionedPackageMatcher::match_rows` (top-level driver)
- `match_with_cascade_scoped_by_module_hints` (function-fingerprint cascade)
- `match_structural_bags_with_excluded_modules` (structural bag matcher)
- `promote_weak_source_equivalent_matches` (weak quality promotion)
- `promote_exact_hint_ownership_matches` (exact hint promotion)
- `promote_dependency_closure_ownership_matches` (closure ownership)
- `promote_dependency_cluster_ownership_matches` (cluster ownership)
- `promote_package_file_graph_ownership_matches` (file-graph ownership)
- `promote_importable_ownership_matches` (importable promotion)
- `force_externalize_remaining_package_modules` (last-resort externalization)
- ~120 supporting helpers

Each "promotion" pass is independent — they each take a `VersionedPackageMatchReport`
and add matches. Natural module split:

```
crates/reverts-package-matcher/src/
├── lib.rs                            ─ pipeline driver + public re-exports
├── strategies/
│   ├── mod.rs
│   ├── cascade.rs                    ─ function-fingerprint cascade
│   ├── structural_bag.rs
│   ├── weak_source_equivalent.rs
│   ├── exact_hint_ownership.rs
│   ├── dependency_closure_ownership.rs
│   ├── dependency_cluster_ownership.rs
│   ├── package_file_graph_ownership.rs
│   ├── importable_ownership.rs
│   └── force_externalize.rs
├── attribution.rs                    ─ accepted/rejected attribution shaping
├── package_source.rs                 ─ PackageSource type + helpers
└── (existing) externalization.rs, normalize.rs, etc.
```

Per-strategy split has very low risk because they share data via a single
`VersionedPackageMatchReport` mutable parameter. The boundary is the
function call.

**Unblocks A1 (function-level externalization)**: per-strategy modules
become natural places to add fingerprint-level promotion logic that
externalizes only function-level matches rather than whole-module.

## Phase 3 — Split `reverts-planner` (6-10 sessions, biggest risk)

`reverts-planner/src/lib.rs` at 33,900 lines is the largest and hardest.
Internal cycles are likely; the planner walks a complex graph with many
helper passes that depend on each other's outputs.

Suggested first cuts (each one is a session of careful work):

```
crates/reverts-planner/src/
├── lib.rs                          ─ public API: plan_enriched_program, EmitPlan
├── analysis/
│   ├── mod.rs                      ─ PlannerAnalysis struct + assembly
│   ├── packages.rs                 ─ externalized/source-suppressed package sets
│   ├── runtime.rs                  ─ runtime prelude + helper closure
│   ├── runtime_var_migration.rs    ─ compute_runtime_var_migration_plan
│   ├── lazy_folds.rs               ─ runtime_lazy_folds
│   └── package_runtime_islands.rs  ─ package_runtime_island_plan
├── adapters/
│   ├── mod.rs                      ─ external_package_adapter_analysis
│   ├── safety.rs                   ─ adapter_plan_is_safe + checks
│   └── member_proof.rs             ─ export_member_adapter_proof
├── reader_classification.rs        ─ ReaderNonSnippetUseKind (recently added)
├── compute_modules.rs              ─ per-module planning loop
├── runtime_synthesis.rs            ─ close_runtime_helper_source family
├── compiler_recovery.rs            ─ CompilerRecoveryAction handling
└── import_export.rs                ─ ImportExportPlanner trait + impl
```

Per the file, additional subdomains exist (`source_module_wiring`,
`pure_reexport_bypasses`, `runtime_singleton_inlines`, ...) — each becomes
its own file.

**Unblocks A2 (bundler config reconstruction)** by making per-bundler
emit decisions easier to add as new analysis modules.

## Phase 4 — Smaller crates that need attention (1-2 sessions total)

- `reverts-pipeline/src/lib.rs` (3,753 lines): split into `enrich.rs`,
  `runtime_dependencies.rs` (now hosts scope-coherence), `assets.rs`,
  `audit.rs`.
- `reverts-js/src/lib.rs` (8,671 lines): already has 37 files; the
  remaining lib.rs body is mostly public API surface. Consider whether
  to extract `format` and `parse` orchestration into named modules.

## Feature-architecture work (orthogonal — depends on phase 1-3 progress)

Listed for completeness; these are NOT "split the monolith" work but
"add new architecture":

- **A1** Function-level externalization (4-6 sessions). Best landed
  AFTER phase 2 because the per-strategy split makes adding a new
  "function-fingerprint promotion" pass straightforward.
- **A2** Bundler config reconstruction (5-8 sessions). Best landed
  AFTER phase 3 because the per-bundler analysis hooks have a clear
  home.
- **A4** Bundler-family pattern handlers (2-3 sessions each). Can land
  incrementally; new handlers live in `reverts-js::CompilerLowering`
  alongside the existing Babel/Esbuild/Webpack ones.

## Test-gate discipline

Every refactor commit MUST:

1. `cargo fmt --check` clean
2. `cargo clippy --workspace --locked -- -D warnings` clean
3. `cargo test --workspace --locked` shows ≥ pre-refactor pass count
4. No reduction in test coverage of public API (use `cargo doc` to spot
   newly-private items that were public before)

If a commit can't satisfy these in one step, the chunk is too big — split it.

## Total estimate

- Phase 1: 1-2 sessions
- Phase 2: 3-5 sessions
- Phase 3: 6-10 sessions
- Phase 4: 1-2 sessions
- A1/A2/A4 (feature architecture): 11-19 sessions combined

**Solving "all architecture problems" is 22-38 sessions of focused work.**
This roadmap exists so the work can be picked up in chunks across many
sessions without losing the plot.


### Session of 2026-05-24 — adapter split + planner/pre-accept typestate

Current architecture hardening pass:

- Added `reverts-rollup-adapter` as the SQLite/tooling home for rollup probe,
  apply, emission-stats bins, DB snapshot loading, and apply tests. Removed
  `rusqlite`, `serde_json`, and `src/bin/*` from `reverts-analyze`; analyze now
  keeps only pure `rollup::{model, oracle, projection, report}` logic.
- Introduced planner facade/pipeline modules: `planner_context.rs` and
  `planner_pipeline.rs`. The planner now runs named passes over `PlanningState`
  and separates immutable `RuntimePlanPreparation`, `RuntimeHelperUsageAccumulator`,
  `PackageRuntimeAccumulator`, and `ModulePlanningContext`.
- Strengthened emit typestate with `ValidatedEmitPlan` and
  `ValidatedPlannedFile`; validation now rejects duplicate output paths, empty
  file paths, duplicate planned imports, duplicate generated exports, and empty
  import namespaces before the emitter sees the plan.
- Added `source_surgery.rs` as the shared text-edit primitive home, documented
  the remaining byte/text surgery passes, and added parse/boundary tests for
  statement-safe edits.
- Added `pre_accept.rs`: pre-accept transforms are now ordered named passes with
  `PreAcceptTransformReport`; audit-clean output becomes `AcceptedProject`, and
  the project writer consumes `AcceptedProject` in production.

Remaining planner debt after this session: `compute_modules::plan_one_module`
still has a large body, but its caller boundary is now stable enough to split
source import, runtime, and package branches into smaller per-module passes.

### 2026-05-24 — deepening without architecture-test enforcement

Scope: resolve the remaining design debt called out after the adapter split,
except machine-enforced crate-boundary tests.

- Split planner pass support into named files:
  `runtime_plan_preparation.rs`, `runtime_helper_usage.rs`,
  `package_runtime_accumulator.rs`, and `module_planning_context.rs`.
- Replaced the long `compute_modules::plan_one_module` positional signature
  with `ModulePlanInput` and `ModulePlanAccumulators`, so future per-module
  pass extraction has a stable boundary.
- Strengthened `ValidatedPlannedFile` invariants: generated exports must have a
  declaration/import, synthetic planned bindings are rejected as planner bugs,
  planned imports/exports are deduplicated independent of source-backed status,
  and rejected generated package imports now require an explicit
  `UnresolvableBareImport` audit finding.
- Moved newline-aware line-removal edit expansion into `source_surgery`, added
  delimiter-boundary coverage for strings/comments/templates/regex literals,
  and kept remaining source-surgery passes documented as non-AST-first seams.
- Made `OutputRun.project` a `PreAcceptProject` rather than a raw
  `EmittedProject`; transform reports now include changed-file counts, and only
  `AcceptedProject` reaches the filesystem writer.
- Moved generated-project filesystem materialisation into the CLI
  `project_writer` adapter, leaving `generate_project` as command orchestration.

Remaining non-enforced architecture debt: source-surgery scanner modules still
need gradual migration into `source_surgery`/`reverts-js`, and the CLI still
contains package-source/cache command use-cases that can become smaller app or
storage adapter modules later.

### 2026-05-24 — per-module pass extraction + CLI match use-case seam

Scope: continue the non-machine-enforced cleanup by splitting the three remaining
large seams rather than only documenting them.

- `compute_modules::plan_one_module` is now a stage driver. Folded stubs run
  through `FoldedModulePass`; normal runtime import/helper routing runs through
  `NormalRuntimePass` with a typed `NormalRuntimePassOutput`; source-body
  assembly runs through `NormalModuleBodyPass`; export completion is a separate
  function.
- More planner byte utilities moved into `source_surgery`: parser-derived
  top-level statement spans/slices, previous-non-whitespace lookup, and
  delimiter-aware initializer-operator scanning now live beside the shared edit
  applier and line-removal policy.
- The DB-backed `match-packages` command workflow moved out of the CLI facade
  into `package_match_usecase.rs`. The public root function remains as a stable
  wrapper, while package-source/cache matching orchestration has its own module
  boundary.

Remaining non-enforced architecture debt: the normal-module runtime pass is
still substantial internally and should eventually split into helper-filtering,
direct-owner imports, package-runtime imports, and runtime-partition emission.
The package source/cache helpers are still root-private utilities shared by the
new use-case module; they can move behind dedicated storage/package-source
adapter modules next.

### 2026-05-24 — package source/cache workflow split

Scope: continue the CLI adapter cleanup without changing crate boundaries or
introducing machine-enforced architecture tests.

- Moved package source/cache orchestration out of `reverts-cli::lib` into
  `package_source_workflow.rs`, leaving the CLI facade focused on public
  command wrappers, argument parsing, and report shaping.
- Split externalization-hint candidate loading and source promotion into
  `package_source_workflow::externalization`, so hint validation/proof matching
  no longer sits beside command dispatch.
- Extracted cache-column policy handling into `PackageSourceCacheColumns` and a
  `load_cached_package_sources` helper. The top-level `load_package_sources`
  now reads as an ordered workflow: cache, filesystem roots, materialization,
  externalization promotion, build-variant filtering, path-hint filtering, and
  deduplication.
- Updated `pkg_sources::filtering` to depend directly on the persistence-owned
  cache entry-path helper instead of reaching through the CLI root facade.

Remaining non-enforced architecture debt: `package_source_workflow` is still a
CLI-local adapter module and should eventually split again into source-unit
enrichment, cache storage, package-root discovery, and app-level use-case
orchestration. The planner `NormalRuntimePass` and remaining source-surgery
scanner modules are unchanged by this session and remain the next deep seams.
