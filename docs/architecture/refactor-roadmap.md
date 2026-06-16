# Architecture refactor roadmap

Captures the architectural debts in reverts-next and a phased plan for
addressing them. Each phase lists concrete extractions, expected line moves,
test-gate impact, and what it unblocks. Phases are independent — each can
land on its own and unblocks downstream work, but they're ordered by
combined "leverage × tractability".

## Current monolith sizes (2026-05-23)

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
