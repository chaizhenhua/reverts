# External Bundler Corpus

This directory holds curated outputs from real bundler / transpiler test
suites, used to verify that ReverTS Next handles real-world inputs without
assuming any specific synthetic shape.

## Contents

`cases/<bundler>/<category>/<case-id>/` — one fixture per case.

Each case follows a uniform layout:

| Path | Contents |
| --- | --- |
| `case.json` | Case manifest (id, category, expected `bundler_family`, expected wrappers/helpers) |
| `README.md` | Human-readable description of what the case covers |
| `input/` | Original ESM source (ground truth, before bundling) |
| `artifacts/<tool>@<version>/` | Real bundler output — the input that ReverTS must lower |
| `oracle/graph.json` | Expected analysis graph (modules, exports) |
| `oracle/structure.json` | Expected wrapper / helper structure |
| `oracle/equivalence.json` | Decompiled-vs-original module pair declarations |

`<bundler>/conversion-report.json` records the case roster generated for that
bundler.

## Coverage

| Bundler | Cases |
| --- | --- |
| esbuild | 894 |
| webpack | 29 |
| rollup | 27 |
| bun | 23 |
| babel | 21 |
| vite | 18 |
| parcel | 12 |
| rolldown | 10 |
| swc | 8 |
| rspack | 7 |
| tsc | 6 |

Total: 1055 cases.

## Attribution

Cases are curated from upstream bundler / transpiler test suites. Each
case's `case.json::upstream` and `artifacts/*/build-meta.json` record:

- the upstream project name and version,
- the upstream commit,
- the upstream test reference path.

Curation was originally produced for the prior ReverTS implementation
and is reused here verbatim under this project's Apache License 2.0. The
underlying bundler test sources retain their respective upstream licenses.

## Loader

The `reverts-fixtures::external_corpus` module owns the serde contract for
`case.json` and the file-system traversal that yields one `ExternalCase`
per directory. Tests must depend on that loader rather than walking this
directory directly, so the on-disk layout can evolve without rewriting
every test.

## Stability

These fixtures are reference inputs, not regression baselines. Adding,
removing, or renaming cases is allowed when the upstream project does the
same. Tests must tolerate cases that the current pipeline does not yet
support — see the corpus integration test for the report-only pattern.
