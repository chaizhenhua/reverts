# Claude Code decompiles Claude Code (cc-2.1.89 demo)

The demo: point an agent at Claude Code's own shipped npm bundle and, from **one
sentence**, recover a runnable, readable TypeScript project — then prove it runs Claude
Code. Self-referential dogfooding: the agent reverse-engineers its own distribution.

Target: `cc-2.1.89/package/cli.js` — a single **13 MB minified one-line** bundle.

## The one sentence

To a Claude Code / Codex agent that has the Reverts skills installed:

```text
Use the reverts-decompile skill to decompile this Claude Code npm package into a
runnable TypeScript project and prove it runs:

/path/to/cc-2.1.89/package
```

That is the whole user input. Everything below is what the agent does under it.

## Does it need skill support? Yes — and here is the split

A bare LLM cannot decompile a 13 MB minified bundle by reading it: too large to hold, and
the transforms (module splitting, scope-correct renaming, package matching) must be
deterministic and verifiable. The skills supply that; the agent supplies judgment. Neither
alone is enough.

| Layer | Who | Responsibility |
|-------|-----|----------------|
| `node-bundle-collector` (skill, **added for this demo**) | tool | npm package / `cli.js` → `reverts-import-evidence.json` manifest |
| `reverts-decompile` (skill) | tool | orchestrates import → split → classify → match → generate → naming loop → validate |
| `reverts-cli` (binary) | tool | deterministic engine: `import-unpacked`, `generate-project-v2`, `module-classify`, `match-packages`, `symbol-names`, `naming-plan/progress` |
| the agent | judgment | read recovered code → semantic names; identify bundled third-party; decide what is dead; validate the run |

The "one sentence" works **because** the `reverts-decompile` skill encodes the procedure
the agent follows. The agent's irreplaceable part is semantic: naming `vF`→`Stream`,
recognizing module 158 is `d3-color`, judging that `var A,i,n,o,s` are dead esbuild hoists.

## Problem found and fixed (end-to-end gap)

The existing collectors handle Electron DMG/ASAR, bun-fs, and browser extensions — but a
plain npm package (`target_kind: node_bundle`) had **no collector**, so the import-evidence
manifest had to be hand-written. That broke "one sentence from scratch."

Fix: `node-bundle-collector` (`~/.codex/skills/node-bundle-collector/scripts/collect_node_bundle.py`).
Given a `.js` bundle or an npm package dir, it resolves the bundle (via `package.json`
`bin`/`main`), copies it into `unpacked/`, and emits the `reverts.import_evidence.v1`
manifest (size + sha256). Validated: collector → `import-unpacked` ingests with no manual
step.

## Verified end-to-end (release binary, from the raw bundle)

Every stage was actually run on `cc-2.1.89/package/cli.js`:

```text
1. collect   node-bundle-collector  → reverts-import-evidence.json   (bundle + sha256)
2. import    import-unpacked        → project.sqlite (53 KB; 1 bundle source)
3. generate  generate-project-v2    → 2883 TypeScript modules
4. name      agent loop (skill)     → first-party named; bundled 3p classified out;
                                       dead esbuild hoists def-use–excluded
                                       (naming worklist 26800 → 3845, then bounded)
5. compile   tsc -p tsconfig.runtime.json  → exit 0, 0 errors
6. run       node dist/cli.js ...   → see below
```

Run proof (the recovered code, not the original):

```text
--version       → 2.1.89 (Claude Code)
--help          → full usage (every option)
-p "say hi"     → Hi! How can I help you today?    (real model API call + streaming)
--bare (TTY)    → renders the interactive trust prompt + menu (1/2, Enter/Esc)
```

`-p` is the strongest proof: it exercises the entire recovered pipeline — module load,
init, arg parse, agent loop, API client, SSE streaming, render — end to end.

## How to pitch it

> Give an agent one sentence and Claude Code's own 13 MB minified `cli.js`. It unpacks
> nothing by hand: it ingests the bundle, splits it into **2883 readable TypeScript
> modules**, recognizes and sets aside the bundled third-party libraries (d3, monaco,
> lru-cache, the SDK), names the first-party code from understanding (`vF`→`Stream`,
> `findGitRoot`, …), drops the dead bundler scaffolding via data-flow analysis, and ends
> with a project that **`tsc`-compiles clean and runs Claude Code** — `--version`,
> `--help`, a real `-p` conversation, and the interactive UI.

Live proof points, in order:

1. The one sentence + the 13 MB single-line `cli.js` (the "before").
2. A recovered `modules/*.ts` next to it (the "after" — real imports, structure, names).
3. `tsc` exit 0 on the recovered project.
4. `node dist/cli.js -p "say hi"` returning a real answer — the decompiled Claude Code
   *being* Claude Code.

## Honest boundaries

- First-party naming is not 100%: bundled third-party and dead esbuild scaffolding are
  excluded (correctly), leaving a bounded first-party worklist the agent names across
  rounds. The agent does not fabricate names to game the number.
- Validated on the core run paths (version/help/print/TUI render), not every subcommand.
- `vendor/` native assets (e.g. ripgrep `rg`) are not bundled by the minimal collector run
  (`--include-vendor` adds them); the core CLI paths above do not need them.
