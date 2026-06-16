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

- `reverts-cli/src/args.rs` extracted (commit `a1ace2b`). 384 lines moved
  out; lib.rs dropped 14,173 → 13,810 (-363 lines). All 1,401 tests green.

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
