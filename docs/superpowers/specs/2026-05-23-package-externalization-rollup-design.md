# Package Externalization Rollup — Design

Date: 2026-05-23
Status: Draft, awaiting review
Goal: raise external_import acceptance for `module_category='package'` from **38% → 90%** on the dataset in `~/.reverts/.reverts.db`.

## 1. Problem

The current matcher proves "this module belongs to package P" for 11,189 modules but only 4,243 (38%) are accepted as `external_import`. The remaining 6,946 are rejected — 6,535 (94% of rejections) with the same reason:

> "matched package ownership, but the evidence does not prove a safe single external import"

Drilling into evidence_json shows two failure populations:

| Population | Match strategy | Subpath identified? | Signature/string anchor hits | Today's outcome |
|---|---|---|---|---|
| **Surface matches** | direct source/hash match | yes (e.g. `lodash/property.js`) | ≥1 | accepted |
| **Closure matches** | `dependency_closure_ownership` | no | 0 | rejected |

Closure matches are package-internal helpers (e.g. `lodash/_baseGet`, `lodash/_root`). They are correctly attributed to the package, but the matcher cannot point at a specific subpath because the file content was inlined or the bundler stripped path hints.

The current proof gate insists every accepted attribution name **one** safe subpath. Internal helpers cannot satisfy that — they have no public subpath. So they fall out into emitted source, even though the consumer code only ever reaches them through public entries that are already externalized.

This is the root cause of the acceptance gap. The same pattern explains the per-package acceptance rates: lodash 4.5%, zod 4.4%, react 3.2%, `@aws-sdk/client-bedrock` 0.5% — all packages where the bundle exploded internal helpers across many modules and the matcher's "single subpath" rule cannot bind them.

## 2. Strategy

Stop treating each module's externalization as an independent proof. Treat the **package version** as the externalization unit, and let internal modules of an externalized package dissolve.

A package version is **externalizable** when:

1. At least one module of that package version is accepted today (`emission_mode=external_import`, `external_import_proof=matched_package_source`); AND
2. A top-level entry hint exists in `package_externalization_hints` for that `(package_name, package_version)` (giving us a public surface to import from); AND
3. No accepted external module of that package version has been rejected by the cycle-breaking / consumer-resolution check (`all_incoming_consumers_resolved=true` in evidence).

For every module whose attribution today is `rejected` *only* because of "ownership without subpath proof", we promote it to one of two new accepted states:

- **`external_import` via package top-level**: if the module has at least one cross-package consumer, attribute it to the package's top-level export specifier (e.g. `lodash`) so callers see a public surface import. The module body is dropped from emitted source.
- **`internal_to_externalized_package`** (new `emission_mode`): no cross-package consumer; module is internal to the now-externalized package. Module body dropped, all references rewritten to call sites of the package's public API (resolved via `package_externalization_hints.public_members_json`).

Both states still count as "external" — neither generates source code in the project output.

## 3. Components

Three changes, scoped by existing crate boundaries:

### 3.1 `reverts-package-matcher`: closure-ownership acceptance path

- New acceptance branch in `acceptance.rs`: when a candidate has `match_strategy='dependency_closure_ownership'`, no subpath, but the package version is already externalizable per §2, produce an attribution with the new `emission_mode='internal_to_externalized_package'` (or `external_import` against the top-level specifier if there is a cross-package consumer in the dep graph).
- The existing "single safe subpath" check stays unchanged for direct matches.

### 3.2 `reverts-package-index`: package-version externalizability index

- A read-only view computed once per pipeline run: `package_version → {externalizable: bool, top_level_specifier: String, public_members: Vec<String>}`.
- Inputs: `package_externalization_hints` + currently-accepted attributions.
- Used by the matcher branch above and by the planner to choose between top-level vs. subpath imports.

### 3.3 `reverts-emitter`: drop dissolved modules, rewrite references

- For modules accepted with `emission_mode in ('external_import', 'internal_to_externalized_package')` against the same package version, the emitter:
  - Does not emit a source file.
  - Rewrites `__webpack_require__(<id>)` / equivalent runtime calls in consumers to use the package's public surface (`import * as Lodash from 'lodash'` + member-access), driven by `public_members_json`.
- Audit invariant added: no emitted file references a dissolved module id.

### 3.4 Schema additions (additive only, no migrations to existing data)

- Extend `package_attributions.emission_mode` CHECK to include `internal_to_externalized_package`.
- Add `external_import_policy_version=2` to mark attributions produced by this path, so old runs remain interpretable.

## 4. Data flow

```
modules + module_matches
    │
    ▼
reverts-package-matcher (existing direct-subpath proof) ──► attributions accepted (surface)
    │
    ▼
reverts-package-index.externalizability_view  ──► per-package-version capability
    │
    ▼
reverts-package-matcher (new closure-ownership acceptance) ──► attributions accepted (internal/top-level)
    │
    ▼
reverts-planner ──► emit plan (dissolved set, public-surface rewrites)
    │
    ▼
reverts-emitter ──► no source for dissolved modules, rewritten consumer code
    │
    ▼
audit: every consumer reference resolves to an external import; bundle entry never statically requires a dissolved module body.
```

## 5. Error handling

- If §2 conditions are not met for a package version, the new acceptance branch is a no-op — modules continue to be rejected the same way they are today. No regressions.
- If `public_members_json` is empty for a hint, the package is not externalizable; modules stay rejected. (Avoids emitting unresolvable member accesses.)
- A cycle of dissolved modules where some consumer is *itself* a rejected module triggers a planner audit failure, not a silent fallback. ADR 0002: no post-write repair.

## 6. Tests

All self-contained per ADR 0003.

- Fixture project with a synthetic "lodash-like" package: 5 modules, 1 directly matched (`property.js`), 4 closure-owned internal helpers. Expect 5/5 accepted, 0 emitted source files, consumer rewritten to a top-level `import * as L from 'lodash'`.
- Fixture where the package has no externalization hint → closure-owned modules still rejected with the existing reason.
- Fixture where a closure-owned module has a consumer outside the package and outside the externalized set → planner rejects and reports, no silent application_source fallback.
- DB integration test using a slim copy of `~/.reverts/.reverts.db` filtered to one package, asserting accept-rate jumps from ≤10% to ≥90% on that slice.

## 7. Success criterion

Re-running the matcher + planner against `~/.reverts/.reverts.db` produces:

- `package_attributions` with `status='accepted'` AND `emission_mode in ('external_import','internal_to_externalized_package')` covering ≥ **90%** of `module_category='package'` modules (10,070 of 11,189).
- Audit clean: every emitted file parses, no reference points at a dissolved module.

## Phase A outcome (2026-05-23)

The `reverts-rollup-probe` binary, run against `~/.reverts/.reverts.db` with the iterated oracle, reports:

- package modules: 11,189
- already accepted (today): 4,243 (37.9%)
- projected rolled-up (new): 6,421 (57.4%)
- still rejected: 36 (0.3%)
- **projected external import ratio: 0.9531** — exceeds the 0.90 gate

Final oracle rule that produced this result: a `(package_name, package_version)` is `Externalizable` when (a) the matcher recorded **any** attribution for that version in this DB (accepted or rejected — both prove the matcher saw the package in the bundle) **and** (b) the externalization-hint table contains a row where `export_specifier = package_name` for that version (proving the package can be imported by its bare name). The original "≥30% direct-match ratio" floor was relaxed to 0 — for bundled libraries that explode into many internal helpers (lodash, zod, react, msal, aws-sdk), the direct-match ratio is naturally low even when externalization is safe, so the floor was filtering out exactly the cases we wanted to roll up.

Top still-rejected packages after rollup: `@aws-sdk/client-bedrock` (216), `zod` (77 of 298), `@opentelemetry/otlp-exporter-base` (67) — these have version-mismatched evidence (e.g. attribution version differs from hint version) and need a separate version-reconciliation pass; deferred out of Phase A.

## 8. Out of scope

- Reclassifying `application` / `unknown` modules into `package` (separate problem, ~16k modules — would be a follow-up to push the all-modules import ratio higher than 36%).
- Improving direct-subpath matching recall (orthogonal — would shift work from "rollup" to "surface" but doesn't change the 90% target).
- Cross-package version reconciliation when two versions of the same package coexist in one project (already handled by existing version-mismatch rejection path).
