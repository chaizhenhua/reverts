# Naming Universe

This document defines what counts as a *nameable symbol* for the semantic-naming
workflow, so the naming gate (`name progress`) and the agent work list
(`name plan`) measure the same set and stay coherent.

## The universe is emitted- and graph-driven, not the `symbols` table

The actionable naming universe is **not** the SQLite `symbols` rows, and it is
**not** the input def-use graph (`RevertsGraph::definitions_for`). It is parsed
from the **emitted / reconstructed TypeScript**:

- The agent reads the *output*; the def-use graph is built from the *input*
  (minified bundle). Bundlers wrap top-level bindings inside a function in the
  input (e.g. an esbuild module's `var U$, sG` live inside a wrapper), so
  `definitions_for` correctly omits them — but emission un-wraps them to module
  top level. Input-graph definitions therefore do **not** equal emitted
  top-level bindings, which is why `definitions_for` is not the universe.
- `reverts-pipeline::build_symbol_index` parses each emitted file with
  `reverts_js::collect_top_level_statement_facts` (Function / Class / Variable /
  LazyValue / LazyModule / Export) into `OutputRun.symbol_index`
  (`SymbolIndexEntry { module_id, original_name, emitted_name, file_path,
  function_like }`). These are the bindings the agent can actually open and
  rename.
- `naming_progress::emitted_universe(program, excluded)` adds the
  **first-party** module set (externalized, vendored, and classified-out modules
  excluded) and per-module export *names* from the graph's `import_export()`
  view — export names *do* survive reconstruction.
- `classify_emitted_entry` is the single source of truth shared by both
  `name progress` and `name plan`: it drops entries whose module is not
  first-party, then tiers each entry (exported × function-like/value-like) and
  marks whether it is already named.

`import` populates modules and sources but not `symbols`; the `symbols`
table is only the agent's `semantic_name` overlay used to key renamed bindings
back to the database.

## Gate semantics

- `name progress` measures named/total **over the emitted universe**, so the
  denominator only includes bindings that were actually emitted into first-party
  files.
- `name plan` lists the unnamed subset of that same universe, each entry
  carrying the emitted file and location so the agent can open and rename it.

Because both commands flow through `emitted_universe` + `classify_emitted_entry`,
the plan and the progress percentage cannot disagree about which symbols count.

## Known limitation: top-level destructuring is dropped

The universe is only as complete as the emitted-source parser
(`collect_top_level_statement_facts` → `build_symbol_index`). Multi-declarator
simple declarations are handled correctly — `var a, b, c;`, `let x = 1, y = 2;`,
and `const p = 1, q = 2, r = 3;` each expand to one `symbol_index` entry per name
(verified 2026-05-27).

The real gap is **top-level destructuring patterns**:
`declaration_binding_names` (`reverts-js/src/facts.rs`) keeps only
`BindingIdentifier` declarators and drops everything else via its `_ => None`
arm. So:

- `const { m, n } = obj;` → **0** names
- `const [ s, t ] = arr;` → **0** names
- `var u, { v, w } = obj, z;` → only `["u", "z"]` (the destructured `v`, `w` are
  lost)

Any name bound only through an emitted top-level object/array (or nested)
pattern will not appear as a nameable target and will not count in the
`name progress` denominator, which silently inflates the coverage percentage.
This is latent today because emitted top-level bindings are usually plain
identifiers; it surfaces for targets whose reconstructed top level destructures.

If this needs to be closed, fix it in the emitted-source fact collection
(`reverts-js`) by walking object/array pattern bindings — not by counting the
`symbols` table and not by falling back to the input-graph `definitions_for`.

## References

- `crates/reverts-cli/src/commands/naming_progress.rs` —
  `emitted_universe`, `classify_emitted_entry`, `compute_naming_progress`.
- `crates/reverts-cli/src/commands/naming_plan.rs` — agent work list.
- `crates/reverts-pipeline` — `build_symbol_index`.
- `reverts-js::collect_top_level_statement_facts` — emitted-source binding facts.
- [input-data-model.md](input-data-model.md) — why `symbols` is not the source
  of truth for nameable bindings.
