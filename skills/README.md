---
title: ReverTS Skills
---

# ReverTS Skills

This directory is the canonical source of all ReverTS Claude/Codex skills.
Each subdirectory ships as one skill bundle. Skills are **installed** into the
host's skill loader (`~/.claude/skills/`, `~/.codex/skills/`, or a plugin cache);
they are not loaded directly from this repository at runtime.

## Layout

```
skills/
├── decompile/                # webpack/esbuild bundle decompilation pipeline
├── browser-extension-collector/ # browser-extension artifact collection + manifest ingestion
├── electron-collector/       # Electron artifact collection + decompile handoff
├── reverts-decompile/        # post-export install / tsc / startup validation
└── install                   # local-dev installer script (symlinks into ~/.claude/skills)
```

Each skill directory contains:

- `SKILL.md` — required, with YAML frontmatter (`name`, `description`).
- `references/` — supporting documents loaded on demand by the skill body.
- `bin/` — optional collector/utility scripts the skill invokes.
- `agents/` — optional sub-agent prompt templates.

## Install

### End users (npm release)

The `reverts` npm package bundles this directory at publish time
(`npm/prepare-package.js` copies `skills/` → `npm/skills/`, and
`npm/package.json` lists `skills/` in its `files` field). Installing the
package therefore installs the skills:

```bash
npm install -g reverts
codex mcp add reverts -- npx -y reverts
claude mcp add reverts -- npx -y reverts
```

The MCP server announces the bundled skills under the `reverts:` namespace:
`reverts:decompile`, `reverts:browser-extension-collector`,
`reverts:electron-collector`, and `reverts:reverts-decompile`.

### Local development (this worktree)

Run the bundled installer once. It creates symlinks from
`~/.claude/skills/<name>` to each subdirectory under `skills/`, so edits in
the worktree show up in Claude Code immediately:

```bash
./skills/install                      # default: ~/.claude/skills
./skills/install --target ~/.codex/skills
./skills/install --uninstall          # remove all symlinks
```

Restart your Claude/Codex session (or use `/mcp` to reconnect) after the
first install so the skill registry picks them up. Subsequent edits to
`SKILL.md` or `references/*.md` are picked up live (next skill invocation).

When working on the MCP server itself, also rebuild the binary and re-point
the MCP registration:

```bash
cargo build --release --bin reverts-mcp
claude mcp add reverts -- "$(pwd)/target/release/reverts-mcp"
```

## Authoring conventions

These conventions apply to every skill in this directory.

### Frontmatter

```yaml
---
name: <slug-matching-directory-name>
description: <one-line, action-oriented; used by the skill loader for routing>
---
```

`name` MUST match the directory name. `description` should encode the trigger
condition (when to invoke) — the loader presents this to the model when
deciding whether to call the skill.

### Required sections

Every `SKILL.md` should provide, in this order:

1. **Brief intro** — one paragraph stating what the skill does and when to use it.
2. **Agent boundary** — explicit "do not do X / only do Y" statements.
3. **Phases / workflow** — numbered or named stages with entry/exit conditions.
4. **Decision table** — when there are multiple input shapes or branches, list
   them in a table mapping condition → action.
5. **Completion criteria** — quantitative gate (counters, ratios, exit codes).
6. **Failure recovery** — what to do when a hard blocker is hit.
7. **Tool summary** — the MCP tools / CLI commands the skill calls.
8. **References** — links to `references/*.md` for templates and deep dives.

### Style rules

- Prefer **quantitative gates** (`public_surface == 100%`, `tsc exit code 0`)
  over vague success criteria ("verify output looks right").
- Prefer **observable signals** for reverse anti-patterns (mention specific DB
  fields, log lines, file states), not psychological "red flags".
- Do not adopt coercive language ("you MUST", "1% chance"). State the cost of
  skipping a step instead.
- Define hard blockers explicitly: missing input, permission denied, MCP server
  unreachable, same operation failing N×, schema version mismatch.
- Keep `SKILL.md` under ~350 lines. Push templates, profile-specific checklists,
  and worked examples into `references/`.
- Keep `agents/openai.yaml` present for every committed skill, with UI metadata
  synchronized to the current `SKILL.md`.

### Cross-referencing

Within a SKILL body, link references with a relative path. For example,
`skills/decompile/SKILL.md` links to its sub-agent template like this:

```markdown
See [sub-agent-templates.md](references/sub-agent-templates.md).
```

(The same file is reachable from this README at
[decompile/references/sub-agent-templates.md](decompile/references/sub-agent-templates.md).)

Do not assume a particular install path — references are resolved relative to
the skill directory after install.

## Verifying changes

Before committing skill changes:

```bash
# 1. YAML frontmatter parses (every SKILL.md must start with --- ... ---)
for f in skills/*/SKILL.md; do
  python3 -c "import sys, yaml; yaml.safe_load(open('$f').read().split('---')[1])" || echo "FAIL $f"
done

# 2. Internal references resolve (strip anchors; ignore fenced code blocks)
python3 - <<'PY'
import re, pathlib, sys
broken = 0
for p in pathlib.Path('skills').rglob('*.md'):
    body = re.sub(r'```.*?```', '', p.read_text(), flags=re.DOTALL)
    for ref in re.findall(r'\]\(([^)]+)\)', body):
        if ref.startswith(('http', '#', 'mailto:')): continue
        path = ref.split('#', 1)[0]                # strip #anchor
        if not path: continue
        target = (p.parent / path).resolve()
        if not target.exists():
            print(f'MISSING {p}: {ref}'); broken += 1
sys.exit(0 if broken == 0 else 1)
PY

# 3. npm packaging picks them up
node npm/prepare-package.js && ls npm/skills/

# 4. Server still builds
cargo build --release --bin reverts-mcp
```

## File layout invariants

- `skills/` is the single source of truth. Do not put committed skill content
  under `.claude/skills/` — that path is reserved for installer artifacts.
- Each subdirectory's name MUST equal its frontmatter `name`.
- `references/` is the only allowed home for supporting `.md` files.
- `agents/openai.yaml` should contain only product-facing metadata and optional
  tool dependencies; do not duplicate workflow instructions there.
