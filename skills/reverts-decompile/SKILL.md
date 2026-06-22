---
name: reverts-decompile
description: Validate ReverTS decompile output with source/profile-selected install, compile, runtime, and UI tests, routing failures back to pipeline fixes.
---

# ReverTS Decompile Workflow

Use this skill when exporting a decompiled project from ReverTS and validating that the output is installable, compilable, and runnable.

## Install

This skill drives the `reverts-cli` binary. Build it with
`cargo build --release --bin reverts-cli` and put `./target/release/reverts-cli`
on your `PATH`. See [skills/README.md](../README.md#install) for the full install
matrix (`./skills/install` for local-dev symlink installation of the skill
bundle). After installing, restart the Claude/Codex session so the skill
registry rebinds.

This skill picks up where [decompile](../decompile/SKILL.md) finishes:
`decompile` produces structurally-valid generated `.ts`; `reverts-decompile`
runs `pnpm install` / `tsc` / startup against that output and pushes any
mechanical defects back into the ReverTS pipeline.

## Agent Boundary

Mechanical recovery is a ReverTS product capability. The Agent must not treat
manual edits to generated files, generated manifests, imports, exports, or stubs
as the final fix. When validation fails, record the defect as a pipeline/work-item
problem and implement the mechanism in ReverTS with tests. The Agent's durable
role is semantic naming through rename worklists, not patching generated output.

## Post-decompile validation contract

When invoked after any Agent/Ant decompile request, run this contract every
time; do not treat generation alone as completion.

1. Regenerate the output from the current ReverTS pipeline. Do not validate a
   stale generated tree.
2. Install dependencies in the generated output directory:
   - prefer `pnpm install`
   - use `npm install` only when the generated project clearly targets npm or
     pnpm is unavailable
3. Run compile/edit validation with real type checking:
   - `tsc --noEmit -p tsconfig.json`
   - never use `--noCheck`
   - run generated-tree structural/synthesis audits when available
4. Use [Validation tool and profile selection](#validation-tool-and-profile-selection) to choose
   and run the correct [Runtime smoke validation](#runtime-smoke-validation).
   Browser extensions require Playwright-driven UI interaction checks, not just
   extension-load checks.
5. If any step fails, classify the failure with this skill's triage buckets,
   add or update a ReverTS regression test for the durable mechanism, fix the
   pipeline, regenerate, and rerun this full contract.
6. Report the final output path, commands run, runtime smoke artifact/log path,
   and any explicitly accepted residual risk.

## Validation tool and profile selection

Validation must use scripted tools, not manual inspection. Select the runtime
smoke by recovered artifact profile first
<!-- TODO(reverts-cli): list_app_artifacts / get_artifact_manifest have no direct CLI equivalent -->,
then by generated manifests only when metadata is absent. Run every applicable
profile when an artifact has multiple entrypoints.

Tool/profile details are in
[runtime-validation-profiles.md](references/runtime-validation-profiles.md):
`reverts-cli` for project metadata and decompile status, shell commands for
install/`tsc`/Node checks, Playwright MCP for browser-extension and web UI,
and Electron shell/CDP checks for Electron apps.

## Core workflow

1. Export the project with the normal decompile pipeline. Do not patch generated output manually unless the same change is being implemented in the pipeline with tests.
2. Read the generated package-related information before validating runtime:
   - generated `package.json`
   - package modules and recovered package versions from the project graph
   - bare package imports inferred from generated source
   - any manifest-generation tests that already encode dependency rules
3. After export, attempt dependency installation in the output directory:
   - prefer `pnpm install`
   - if `pnpm` is unavailable or the generated output clearly targets an `npm`-only flow, use `npm install`
4. If installation fails or reports peer/dependency conflicts, treat that as a manifest-generation defect, not as a user-environment issue.
5. Resolve conflicts by fixing the decompile/export pipeline mechanisms:
   - reconcile incompatible peer dependency sets into a coherent runtime-compatible version group
   - use recovered package metadata and import evidence as the source of truth
   - prefer generalized manifest rules and compatibility normalization over package-specific output patches
   - do not hardcode fixes around specific package names when resolving dependency conflicts; derive the correction from package metadata, installed package manifests, peer dependency declarations, and import evidence
   - do not leave fixes as exported-project manifest edits; use exported evidence to implement a reusable pipeline or manifest-generation mechanism
6. Add or update automated tests before the fix:
   - unit or fixture tests for manifest/dependency normalization rules
   - runtime-oriented validation when a conflict pattern affects execution
   - tests must not depend on external databases or pre-existing exported files
7. Re-run the validation loop after each fix:
   - dependency installation
   - `tsc --noEmit -p tsconfig.json` (no `--noCheck`; the triage below
     depends on real type errors being reported)
   - if `tsc` fails, run the [TypeScript compile triage](#typescript-compile-triage)
     before touching dependency resolution — a structural codegen defect
     masks every downstream package-conflict signal
   - startup command for the exported project
8. After the exported project is validated and the pipeline fix is complete, persist the corrected module-to-package mapping in project storage <!-- TODO(reverts-cli): update_modules / persisted module-package mapping has no direct CLI equivalent -->. Do not leave corrected package relationships only in generated output or transient analysis state.

## TypeScript compile triage

When `tsc --noEmit -p tsconfig.json` fails on the exported project, do not
hand-edit the generated `.ts` to silence errors and do not "fix it later"
during a runtime pass. Triage the errors against the buckets below and route
each bucket to the correct durable repair.

The triage runs before any package-resolution work in the rest of this
skill: a structural defect that ships broken modules will mask all dependency
diagnostics that follow.

### Step 1 — Collect the full error set

1. Run `tsc --noEmit -p tsconfig.json 2> tsc.log` (or pipe to a log file you
   can re-read).
2. Aggregate by error code:
   `grep -oE 'TS[0-9]+' tsc.log | sort | uniq -c | sort -rn`.
3. Aggregate by file:
   `awk -F'[(:]' '/error TS/{print $1}' tsc.log | sort | uniq -c | sort -rn`.

Use the aggregates — not individual error lines — to decide which bucket
each cluster belongs to.

### Step 2 — Bucket the errors

| Bucket | Typical TS codes | Root cause class | Where to fix |
|---|---|---|---|
| **A. Structural decompile defects** | TS2451, TS2448, TS2449, TS2300, TS2304 (when target was emitted) | Codegen synthesized an import while a local declaration of the same name already exists; or hoist/forward-ref recovery missed a binding | ReverTS pipeline (codegen / import-synthesis / hoist passes). Run [decompile](../decompile/SKILL.md) Phase 5.1a/5.1b audit, file the finding with a regression test, regenerate. |
| **B. Cross-source-partition import** | TS2451 + TS2304 mass duplications between independent artifact source units; runtime smoke may fail with TDZ/foreign chunk initialization | Pipeline synthesized imports from same-name symbols without original dependency evidence between source partitions | ReverTS pipeline (source-partition import synthesis). Run [decompile](../decompile/SKILL.md) Phase 5.1b audit, file evidence-tagged finding, regenerate. |
| **C. Type narrowing limitations** | TS2339, TS2349, TS2538, TS2351 on minified short-name initializers (`Object.freeze({})`, `[]`, `''`, primitive boxed types) | Strict TS narrows the inferred type tighter than the original JS use; widen pass did not fire on this construct | ReverTS pipeline (widen-insertion phase). File as widen-coverage finding; regenerate. Do NOT lower `strict` in tsconfig to mask the cluster. |
| **D. Package-attribution defects** | TS2305 (`Module 'X' has no exported member 'Y'`), TS2307 (`Cannot find module 'X'`) | Wrong `cat=pkg` / `pkg=...` / `ver=...` on a module, or import bound to the wrong package version | ReverTS pipeline (classification / version resolution). Use [decompile](../decompile/SKILL.md) Phase 5.1c misclassification scan, then correct the module-package attribution and regenerate <!-- TODO(reverts-cli): update_modules has no direct CLI equivalent -->. |
| **E. Manifest/dependency conflicts** | TS errors caused by missing `@types/<pkg>` or wrong installed version | Generated `package.json` does not match the package-attribution evidence | Manifest-generation rules in this skill's [Version conflict diagnosis during export validation](#version-conflict-diagnosis-during-export-validation). |
| **F. Genuine TS-strict gaps in target code** | TS errors in code paths the original bundle never type-checked | Original source had unsound JS that TS surfaces; not a decompile defect | Document, do not "fix". The exported project is a transcription, not a type-correctness rewrite. |

### Step 3 — Route each bucket

- Buckets A, B, C, D: file as ReverTS pipeline issues with a regression test.
  Do not hand-edit the generated `.ts`. Once the pipeline fix lands,
  regenerate and re-run the triage.
- Bucket E: handled by the dependency-conflict workflow below.
- Bucket F: record in the PR/notes and stop — these are not export-blockers.

### Step 4 — Confirm progress between iterations

After each pipeline fix and regeneration, the `tsc` error count for the
buckets you targeted MUST strictly decrease, and no error code from a
different bucket should newly appear. If the count plateaus or oscillates,
the fix did not address the root cause — re-triage instead of looping.

## Version conflict diagnosis during export validation

When exported code reaches the dependency-validation stage, dependency conflicts
are validation failures. The Agent may inspect evidence, but the durable repair
must be implemented in ReverTS code and tests rather than left as a hand edit in
the exported project.

### Trigger

Run this flow whenever one of these happens after export:

- `pnpm install` or `npm install` reports peer dependency conflicts
- install succeeds but startup fails inside an installed package dependency chain
- installed transitive runtime packages reveal a version set that is incompatible with the exported top-level manifest

### Required process

1. Inspect the exported `package.json`.
2. Inspect installed package manifests under `node_modules` for the failing dependency chain:
   - `dependencies`
   - `peerDependencies`
   - installed transitive runtime package versions
3. Infer the coherent runtime-compatible version set from the installed evidence.
4. Encode the compatible version group as a manifest-generation or package-attribution mechanism in ReverTS.
5. Regenerate the output and re-run:
   - `pnpm install` or `npm install`
   - `tsc --noEmit -p tsconfig.json` (no `--noCheck`)
   - startup command
6. If the conflict is caused by incorrect module-to-package or package-version attribution, update that relationship in project storage after validation succeeds <!-- TODO(reverts-cli): module-package attribution persistence has no direct CLI equivalent -->.

### Constraints

- Use exported-project edits only as disposable experiments to understand the failure. Do not submit them as the final fix.
- Do not hardcode one-off package names in the general workflow description; derive corrections from installed dependency metadata and peer constraints.
- Treat package groups as runtime compatibility clusters rather than independent version pins.
- If the same conflict pattern recurs across exports, then promote the repair into a reusable pipeline or manifest-generation mechanism.

## Dependency-resolution rules

- Do not stop after generating `package.json`; exported code is not considered validated until installation has been attempted.
- Do not hand-edit the exported project's dependencies as an end state. Move the rule into manifest generation.
- Prefer mechanism-level reasoning:
  - peer-set harmonization
  - installable version normalization
  - alias-to-published-package resolution
  - runtime-compatible import/export interop constraints
- When manifest conflicts remain after export, the Agent should actively run `pnpm install` or `npm install`, inspect the resulting peer/dependency errors and installed package manifests, and then update the pipeline or manifest-generation logic based on those constraints.
- Prefer using exported project evidence to derive a mechanism-level fix. Do not normalize manual manifest repair as the workflow outcome.
- Use the installed dependency graph as evidence:
  - inspect package `peerDependencies`
  - inspect installed transitive runtime packages when startup fails despite successful install
  - derive the minimal coherent version set that satisfies the detected peer constraints
- When multiple packages form one runtime cluster, validate them as a set rather than pinning one package in isolation.
- After fixing package classification or manifest-generation defects, sync the corrected module/package relationship back into project storage so future exports reuse the repaired mapping <!-- TODO(reverts-cli): module-package mapping persistence has no direct CLI equivalent -->.

## Runtime smoke validation

Compilation passing is necessary but not sufficient. Runtime smoke is the hard
completion gate and must be source/profile-selected using
[runtime-validation-profiles.md](references/runtime-validation-profiles.md).
A clean `tsc` run does not replace browser-extension UI checks, Electron
main/renderer checks, web-app Playwright checks, CLI command smoke, or
node-library import smoke.

If smoke fails, triage it with the same buckets as TypeScript compile errors,
add/update a pipeline regression test, fix ReverTS, regenerate, and rerun the
selected validation profile. Do not hand-patch the generated tree to make smoke
pass.

## Done criteria

An exported project is only considered complete when all of the following
succeed in the generated output directory:

- dependency installation via `pnpm install` or `npm install`
- TypeScript compile check (`tsc --noEmit`) reports zero errors, OR every
  remaining error is in bucket F of the [TypeScript compile triage](#typescript-compile-triage)
  and is documented in the PR/notes
- the profile-specific [Runtime smoke validation](#runtime-smoke-validation)
  passes with zero `console.error` and zero uncaught exceptions; a clean
  `tsc` run does NOT substitute for smoke pass
- any newly added tests covering the recovered failure mode (compile- AND
  runtime-level), keyed to the bucket the failure was triaged into
- module/package mapping persisted in project storage when the fix changes package ownership or package identity <!-- TODO(reverts-cli): module-package mapping persistence has no direct CLI equivalent -->
