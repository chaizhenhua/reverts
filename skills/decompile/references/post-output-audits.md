# Post-output Structural Audits

Run these checks after `reverts-cli generate --input <db> --project-id <id>
--output <dir> --source-root src` and before handing the output to
`reverts-decompile`. Each finding is a ReverTS pipeline defect to fix with a
regression test; do not hand-edit generated `.ts` output to hide it.

Use AST or structured parsing (for example `oxc`) to enumerate imports,
exports, and declarations. Avoid regex over expression bodies; these audits are
about structured top-level data and project metadata.

## 5.1a Decl-vs-import name collision

A generated file must not both locally declare and import the same binding name.
When this happens, `tsc` reports errors such as `TS2451 Cannot redeclare
block-scoped variable 'X'`, and runtime behavior depends on whichever binding
wins under module evaluation.

- Symptom: intersect each file's top-level imported binding names with its
  top-level declared names (`const`, `let`, `var`, `function`, `class`, exported
  declarations, and equivalent AST nodes). Any non-empty intersection is a
  defect.
- Root cause: import synthesis matched by short name without consulting the
  consumer file's local declaration table.
- Action: record `{file, name, local_decl_kind, imported_from}`, add a pipeline
  regression test, fix the synthesis guard, regenerate, and re-audit.

## 5.1b Source-partition evidence isolation

Trigger this check whenever app-artifact metadata has more than one ingest-
enabled JavaScript/TypeScript source unit. A source partition is the collector
source-unit boundary, not a runtime kind. Cross-partition generated imports are
valid only when original dependency evidence links the source units.

Browser-extension role groups:

| Runtime context | Source-unit roles |
|---|---|
| MV3 service worker | `service_worker` |
| MV2 background | `background_script`, `background_page` |
| Offscreen document | `offscreen_html` and scripts referenced by it |
| Content scripts | `content_script` per manifest entry |
| Popup | `popup_html` and referenced scripts |
| Options page | `options_html` and referenced scripts |
| DevTools | `devtools_html` and referenced scripts |
| Sidebar | `sidebar_html` and referenced scripts |
| Web page | `accessible_chunk` |

Electron role groups such as `main_process`, `renderer_process`,
`preload_script`, and `worker_thread` are useful validation targets, but they
are not sufficient import-synthesis evidence. A renderer chunk may legitimately
reference another chunk only when the original artifact carried an import,
require, dynamic import, script, or equivalent edge.

Audit steps:

1. Pull source-unit, file, and coverage counts from `reverts-cli full-inventory
   --input <db> --project-id <id> --json <file>`.
2. Compute the source-partition label of each generated `.ts` file from
   source-unit/file provenance.
3. Enumerate each file's top-level relative or alias imports that resolve to
   another generated `.ts` file.
4. Record every cross-partition import lacking source-unit dependency evidence
   as `{from_file, from_partition, to_file, to_partition, import_names[]}`.

Why this matters: same-name minified globals in different source units often
come from independent minifier passes, not from a shared module graph. Cross-
partition imports can compile yet fail only when Chrome loads the extension,
Electron starts a renderer/preload boundary, or Node evaluates a chunk whose
top-level initialization order was never part of the importer graph.

## 5.1c Package misclassification scan

Scan for application-owned symbols that were routed through a package namespace
such as `__reverts_pkg_*`. These become runtime failures like `TypeError:
Cannot read properties of undefined (reading 'Y')` because the package never
exported the application symbol.

Use the MCP/DB-backed query:

```text
query(project_id, entity="symbols",
      filter="appears_as_pkg_property=true",
      page_size=50)
```

Each row is derived from structured AST/project indexes and should include the
offending symbol, namespace, and source location.

Misclassification signals:

| Signal | Example | Why it is wrong |
|---|---|---|
| Property has an app-module path prefix | `__reverts_pkg_axios.terminal_ascii_table_AT` | Application module path, not an axios export |
| Property has init/config prefix | `__reverts_pkg_axios.init_oauth_config_D9` | Internal init/config symbol |
| Property is a minified short name | `__reverts_pkg_axios.bqq` | Published package APIs do not expose bundle-local minified names |
| Same property exists in an app module | DB shows `AT` owned by `terminal/ascii-table` | Symbol belongs to app code |

Fix workflow:

1. For each leaked property `Y`, run `query(project_id, entity="symbols",
   search=Y)` and identify the actual owner module.
2. If the owner module is application code misclassified as a package, correct
   it with `reverts-cli module-classify --input <db> --project-id <id> --batch
   <TSV> --apply` (the TSV row carries durable classification evidence).
3. If the module classification is already correct, file the finding against the
   import-binding/cross-reference synthesis mechanism.
4. Regenerate with `reverts-cli generate --input <db> --project-id <id> --output
   <dir> --source-root src` (it overwrites the output directory in place).
5. Re-run the scan and require the finding count to reach zero or be explicitly
   documented as non-actionable with evidence.
