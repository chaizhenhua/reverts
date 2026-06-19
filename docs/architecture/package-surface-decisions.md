# Package Surface Decisions

Package surface decisions are the Agent-facing policy gate for source-backed
bare package imports that Reverts cannot safely settle with deterministic
matching alone.

## Responsibilities

- **Skills** unpack targets, import facts, run deterministic Reverts commands,
  and hand TSV worklists to Agents. Skills do not write SQLite directly.
- **Agent/xagent workers** inspect emitted/source context and propose
  `accept_surface`, `reject_surface`, or `block_surface` TSV rows with evidence.
  They may derive package/version candidates from code, but their output is a
  proposal.
- **Reverts CLI** owns validation and persistence. `package-surface-decisions`
  validates TSV rows, records an append-only decision ledger, and writes accepted
  `package_surfaces` only through the same SQLite gate used by deterministic
  matching.
- **Matcher/generator pipeline** consumes the latest decision before persisting
  generated surfaces. A rejected or blocked latest decision suppresses future
  automatic surface acceptance for that specifier.

## Tables and meanings

| Table | Meaning | Consumer |
| --- | --- | --- |
| `package_attributions` | Module-level package ownership and external-import decisions. | `reverts-input`, analyzer, planner, emitter |
| `package_surfaces` | Accepted project-level import surfaces that may satisfy bare imports such as `ws` or `rxjs/operators`. | `PackageSurfaceIndex` during analysis |
| `package_surface_decisions` | Append-only Agent/Reverts ledger for source-backed package surface decisions. | `package-surface-decisions`, `match-packages` |

`package_surface_decisions` is not a post-write repair layer. It is consumed
before `match-packages` persists new accepted surfaces, so rejected or blocked
specifiers do not get reintroduced by a later deterministic pass.

## TSV contract

```tsv
accept_surface<TAB>package<TAB>exact_version<TAB>specifier<TAB>evidence
reject_surface<TAB>package<TAB>version|-<TAB>specifier<TAB>evidence
block_surface<TAB>package<TAB>version|-<TAB>specifier<TAB>evidence
```

Validation is fail-closed:

- package names must be syntactically valid;
- `specifier` must be a bare package specifier whose package segment matches
  the `package` column;
- `specifier` must exist in a source import/require/export/import-expression
  site for the project;
- `accept_surface` requires an exact semver version;
- evidence is required;
- multiple `accept_surface` rows for the same specifier in one batch are
  rejected;
- replacing an existing accepted `package_surfaces` row with a different
  package/version requires `--replace-existing --apply`, making Agent conflict
  resolution explicit.

## Conflict and precedence policy

The decision ledger is append-only. Consumption uses **latest decision wins** per
`(project_id, export_specifier)`, ordered by the SQLite `id`.

- Latest `accept_surface`: accepted by the CLI gate and written to
  `package_surfaces`.
- Latest `reject_surface`: future `match-packages` runs suppress matching output
  for that specifier and emit `PackageSurfaceDecisionBlocked` audit evidence.
- Latest `block_surface`: same suppression behavior as reject, reserved for hard
  blockers such as package/runtime incompatibility or unsafe API shape.

This policy lets an Agent correct an earlier rejection by applying a later
`accept_surface`, while preserving the full ledger for audit.

## Worklist and candidate generation

`reverts-cli package-surface-decisions --list` prints source-backed import sites
with:

- package name;
- concrete specifier;
- source files;
- current accepted surface status/version;
- candidate versions gathered from package modules, accepted attributions, and
  `package_source_cache`;
- the latest Agent decision.

Candidate versions are hints for the Agent/xagent. Reverts still validates the
final TSV and is the only component that mutates package-surface tables.

## Relationship to safety filtering

`match-packages` can generate cache-anchored public surfaces from accepted
attributions. The CLI safety filter may later remove unsafe attributions. After
that filter, cache-anchored surfaces whose supporting accepted attribution is no
longer present are removed before persistence. Source-backed surfaces remain
valid independently, unless the latest Agent decision rejects or blocks them.
