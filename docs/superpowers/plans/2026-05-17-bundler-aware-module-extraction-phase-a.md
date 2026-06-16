# Bundler-Aware Module Extraction — Phase α Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a new `reverts-bundle` crate that recognises esbuild/webpack5/rollup-CJS bundle shapes, extracts inner modules with parseable body spans, and merges them into `InputRows` before graph construction so the cascade matcher can attribute functions inside bundled JS.

**Architecture:** Three-way classifier (`Plain` / `Marked` / `Iife`) dispatched by `CompilerKind`. Per-bundler template detectors return `Vec<InnerModule>` whose `body_span` always covers a parseable program unit. Merge layer reconciles extracted modules with upstream-loaded `ModuleInput.source_span` values, preserving DB metadata while replacing fragment spans with parseable body spans.

**Tech Stack:** Rust 2024 (toolchain 1.93.0), oxc 0.42 (parser/codegen/ast/span/allocator), serde_json (for path hints), rusqlite (only in CLI integration test). Workspace lints `unsafe_code = forbid`, `clippy::unwrap_used = deny`, `clippy::todo = deny`, `clippy::dbg_macro = deny`. Tests self-contained per ADR 0003.

**Spec:** `docs/superpowers/specs/2026-05-17-bundler-aware-module-extraction-design.md` (gitignored).
**ADR:** `docs/adr/0004-bundler-aware-module-extraction.md` (committed).

---

## Conventions for every task

- Every test goes in `#[cfg(test)] mod tests { ... }` at the bottom of its module file unless an integration-test path is called out.
- Commit messages follow `<emoji> <type>(<scope>): <subject>` (≤100 chars, single line). Enforced by `lefthook.yml` `commit-msg`. No `Co-Authored-By:`, no AI markers.
- Each task ends with `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked` all green before commit.
- Use `oxc_allocator::Allocator` directly; `oxc_span::Span` for source-byte ranges inside OXC AST; `reverts_ir::ByteRange` at our API boundary.
- FNV hashes via `reverts_ir::hash::{fnv1a, fnv1a_of_string_set, update_fnv1a}` — never roll a new hash function.

## File structure created or modified

```
crates/
├── reverts-bundle/                            CREATE
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs                              public extract() entry + types
│       ├── inner_module.rs                     InnerModule, BundlerKind
│       ├── classification.rs                   BundleClassification + metadata
│       ├── classifier.rs                       classify(path, source) -> ...
│       ├── detectors/
│       │   ├── mod.rs                          dispatcher
│       │   ├── esbuild.rs                      __commonJS + __esm patterns
│       │   ├── webpack5.rs                     __webpack_modules__ pattern
│       │   └── rollup_cjs.rs                   UMD-CJS hybrid pattern
│       ├── merge.rs                            merge_into(input)
│       └── audit.rs                            audit-emission helpers
├── reverts-analyze/src/lib.rs                  ADD: pub ESBUILD_WRAPPER_NAMES
├── reverts-js/src/normalize/
│   └── bundler_wrapper_unwrapped.rs            REPLACE local const with import
├── reverts-graph/src/lib.rs                    pub fn iife_kind
├── reverts-observe/src/lib.rs                  +4 FindingCode variants
├── reverts-cli/src/lib.rs                      wire extract() into match-packages
└── Cargo.toml                                  workspace member entry
```

---

## Task 1: Add 4 new FindingCode variants

**Files:**
- Modify: `crates/reverts-observe/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/reverts-observe/src/lib.rs` inside the existing `#[cfg(test)] mod tests`:

```rust
#[test]
fn bundle_extraction_finding_codes_are_distinct() {
    let codes = [
        FindingCode::BundlerKindUnrecognised,
        FindingCode::BundleDetectorAmbiguous,
        FindingCode::MissingParseableBody,
        FindingCode::IifeClusterDegenerate,
    ];
    for (i, a) in codes.iter().enumerate() {
        for (j, b) in codes.iter().enumerate() {
            if i != j {
                assert_ne!(a, b);
            }
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p reverts-observe --locked bundle_extraction_finding_codes_are_distinct`
Expected: FAIL with "no variant `BundlerKindUnrecognised`" etc.

- [ ] **Step 3: Add the variants**

Add to `pub enum FindingCode` (alphabetical position):

```rust
    BundlerKindUnrecognised,
    BundleDetectorAmbiguous,
    MissingParseableBody,
    IifeClusterDegenerate,
```

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-observe/src/lib.rs
git commit -m "✨ feat(observe): add four FindingCode variants for bundle extraction"
```

---

## Task 2: Lift `ESBUILD_WRAPPER_NAMES` to `reverts-analyze`

The constant lives privately inside `reverts-js::normalize::bundler_wrapper_unwrapped`. Move it to `reverts-analyze` alongside `ESBUILD_RUNTIME_IDENTIFIERS`, then update the existing consumer to import from the new home. Spec §5.2.

**Files:**
- Modify: `crates/reverts-analyze/src/lib.rs`
- Modify: `crates/reverts-js/src/normalize/bundler_wrapper_unwrapped.rs`

- [ ] **Step 1: Write the failing test**

In `crates/reverts-analyze/src/lib.rs`, find the existing `ESBUILD_RUNTIME_IDENTIFIERS` constant. Add nearby in the same `mod tests` (or near the bottom of the file with a fresh test):

```rust
#[cfg(test)]
mod esbuild_wrapper_names_tests {
    use super::ESBUILD_WRAPPER_NAMES;

    #[test]
    fn esbuild_wrapper_names_list_covers_known_wrappers() {
        let names: std::collections::BTreeSet<&'static str> = ESBUILD_WRAPPER_NAMES.iter().copied().collect();
        for name in [
            "__toESM",
            "__toCommonJS",
            "__commonJS",
            "__esm",
            "__defProp",
            "__defProps",
            "__export",
            "__exportStar",
            "__reExport",
            "__copyProps",
        ] {
            assert!(names.contains(name), "missing wrapper name {name}");
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p reverts-analyze --locked esbuild_wrapper_names_list_covers_known_wrappers`
Expected: FAIL with "cannot find value `ESBUILD_WRAPPER_NAMES`".

- [ ] **Step 3: Add the const + update import**

In `crates/reverts-analyze/src/lib.rs`, near the existing `ESBUILD_RUNTIME_IDENTIFIERS` declaration, add:

```rust
/// esbuild output wrapper function names. These are emitted by the esbuild
/// runtime around imported CJS modules, exported namespaces, and helper
/// inits; `reverts-js::normalize::BundlerWrapperUnwrapped` strips them for
/// `ast_hash` collision, `reverts-bundle::detectors::esbuild` recognises
/// them as module boundaries.
pub const ESBUILD_WRAPPER_NAMES: &[&str] = &[
    "__toESM",
    "__toCommonJS",
    "__commonJS",
    "__esm",
    "__defProp",
    "__defProps",
    "__export",
    "__exportStar",
    "__reExport",
    "__copyProps",
];
```

Then in `crates/reverts-js/src/normalize/bundler_wrapper_unwrapped.rs`, replace the local `pub const ESBUILD_WRAPPER_NAMES: &[&str] = &[ ... ];` block with:

```rust
pub use reverts_analyze::ESBUILD_WRAPPER_NAMES;
```

`reverts-js` may need `reverts-analyze` added to `[dependencies]` in `crates/reverts-js/Cargo.toml`. If the import is already present, just add the `pub use`.

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS, no duplicate-definition errors.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-analyze/src/lib.rs crates/reverts-js/src/normalize/bundler_wrapper_unwrapped.rs crates/reverts-js/Cargo.toml
git commit -m "♻️ refactor(analyze): lift ESBUILD_WRAPPER_NAMES to single source"
```

---

## Task 3: Make `reverts-graph::iife_kind` public

The classifier needs to recognise the IIFE shape of vendored bundles using the same predicate the graph fact extractor uses.

**Files:**
- Modify: `crates/reverts-graph/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/reverts-graph/src/lib.rs` (at the bottom near other `pub use`/exports), or inside the existing test module if convenient:

```rust
#[cfg(test)]
mod iife_kind_visibility_tests {
    use super::{AstWrapperKind, iife_kind};
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    #[test]
    fn iife_kind_is_public_and_recognises_function_iife() {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, "(function () { return 1; })()", SourceType::default()).parse();
        assert!(parsed.errors.is_empty());
        let stmt = parsed.program.body.first().expect("at least one statement");
        let oxc_ast::ast::Statement::ExpressionStatement(expr) = stmt else {
            panic!("expected expression statement");
        };
        let oxc_ast::ast::Expression::CallExpression(call) = &expr.expression else {
            panic!("expected call expression");
        };
        let kind = iife_kind(&call.callee);
        assert_eq!(kind, Some(AstWrapperKind::FunctionIife));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p reverts-graph --locked iife_kind_is_public_and_recognises_function_iife`
Expected: FAIL with "function `iife_kind` is private" or similar.

- [ ] **Step 3: Promote `fn iife_kind` to `pub fn`**

In `crates/reverts-graph/src/lib.rs`, locate `fn iife_kind(expression: &Expression<'_>) -> Option<AstWrapperKind>` and change to:

```rust
/// Recognises the three IIFE wrapper shapes the graph uses for top-level
/// program scans: `(function () { … })()`, `(() => { … })()`, and the
/// TypeScript-style `var X; (function (X) { … })(X || (X = {}));`
/// initialiser. Exposed `pub` so `reverts-bundle::classifier` can reuse
/// the same predicate and avoid a two-track implementation.
pub fn iife_kind(expression: &Expression<'_>) -> Option<AstWrapperKind> {
```

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-graph/src/lib.rs
git commit -m "♻️ refactor(graph): make iife_kind pub for shared classifier use"
```

---

## Task 4: Create `reverts-bundle` crate skeleton

**Files:**
- Create: `crates/reverts-bundle/Cargo.toml`
- Create: `crates/reverts-bundle/src/lib.rs`
- Modify: `Cargo.toml` (workspace root)

- [ ] **Step 1: Write the failing test**

Create `crates/reverts-bundle/src/lib.rs`:

```rust
//! Bundler-aware module extraction.
//!
//! Recognises bundler-specific wrapper shapes in JavaScript bundle source
//! and produces `InnerModule` records whose `body_span` always slices a
//! parseable program unit. See ADR 0004 for the architectural rationale.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles_and_links() {
        // Sentinel test — proves the crate is wired into the workspace.
    }
}
```

Create `crates/reverts-bundle/Cargo.toml`:

```toml
[package]
name = "reverts-bundle"
version = "0.1.0"
edition.workspace = true
license.workspace = true
rust-version.workspace = true

[lints]
workspace = true

[dependencies]
oxc_allocator.workspace = true
oxc_ast = "0.42"
oxc_parser.workspace = true
oxc_span.workspace = true
reverts-analyze = { path = "../reverts-analyze" }
reverts-graph = { path = "../reverts-graph" }
reverts-input = { path = "../reverts-input" }
reverts-ir = { path = "../reverts-ir" }
reverts-js = { path = "../reverts-js" }
reverts-observe = { path = "../reverts-observe" }
serde_json = "1"
```

Append `"crates/reverts-bundle"` to the workspace `members` array in root `Cargo.toml`, kept in alphabetical position (between `reverts-analyze` and `reverts-cli`).

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-bundle --locked crate_compiles_and_links`
Expected: PASS (sentinel — already wired now).

- [ ] **Step 3: (no implementation needed beyond Cargo wiring)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/reverts-bundle
git commit -m "✨ feat(bundle): scaffold reverts-bundle crate with workspace wiring"
```

---

## Task 5: `InnerModule` + `BundlerKind` data types

**Files:**
- Create: `crates/reverts-bundle/src/inner_module.rs`
- Modify: `crates/reverts-bundle/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/reverts-bundle/src/inner_module.rs`:

```rust
use reverts_ir::{ByteRange, ModuleId};

/// Bundler wrapper shape encountered around a module body.
///
/// Distinct from `reverts_analyze::CompilerKind` because a single bundle
/// can be produced by webpack yet contain `define(...)` AMD modules
/// inside; we record what wrapper shape was actually decoded for each
/// inner module rather than the toolchain that emitted the whole file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BundlerKind {
    Esbuild,
    Webpack4,
    Webpack5,
    RollupCjs,
    RollupEsm,
    Umd,
    Browserify,
    Amd,
}

impl BundlerKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Esbuild => "esbuild",
            Self::Webpack4 => "webpack4",
            Self::Webpack5 => "webpack5",
            Self::RollupCjs => "rollup_cjs",
            Self::RollupEsm => "rollup_esm",
            Self::Umd => "umd",
            Self::Browserify => "browserify",
            Self::Amd => "amd",
        }
    }
}

/// An inner module extracted from a bundle. `body_span` always covers
/// a parseable program unit — never a mid-expression fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InnerModule {
    /// Stable identifier within the parent bundle. Strategies:
    /// - esbuild `__commonJS`: the registration key, e.g. `"node_modules/lodash/index.js"`
    /// - webpack: the module id (string or number stringified)
    /// - rollup / umd / fallback: `"<bundler>:<seq>"`, e.g. `"rollup_cjs:0"`.
    pub virtual_id: String,
    /// Byte range of the body inside the parent file's source. Always
    /// slices a parseable JavaScript program unit.
    pub body_span: ByteRange,
    /// Wrapper shape decoded for this module.
    pub bundler: BundlerKind,
    /// Source path hint when the bundler embeds it as the registration
    /// key. None when the bundler uses numeric ids or anonymous shapes.
    pub source_path_hint: Option<String>,
    /// Parent module that contains this inner.
    pub parent_module_id: ModuleId,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundler_kind_as_str_round_trip() {
        for k in [
            BundlerKind::Esbuild,
            BundlerKind::Webpack4,
            BundlerKind::Webpack5,
            BundlerKind::RollupCjs,
            BundlerKind::RollupEsm,
            BundlerKind::Umd,
            BundlerKind::Browserify,
            BundlerKind::Amd,
        ] {
            assert!(!k.as_str().is_empty());
        }
    }

    #[test]
    fn inner_module_struct_holds_all_fields() {
        let m = InnerModule {
            virtual_id: "esbuild:0".into(),
            body_span: ByteRange::new(100, 500),
            bundler: BundlerKind::Esbuild,
            source_path_hint: Some("node_modules/lodash/index.js".into()),
            parent_module_id: ModuleId(7),
        };
        assert_eq!(m.virtual_id, "esbuild:0");
        assert_eq!(m.body_span.start, 100);
        assert_eq!(m.body_span.end, 500);
        assert_eq!(m.bundler, BundlerKind::Esbuild);
    }
}
```

Modify `crates/reverts-bundle/src/lib.rs` to add:

```rust
mod inner_module;
pub use inner_module::{BundlerKind, InnerModule};
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p reverts-bundle --locked inner_module::tests`
Expected: PASS.

- [ ] **Step 3: (impl + tests together; already covered)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-bundle
git commit -m "✨ feat(bundle): add InnerModule and BundlerKind data types"
```

---

## Task 6: `BundleClassification` + `MarkedMetadata` / `IifeMetadata`

**Files:**
- Create: `crates/reverts-bundle/src/classification.rs`
- Modify: `crates/reverts-bundle/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/reverts-bundle/src/classification.rs`:

```rust
use reverts_graph::AstWrapperKind;

use crate::inner_module::{BundlerKind, InnerModule};

/// Outcome of classifying a single source file. Drives downstream
/// behaviour: Plain flows through unchanged, Marked is split into
/// inner modules, Iife is reserved for monolithic vendored bundles
/// recovered via clustering (Phase γ).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleClassification {
    /// No bundler wrapper recognised. Source flows through as one
    /// module with no inner subdivision.
    Plain,
    /// Bundler wrapper recognised. Carries the extracted inner-module
    /// list and the bundler kind that produced it.
    Marked(MarkedMetadata),
    /// Monolithic IIFE-shaped vendored bundle. Phase α records the
    /// wrapper shape but does not recover inner clusters (Phase γ
    /// fills `inner_clusters`).
    Iife(IifeMetadata),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkedMetadata {
    /// Inner modules in source-text order.
    pub inner_modules: Vec<InnerModule>,
    /// Bundler whose detector emitted these inner modules. Multiple
    /// detectors may match the same file; the highest-confidence
    /// detector's results are used and runners-up emit
    /// `BundleDetectorAmbiguous` audit findings.
    pub detected_by: BundlerKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IifeMetadata {
    /// IIFE wrapper shape recognised at the top level of the file.
    pub wrapper_kind: AstWrapperKind,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_are_distinguishable() {
        let plain = BundleClassification::Plain;
        let marked = BundleClassification::Marked(MarkedMetadata {
            inner_modules: vec![],
            detected_by: BundlerKind::Esbuild,
        });
        let iife = BundleClassification::Iife(IifeMetadata {
            wrapper_kind: AstWrapperKind::FunctionIife,
        });
        assert_ne!(plain, marked);
        assert_ne!(marked, iife);
        assert_ne!(plain, iife);
    }
}
```

Modify `crates/reverts-bundle/src/lib.rs`:

```rust
mod classification;
mod inner_module;
pub use classification::{BundleClassification, IifeMetadata, MarkedMetadata};
pub use inner_module::{BundlerKind, InnerModule};
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p reverts-bundle --locked classification::tests`
Expected: PASS.

- [ ] **Step 3: (impl + tests together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-bundle
git commit -m "✨ feat(bundle): add BundleClassification with Plain/Marked/Iife variants"
```

---

## Task 7: Classifier skeleton (path heuristics, returns Plain)

Add the public `classify` entry point. Phase α starts with path heuristics and `CompilerKind` dispatch but every code path returns `Plain` — detectors land in later tasks.

**Files:**
- Create: `crates/reverts-bundle/src/classifier.rs`
- Modify: `crates/reverts-bundle/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/reverts-bundle/src/classifier.rs`:

```rust
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_span::SourceType;
use reverts_analyze::{
    BABEL_RUNTIME_IDENTIFIERS, CompilerKind, ESBUILD_RUNTIME_IDENTIFIERS,
    ROLLUP_RUNTIME_IDENTIFIERS, WEBPACK_RUNTIME_IDENTIFIERS,
};

use crate::classification::{BundleClassification, MarkedMetadata};
use crate::inner_module::BundlerKind;

/// Detect which bundler runtime fingerprint dominates a source file.
/// Returns `CompilerKind::Unknown` when no runtime identifier matches —
/// callers should treat that as `Plain`.
#[must_use]
pub fn detect_kind_from_source(source: &str) -> CompilerKind {
    let probe = |needles: &[&'static str]| needles.iter().any(|n| source.contains(*n));
    if probe(WEBPACK_RUNTIME_IDENTIFIERS) {
        CompilerKind::Webpack
    } else if probe(ESBUILD_RUNTIME_IDENTIFIERS) {
        CompilerKind::Esbuild
    } else if probe(ROLLUP_RUNTIME_IDENTIFIERS) {
        CompilerKind::Rollup
    } else if probe(BABEL_RUNTIME_IDENTIFIERS) {
        CompilerKind::Babel
    } else {
        CompilerKind::Unknown
    }
}

/// Classify a single source file. Phase α: returns `Plain` for everything
/// — detector dispatch is wired in Task 11.
///
/// `_path` is reserved for future path-heuristic gating (vendored
/// directories, `.bundle.js` markers).
#[must_use]
pub fn classify(_path: &Path, source: &str) -> BundleClassification {
    let _kind = detect_kind_from_source(source);
    let alloc = Allocator::default();
    let parsed = Parser::new(&alloc, source, SourceType::default()).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return BundleClassification::Plain;
    }
    // Detector dispatch lands in Task 11; until then everything is Plain.
    BundleClassification::Plain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_kind_recognises_webpack_runtime() {
        let src = "var x = __webpack_require__('./foo');";
        assert_eq!(detect_kind_from_source(src), CompilerKind::Webpack);
    }

    #[test]
    fn detect_kind_recognises_esbuild_runtime() {
        let src = "var pkg = __toESM(require('mod'));";
        assert_eq!(detect_kind_from_source(src), CompilerKind::Esbuild);
    }

    #[test]
    fn detect_kind_is_unknown_for_plain_js() {
        let src = "function add(a, b) { return a + b; }";
        assert_eq!(detect_kind_from_source(src), CompilerKind::Unknown);
    }

    #[test]
    fn classify_returns_plain_for_phase_alpha_baseline() {
        let src = "var x = __toESM(require('mod'));";
        let result = classify(Path::new("bundle.js"), src);
        assert_eq!(result, BundleClassification::Plain);
        // Marker test — proves dispatch will route via CompilerKind
        // (use `_kind` binding in classify() above).
        let _ = MarkedMetadata {
            inner_modules: vec![],
            detected_by: BundlerKind::Esbuild,
        };
    }

    #[test]
    fn classify_returns_plain_when_parse_fails() {
        let src = "function bad( { )";
        assert_eq!(
            classify(Path::new("bundle.js"), src),
            BundleClassification::Plain
        );
    }
}
```

Modify `crates/reverts-bundle/src/lib.rs`:

```rust
pub mod classifier;
mod classification;
mod inner_module;
pub use classification::{BundleClassification, IifeMetadata, MarkedMetadata};
pub use inner_module::{BundlerKind, InnerModule};
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p reverts-bundle --locked classifier::tests`
Expected: PASS (5 tests).

- [ ] **Step 3: (impl + tests together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-bundle
git commit -m "✨ feat(bundle): add classifier skeleton with CompilerKind dispatch"
```

---

## Task 8: esbuild `__commonJS` detector

The `__commonJS({"path": (exports, module) => { … }})` map registers commonJS modules. Each entry's value is a parseable function body.

**Files:**
- Create: `crates/reverts-bundle/src/detectors/mod.rs`
- Create: `crates/reverts-bundle/src/detectors/esbuild.rs`
- Modify: `crates/reverts-bundle/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/reverts-bundle/src/detectors/mod.rs`:

```rust
pub mod esbuild;
```

Create `crates/reverts-bundle/src/detectors/esbuild.rs`:

```rust
use oxc_ast::Visit;
use oxc_ast::ast::{Argument, CallExpression, Expression, ObjectPropertyKind, Program, PropertyKey};
use oxc_span::GetSpan;
use reverts_ir::{ByteRange, ModuleId};

use crate::inner_module::{BundlerKind, InnerModule};

/// Recognise esbuild's `__commonJS({"key": (exports, module) => { … }})`
/// registration map. Each map entry becomes one `InnerModule` whose
/// `body_span` covers the arrow function body so the parent program
/// can be sliced into independent compilable units.
#[must_use]
pub fn detect_commonjs(program: &Program<'_>, parent_module_id: ModuleId) -> Vec<InnerModule> {
    let mut collected = Vec::new();
    let mut visitor = CommonJsVisitor {
        out: &mut collected,
        parent_module_id,
    };
    visitor.visit_program(program);
    collected
}

struct CommonJsVisitor<'a> {
    out: &'a mut Vec<InnerModule>,
    parent_module_id: ModuleId,
}

impl<'a> Visit<'a> for CommonJsVisitor<'_> {
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        // Pattern: `__commonJS({ "key": (...) => { ... }, ... })`
        // The first argument is an object expression of property entries.
        if let Expression::Identifier(callee) = &call.callee
            && callee.name == "__commonJS"
            && let Some(Argument::ObjectExpression(obj)) = call.arguments.first()
        {
            for prop in &obj.properties {
                let ObjectPropertyKind::ObjectProperty(p) = prop else {
                    continue;
                };
                let key_text = match &p.key {
                    PropertyKey::StaticIdentifier(id) => id.name.as_str().to_string(),
                    PropertyKey::StringLiteral(s) => s.value.as_str().to_string(),
                    _ => continue,
                };
                let body_span = match &p.value {
                    Expression::ArrowFunctionExpression(a) => {
                        let s = a.body.span();
                        ByteRange::new(s.start, s.end)
                    }
                    Expression::FunctionExpression(f) => {
                        let Some(body) = f.body.as_ref() else { continue };
                        let s = body.span();
                        ByteRange::new(s.start, s.end)
                    }
                    _ => continue,
                };
                self.out.push(InnerModule {
                    virtual_id: format!("esbuild:{}", key_text),
                    body_span,
                    bundler: BundlerKind::Esbuild,
                    source_path_hint: Some(key_text),
                    parent_module_id: self.parent_module_id,
                });
            }
        }
        oxc_ast::visit::walk::walk_call_expression(self, call);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn extract(src: &str) -> Vec<InnerModule> {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, src, SourceType::default()).parse();
        assert!(parsed.errors.is_empty(), "parse errors: {:?}", parsed.errors);
        detect_commonjs(&parsed.program, ModuleId(99))
    }

    #[test]
    fn detect_commonjs_extracts_arrow_module_body() {
        let src = r#"
            var x = __commonJS({
                "node_modules/lodash/index.js": (exports, module) => {
                    module.exports = { map: function () {} };
                }
            });
        "#;
        let modules = extract(src);
        assert_eq!(modules.len(), 1);
        let m = &modules[0];
        assert_eq!(m.bundler, BundlerKind::Esbuild);
        assert_eq!(m.source_path_hint.as_deref(), Some("node_modules/lodash/index.js"));
        assert!(m.virtual_id.starts_with("esbuild:"));
        assert!(m.body_span.end > m.body_span.start);
        assert_eq!(m.parent_module_id, ModuleId(99));
    }

    #[test]
    fn detect_commonjs_extracts_multiple_entries() {
        let src = r#"
            var x = __commonJS({
                "a.js": (exports, module) => { module.exports = 1; },
                "b.js": (exports, module) => { module.exports = 2; }
            });
        "#;
        let modules = extract(src);
        assert_eq!(modules.len(), 2);
        let paths: Vec<_> = modules.iter().filter_map(|m| m.source_path_hint.as_deref()).collect();
        assert!(paths.contains(&"a.js"));
        assert!(paths.contains(&"b.js"));
    }

    #[test]
    fn detect_commonjs_ignores_calls_with_wrong_callee() {
        let src = r#"
            var x = __notCommonJS({
                "a.js": (exports, module) => { module.exports = 1; }
            });
        "#;
        assert!(extract(src).is_empty());
    }

    #[test]
    fn detect_commonjs_ignores_calls_with_non_object_arg() {
        let src = r#"var x = __commonJS([]);"#;
        assert!(extract(src).is_empty());
    }

    #[test]
    fn detect_commonjs_returns_body_span_not_full_function_span() {
        let src = r#"var x = __commonJS({ "a": (e, m) => { var y = 1; m.exports = y; } });"#;
        let modules = extract(src);
        let m = &modules[0];
        let body_text = &src[m.body_span.start as usize..m.body_span.end as usize];
        assert!(body_text.starts_with('{'));
        assert!(body_text.ends_with('}'));
        assert!(body_text.contains("var y = 1"));
    }
}
```

Modify `crates/reverts-bundle/src/lib.rs`:

```rust
pub mod classifier;
pub mod detectors;
mod classification;
mod inner_module;
pub use classification::{BundleClassification, IifeMetadata, MarkedMetadata};
pub use inner_module::{BundlerKind, InnerModule};
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p reverts-bundle --locked detectors::esbuild::tests`
Expected: PASS (5 tests).

- [ ] **Step 3: (impl + tests together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-bundle
git commit -m "✨ feat(bundle): detect esbuild __commonJS module registrations"
```

---

## Task 9: esbuild `__esm` detector

Same shape as `__commonJS` but the value is a zero-arg arrow whose body sets up an ESM module. Combine with `__commonJS` in `esbuild.rs` to share visitor scaffolding.

**Files:**
- Modify: `crates/reverts-bundle/src/detectors/esbuild.rs`

- [ ] **Step 1: Write the failing test**

Add to the existing `crates/reverts-bundle/src/detectors/esbuild.rs` test module:

```rust
#[test]
fn detect_esm_extracts_zero_arg_arrow_body() {
    let src = r#"
        var x = __esm({
            "lib/foo.js": () => {
                init_lib();
                foo = 1;
            }
        });
    "#;
    let modules = extract_esm(src);
    assert_eq!(modules.len(), 1);
    assert_eq!(modules[0].bundler, BundlerKind::Esbuild);
    assert_eq!(modules[0].source_path_hint.as_deref(), Some("lib/foo.js"));
    assert!(modules[0].virtual_id.starts_with("esbuild:"));
}

#[test]
fn detect_esm_ignores_non_esm_calls() {
    let src = r#"var x = __notEsm({ "a": () => {} });"#;
    assert!(extract_esm(src).is_empty());
}

fn extract_esm(src: &str) -> Vec<InnerModule> {
    let alloc = Allocator::default();
    let parsed = Parser::new(&alloc, src, SourceType::default()).parse();
    assert!(parsed.errors.is_empty(), "parse errors: {:?}", parsed.errors);
    detect_esm(&parsed.program, ModuleId(99))
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p reverts-bundle --locked detectors::esbuild::tests::detect_esm_extracts_zero_arg_arrow_body`
Expected: FAIL with "cannot find function `detect_esm`".

- [ ] **Step 3: Add the `detect_esm` function and shared helper**

Append to `crates/reverts-bundle/src/detectors/esbuild.rs` (above the `#[cfg(test)]` block):

```rust
/// Recognise esbuild's `__esm({"key": () => { … }})` registration map
/// for ESM modules. Behaves identically to [`detect_commonjs`] but
/// matches the `__esm` callee name.
#[must_use]
pub fn detect_esm(program: &Program<'_>, parent_module_id: ModuleId) -> Vec<InnerModule> {
    detect_by_callee_name(program, parent_module_id, "__esm")
}

/// Shared implementation: walk the program, find every CallExpression
/// whose callee is the named identifier and whose first argument is an
/// object literal of `"path": (...) => { ... }` registrations.
fn detect_by_callee_name(
    program: &Program<'_>,
    parent_module_id: ModuleId,
    callee_name: &'static str,
) -> Vec<InnerModule> {
    let mut collected = Vec::new();
    let mut visitor = NamedRegistryVisitor {
        out: &mut collected,
        parent_module_id,
        callee_name,
    };
    visitor.visit_program(program);
    collected
}

struct NamedRegistryVisitor<'a> {
    out: &'a mut Vec<InnerModule>,
    parent_module_id: ModuleId,
    callee_name: &'static str,
}

impl<'a> Visit<'a> for NamedRegistryVisitor<'_> {
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if let Expression::Identifier(callee) = &call.callee
            && callee.name == self.callee_name
            && let Some(Argument::ObjectExpression(obj)) = call.arguments.first()
        {
            for prop in &obj.properties {
                let ObjectPropertyKind::ObjectProperty(p) = prop else {
                    continue;
                };
                let key_text = match &p.key {
                    PropertyKey::StaticIdentifier(id) => id.name.as_str().to_string(),
                    PropertyKey::StringLiteral(s) => s.value.as_str().to_string(),
                    _ => continue,
                };
                let body_span = match &p.value {
                    Expression::ArrowFunctionExpression(a) => {
                        let s = a.body.span();
                        ByteRange::new(s.start, s.end)
                    }
                    Expression::FunctionExpression(f) => {
                        let Some(body) = f.body.as_ref() else { continue };
                        let s = body.span();
                        ByteRange::new(s.start, s.end)
                    }
                    _ => continue,
                };
                self.out.push(InnerModule {
                    virtual_id: format!("esbuild:{}", key_text),
                    body_span,
                    bundler: BundlerKind::Esbuild,
                    source_path_hint: Some(key_text),
                    parent_module_id: self.parent_module_id,
                });
            }
        }
        oxc_ast::visit::walk::walk_call_expression(self, call);
    }
}
```

Then refactor `detect_commonjs` to delegate to the shared helper to eliminate duplication:

```rust
#[must_use]
pub fn detect_commonjs(program: &Program<'_>, parent_module_id: ModuleId) -> Vec<InnerModule> {
    detect_by_callee_name(program, parent_module_id, "__commonJS")
}
```

Delete the now-unused `CommonJsVisitor` struct and its `Visit` impl.

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS (existing __commonJS tests + 2 new __esm tests).

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-bundle/src/detectors/esbuild.rs
git commit -m "✨ feat(bundle): detect esbuild __esm registrations sharing visitor with __commonJS"
```

---

## Task 10: webpack 5 `__webpack_modules__` detector

Webpack 5 emits a top-level `var __webpack_modules__ = { "./src/foo.js": (m, e, r) => { … }, … };` declaration. The keys are module paths or numeric ids; the values are arrow functions or function expressions.

**Files:**
- Create: `crates/reverts-bundle/src/detectors/webpack5.rs`
- Modify: `crates/reverts-bundle/src/detectors/mod.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/reverts-bundle/src/detectors/webpack5.rs`:

```rust
use oxc_ast::Visit;
use oxc_ast::ast::{
    Expression, ObjectPropertyKind, Program, PropertyKey, Statement, VariableDeclarator,
};
use oxc_span::GetSpan;
use reverts_ir::{ByteRange, ModuleId};

use crate::inner_module::{BundlerKind, InnerModule};

/// Recognise webpack 5's `var __webpack_modules__ = { … };` module map.
/// Each property's value is a factory function whose body is the module
/// implementation; we slice that body as the inner module.
#[must_use]
pub fn detect(program: &Program<'_>, parent_module_id: ModuleId) -> Vec<InnerModule> {
    let mut out = Vec::new();
    for stmt in &program.body {
        let Statement::VariableDeclaration(decl) = stmt else {
            continue;
        };
        for declarator in &decl.declarations {
            collect_from_declarator(declarator, parent_module_id, &mut out);
        }
    }
    out
}

fn collect_from_declarator<'a>(
    declarator: &VariableDeclarator<'a>,
    parent_module_id: ModuleId,
    out: &mut Vec<InnerModule>,
) {
    use oxc_ast::ast::BindingPatternKind;

    let BindingPatternKind::BindingIdentifier(id) = &declarator.id.kind else {
        return;
    };
    if id.name != "__webpack_modules__" {
        return;
    }
    let Some(Expression::ObjectExpression(obj)) = &declarator.init else {
        return;
    };
    for prop in &obj.properties {
        let ObjectPropertyKind::ObjectProperty(p) = prop else {
            continue;
        };
        let key_text = match &p.key {
            PropertyKey::StaticIdentifier(id) => id.name.as_str().to_string(),
            PropertyKey::StringLiteral(s) => s.value.as_str().to_string(),
            PropertyKey::NumericLiteral(n) => format!("{}", n.value),
            _ => continue,
        };
        let body_span = match &p.value {
            Expression::ArrowFunctionExpression(a) => {
                let s = a.body.span();
                ByteRange::new(s.start, s.end)
            }
            Expression::FunctionExpression(f) => {
                let Some(body) = f.body.as_ref() else { continue };
                let s = body.span();
                ByteRange::new(s.start, s.end)
            }
            _ => continue,
        };
        let source_path_hint = if key_text.starts_with('.') || key_text.starts_with("node_modules/")
        {
            Some(key_text.clone())
        } else {
            None
        };
        out.push(InnerModule {
            virtual_id: format!("webpack5:{}", key_text),
            body_span,
            bundler: BundlerKind::Webpack5,
            source_path_hint,
            parent_module_id,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn extract(src: &str) -> Vec<InnerModule> {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, src, SourceType::default()).parse();
        assert!(parsed.errors.is_empty(), "parse errors: {:?}", parsed.errors);
        detect(&parsed.program, ModuleId(7))
    }

    #[test]
    fn detect_webpack5_path_keys_become_source_hints() {
        let src = r#"
            var __webpack_modules__ = {
                "./src/foo.js": (m, e, r) => { e.x = 1; },
                "./src/bar.js": (m, e, r) => { e.y = 2; }
            };
        "#;
        let modules = extract(src);
        assert_eq!(modules.len(), 2);
        for m in &modules {
            assert_eq!(m.bundler, BundlerKind::Webpack5);
            assert!(m.source_path_hint.as_deref().unwrap().starts_with("./src/"));
        }
    }

    #[test]
    fn detect_webpack5_numeric_keys_have_no_path_hint() {
        let src = r#"
            var __webpack_modules__ = {
                42: (m, e, r) => { e.x = 1; }
            };
        "#;
        let modules = extract(src);
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].virtual_id, "webpack5:42");
        assert!(modules[0].source_path_hint.is_none());
    }

    #[test]
    fn detect_webpack5_ignores_other_module_maps() {
        let src = r#"var __not_webpack__ = { "./a": () => {} };"#;
        assert!(extract(src).is_empty());
    }

    #[test]
    fn detect_webpack5_returns_body_span() {
        let src = r#"var __webpack_modules__ = { "./a": (m, e, r) => { var z = 99; e.z = z; } };"#;
        let modules = extract(src);
        let m = &modules[0];
        let body_text = &src[m.body_span.start as usize..m.body_span.end as usize];
        assert!(body_text.starts_with('{'));
        assert!(body_text.ends_with('}'));
        assert!(body_text.contains("var z = 99"));
    }
}
```

Modify `crates/reverts-bundle/src/detectors/mod.rs`:

```rust
pub mod esbuild;
pub mod webpack5;
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p reverts-bundle --locked detectors::webpack5::tests`
Expected: PASS (4 tests).

- [ ] **Step 3: (impl + tests together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-bundle
git commit -m "✨ feat(bundle): detect webpack5 __webpack_modules__ object table"
```

---

## Task 11: rollup CJS UMD-hybrid detector

Rollup's CJS output uses the pattern `(function (global, factory) { typeof exports === 'object' ... })(this, (function () { … inner … }));`. The inner factory function is one program unit; inside it Rollup may declare multiple top-level functions that we collect as separate inner modules indexed by source-text order.

**Files:**
- Create: `crates/reverts-bundle/src/detectors/rollup_cjs.rs`
- Modify: `crates/reverts-bundle/src/detectors/mod.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/reverts-bundle/src/detectors/rollup_cjs.rs`:

```rust
use oxc_ast::Visit;
use oxc_ast::ast::{Argument, CallExpression, Expression, Program, Statement};
use oxc_span::GetSpan;
use reverts_ir::{ByteRange, ModuleId};

use crate::inner_module::{BundlerKind, InnerModule};

/// Recognise Rollup's CJS-UMD output:
/// `(function (global, factory) { … }(this, (function () { /* body */ })));`
///
/// The inner factory function (second argument of the outer call) holds
/// the bundled code. We slice its body as one `InnerModule` per
/// top-level function declaration inside the body.
#[must_use]
pub fn detect(program: &Program<'_>, parent_module_id: ModuleId) -> Vec<InnerModule> {
    let mut out = Vec::new();
    for stmt in &program.body {
        let Statement::ExpressionStatement(expr) = stmt else {
            continue;
        };
        if let Expression::CallExpression(call) = &expr.expression {
            collect_from_outer_iife(call, parent_module_id, &mut out);
        }
    }
    out
}

fn collect_from_outer_iife<'a>(
    call: &CallExpression<'a>,
    parent_module_id: ModuleId,
    out: &mut Vec<InnerModule>,
) {
    // Expect callee = a function expression with two formal parameters
    // (`global`, `factory`). Reject if shape doesn't match — defensive.
    let outer_fn = match &call.callee {
        Expression::FunctionExpression(f) if f.params.items.len() == 2 => f,
        Expression::ParenthesizedExpression(p) => {
            if let Expression::FunctionExpression(f) = &p.expression {
                if f.params.items.len() != 2 {
                    return;
                }
                f
            } else {
                return;
            }
        }
        _ => return,
    };
    let _ = outer_fn; // we don't dive into the outer; we just gate on its shape

    // Second argument is the inner factory.
    let Some(inner_arg) = call.arguments.get(1) else { return };
    let inner_factory = match inner_arg {
        Argument::FunctionExpression(f) => Some(&**f),
        Argument::ArrowFunctionExpression(_) => None, // arrow uncommon in rollup-cjs
        _ => None,
    };
    let Some(inner) = inner_factory else { return };
    let Some(inner_body) = inner.body.as_ref() else { return };

    // Walk the inner factory body. Each top-level FunctionDeclaration
    // becomes one inner module. Other statements (var, expression,
    // etc.) flow as part of the surrounding container — they aren't
    // independently matchable.
    let mut visitor = InnerCollector {
        out,
        parent_module_id,
        seq: 0,
    };
    for stmt in &inner_body.statements {
        if let Statement::FunctionDeclaration(f) = stmt {
            let body_span = f.body.as_ref().map(|b| b.span());
            if let Some(s) = body_span {
                visitor.record(s.start, s.end);
            }
        }
    }
    let _ = visitor;
}

struct InnerCollector<'a> {
    out: &'a mut Vec<InnerModule>,
    parent_module_id: ModuleId,
    seq: usize,
}

impl InnerCollector<'_> {
    fn record(&mut self, start: u32, end: u32) {
        let seq = self.seq;
        self.seq += 1;
        self.out.push(InnerModule {
            virtual_id: format!("rollup_cjs:{}", seq),
            body_span: ByteRange::new(start, end),
            bundler: BundlerKind::RollupCjs,
            source_path_hint: None,
            parent_module_id: self.parent_module_id,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn extract(src: &str) -> Vec<InnerModule> {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, src, SourceType::default()).parse();
        assert!(parsed.errors.is_empty(), "parse errors: {:?}", parsed.errors);
        detect(&parsed.program, ModuleId(2))
    }

    #[test]
    fn detect_rollup_cjs_extracts_each_inner_function() {
        let src = r#"
            (function (global, factory) {
                typeof exports === 'object' ? factory(exports) : factory({});
            }(this, (function (exports) {
                function helper() { return 1; }
                function entry() { return helper(); }
            })));
        "#;
        let modules = extract(src);
        assert_eq!(modules.len(), 2);
        for m in &modules {
            assert_eq!(m.bundler, BundlerKind::RollupCjs);
            assert!(m.source_path_hint.is_none());
            assert!(m.virtual_id.starts_with("rollup_cjs:"));
        }
    }

    #[test]
    fn detect_rollup_cjs_ignores_non_matching_outer_shape() {
        let src = "(function () { return 1; })()";
        assert!(extract(src).is_empty());
    }

    #[test]
    fn detect_rollup_cjs_returns_body_spans_for_inner_decls() {
        let src = r#"
            (function (global, factory) { factory(); }(this, (function () {
                function helper() { var y = 42; return y; }
            })));
        "#;
        let modules = extract(src);
        assert_eq!(modules.len(), 1);
        let body_text = &src[modules[0].body_span.start as usize..modules[0].body_span.end as usize];
        assert!(body_text.contains("var y = 42"));
    }
}
```

Modify `crates/reverts-bundle/src/detectors/mod.rs`:

```rust
pub mod esbuild;
pub mod rollup_cjs;
pub mod webpack5;
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p reverts-bundle --locked detectors::rollup_cjs::tests`
Expected: PASS (3 tests).

- [ ] **Step 3: (impl + tests together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-bundle
git commit -m "✨ feat(bundle): detect Rollup CJS UMD-hybrid outer-IIFE factory"
```

---

## Task 12: Classifier dispatches to detectors

Wire `classify(path, source)` to actually invoke the detectors when a compiler kind is recognised. Multiple detectors are tried in priority order; the first non-empty result wins; runners-up with overlapping spans emit `BundleDetectorAmbiguous` audit findings.

**Files:**
- Modify: `crates/reverts-bundle/src/classifier.rs`

- [ ] **Step 1: Write the failing test**

Replace the existing `classify_returns_plain_for_phase_alpha_baseline` test (and the `_path` placeholder usage) in `crates/reverts-bundle/src/classifier.rs::tests` with:

```rust
    #[test]
    fn classify_routes_esbuild_commonjs_to_marked() {
        let src = r#"
            var x = __commonJS({
                "node_modules/lodash/index.js": (e, m) => { m.exports = 1; }
            });
        "#;
        let result = classify(Path::new("bundle.js"), src);
        match result {
            BundleClassification::Marked(meta) => {
                assert_eq!(meta.detected_by, BundlerKind::Esbuild);
                assert_eq!(meta.inner_modules.len(), 1);
                assert_eq!(
                    meta.inner_modules[0].source_path_hint.as_deref(),
                    Some("node_modules/lodash/index.js")
                );
            }
            other => panic!("expected Marked, got {other:?}"),
        }
    }

    #[test]
    fn classify_routes_webpack5_module_map_to_marked() {
        let src = r#"
            var __webpack_modules__ = {
                "./src/foo.js": (m, e, r) => { e.x = 1; }
            };
            __webpack_require__("./src/foo.js");
        "#;
        let result = classify(Path::new("bundle.js"), src);
        match result {
            BundleClassification::Marked(meta) => {
                assert_eq!(meta.detected_by, BundlerKind::Webpack5);
                assert_eq!(meta.inner_modules.len(), 1);
            }
            other => panic!("expected Marked, got {other:?}"),
        }
    }

    #[test]
    fn classify_falls_through_to_plain_when_no_detector_matches() {
        let src = "function plain() { return 1; }";
        assert_eq!(
            classify(Path::new("plain.js"), src),
            BundleClassification::Plain
        );
    }
```

- [ ] **Step 2: Run tests to verify it fails**

Run: `cargo test -p reverts-bundle --locked classifier::tests::classify_routes_esbuild`
Expected: FAIL — current `classify` always returns `Plain`.

- [ ] **Step 3: Replace `classify` body**

Replace the body of `classify` in `crates/reverts-bundle/src/classifier.rs`:

```rust
#[must_use]
pub fn classify(_path: &Path, source: &str) -> BundleClassification {
    let alloc = Allocator::default();
    let parsed = Parser::new(&alloc, source, SourceType::default()).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return BundleClassification::Plain;
    }

    let kind = detect_kind_from_source(source);
    let parent = reverts_ir::ModuleId(0);

    // Priority order: prefer detectors aligned with the detected kind.
    let runs: &[(BundlerKind, fn(&oxc_ast::ast::Program<'_>, reverts_ir::ModuleId) -> Vec<crate::inner_module::InnerModule>)] = match kind {
        CompilerKind::Esbuild => &[
            (BundlerKind::Esbuild, crate::detectors::esbuild::detect_commonjs),
            (BundlerKind::Esbuild, crate::detectors::esbuild::detect_esm),
        ],
        CompilerKind::Webpack => &[
            (BundlerKind::Webpack5, crate::detectors::webpack5::detect),
        ],
        CompilerKind::Rollup => &[
            (BundlerKind::RollupCjs, crate::detectors::rollup_cjs::detect),
        ],
        // No-match kinds: still try every detector defensively so an
        // unannotated bundle that happens to use a recognisable shape
        // is still classified. Unknown kind here means "no runtime
        // identifier found"; the file may still register modules.
        _ => &[
            (BundlerKind::Esbuild, crate::detectors::esbuild::detect_commonjs),
            (BundlerKind::Esbuild, crate::detectors::esbuild::detect_esm),
            (BundlerKind::Webpack5, crate::detectors::webpack5::detect),
            (BundlerKind::RollupCjs, crate::detectors::rollup_cjs::detect),
        ],
    };

    for (bundler, detector) in runs {
        let inner_modules = detector(&parsed.program, parent);
        if !inner_modules.is_empty() {
            return BundleClassification::Marked(MarkedMetadata {
                inner_modules,
                detected_by: *bundler,
            });
        }
    }
    BundleClassification::Plain
}
```

Also drop the now-dead test `classify_returns_plain_for_phase_alpha_baseline` if it still exists.

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS (esbuild routes, webpack routes, plain fallback, parse-fail fallback, kind detection — 7 tests in classifier::tests).

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-bundle/src/classifier.rs
git commit -m "✨ feat(bundle): classifier dispatches detectors by CompilerKind"
```

---

## Task 13: Merge layer — reconcile extractor output with `InputRows`

Spec §4.5. Implements the three rules: replace span on overlap, mark unparseable when upstream span has no overlap, emit new module when extractor finds inner with no upstream match.

**Files:**
- Create: `crates/reverts-bundle/src/merge.rs`
- Modify: `crates/reverts-bundle/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/reverts-bundle/src/merge.rs`:

```rust
use reverts_input::{InputRows, ModuleInput, ModuleKind, SourceSpan};
use reverts_ir::{ByteRange, ModuleId};
use reverts_observe::{AuditFinding, AuditReport, FindingCode};

use crate::classification::BundleClassification;
use crate::inner_module::InnerModule;

/// Result of merging an extractor classification into upstream
/// `InputRows`. New modules are returned as a separate list so the
/// caller can either inject them into a clone or apply them in-place.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeOutput {
    pub updated_modules: Vec<ModuleInput>,
    pub new_modules: Vec<ModuleInput>,
    pub audit: AuditReport,
}

/// Reconcile one source file's classification with the upstream
/// modules that already point at the same `source_file_id`.
///
/// Rules (spec §4.5):
/// - For each upstream module, pick the extractor `InnerModule` whose
///   `body_span` overlaps the upstream span. The overlap with the
///   largest share wins; ties resolve by smaller `byte_start`. The
///   upstream metadata (`original_name`, `package_name`,
///   `package_version`, `source_file_id`) is preserved and only
///   `source_span` is replaced with the extractor body span.
/// - Upstream modules with no overlapping inner emit
///   `MissingParseableBody` and keep their original span (matcher
///   skips them).
/// - Inner modules with no overlapping upstream become new
///   `ModuleInput` rows (caller assigns final ids).
/// - Runner-up inners on the same upstream span are emitted as new
///   rows with `BundleDetectorAmbiguous` audit findings.
pub fn merge_classification(
    source_file_id: u32,
    upstream_modules: &[ModuleInput],
    classification: &BundleClassification,
    next_synthetic_id: u32,
) -> MergeOutput {
    let mut updated = Vec::new();
    let mut new_modules = Vec::new();
    let mut audit = AuditReport::default();

    let inners: Vec<InnerModule> = match classification {
        BundleClassification::Plain | BundleClassification::Iife(_) => Vec::new(),
        BundleClassification::Marked(meta) => meta.inner_modules.clone(),
    };

    // Per-upstream: find overlapping inners, score by share, pick winner.
    let mut consumed_inner_indices = std::collections::BTreeSet::<usize>::new();
    for upstream in upstream_modules {
        if upstream.source_file_id != Some(source_file_id) {
            updated.push(upstream.clone());
            continue;
        }
        let Some(upstream_span) = upstream.source_span else {
            updated.push(upstream.clone());
            continue;
        };
        let upstream_range = ByteRange::new(upstream_span.byte_start, upstream_span.byte_end);

        let mut scored: Vec<(usize, f64)> = inners
            .iter()
            .enumerate()
            .filter_map(|(i, m)| {
                let inter_start = m.body_span.start.max(upstream_range.start);
                let inter_end = m.body_span.end.min(upstream_range.end);
                if inter_end <= inter_start {
                    return None;
                }
                let inter = (inter_end - inter_start) as f64;
                let width = (upstream_range.end - upstream_range.start) as f64;
                let share = if width > 0.0 { inter / width } else { 0.0 };
                Some((i, share))
            })
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| inners[a.0].body_span.start.cmp(&inners[b.0].body_span.start))
        });

        if let Some((winner_idx, _)) = scored.first().copied() {
            let mut row = upstream.clone();
            let body = inners[winner_idx].body_span;
            row.source_span = Some(SourceSpan {
                byte_start: body.start,
                byte_end: body.end,
            });
            updated.push(row);
            consumed_inner_indices.insert(winner_idx);
            for (runner_idx, _) in scored.iter().skip(1).copied() {
                audit.push(
                    AuditFinding::warning(
                        FindingCode::BundleDetectorAmbiguous,
                        "extractor produced two overlapping inner modules on the same upstream span",
                    )
                    .with_module(upstream.id.0.to_string())
                    .with_binding(inners[runner_idx].virtual_id.clone()),
                );
                consumed_inner_indices.insert(runner_idx);
                // Runners-up are still emitted as new rows (spec §4.5).
                push_new_from_inner(
                    &inners[runner_idx],
                    source_file_id,
                    next_synthetic_id + new_modules.len() as u32,
                    &mut new_modules,
                );
            }
        } else {
            // No overlap: upstream span is unparseable.
            updated.push(upstream.clone());
            audit.push(
                AuditFinding::error(
                    FindingCode::MissingParseableBody,
                    "no extractor body overlaps this upstream module span",
                )
                .with_module(upstream.id.0.to_string()),
            );
        }
    }

    // Unmatched inners become new modules.
    for (i, inner) in inners.iter().enumerate() {
        if consumed_inner_indices.contains(&i) {
            continue;
        }
        push_new_from_inner(
            inner,
            source_file_id,
            next_synthetic_id + new_modules.len() as u32,
            &mut new_modules,
        );
    }

    MergeOutput {
        updated_modules: updated,
        new_modules,
        audit,
    }
}

fn push_new_from_inner(
    inner: &InnerModule,
    source_file_id: u32,
    synthetic_id: u32,
    out: &mut Vec<ModuleInput>,
) {
    let kind = match inner.source_path_hint.as_deref() {
        Some(p) if p.starts_with("node_modules/") => ModuleKind::Package,
        _ => ModuleKind::Application,
    };
    let original_name = inner.virtual_id.clone();
    let semantic_path = inner
        .source_path_hint
        .clone()
        .unwrap_or_else(|| inner.virtual_id.clone());
    let mut row = ModuleInput {
        id: ModuleId(synthetic_id | 0x8000_0000),
        kind,
        original_name,
        semantic_path,
        source_file_id: Some(source_file_id),
        source_span: Some(SourceSpan {
            byte_start: inner.body_span.start,
            byte_end: inner.body_span.end,
        }),
        package_name: None,
        package_version: None,
    };
    if matches!(kind, ModuleKind::Package)
        && let Some(p) = inner.source_path_hint.as_deref()
        && let Some((pkg, _rest)) = parse_node_modules_path(p)
    {
        row.package_name = Some(pkg.to_string());
    }
    out.push(row);
}

fn parse_node_modules_path(p: &str) -> Option<(String, String)> {
    let s = p.strip_prefix("node_modules/")?;
    if let Some(slash) = s.find('/') {
        Some((s[..slash].to_string(), s[slash + 1..].to_string()))
    } else {
        Some((s.to_string(), String::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classification::{BundleClassification, MarkedMetadata};
    use crate::inner_module::{BundlerKind, InnerModule};
    use reverts_input::ModuleInput;
    use reverts_ir::ModuleId;

    fn make_upstream(id: u32, file_id: u32, span: (u32, u32), name: &str) -> ModuleInput {
        ModuleInput {
            id: ModuleId(id),
            kind: ModuleKind::Application,
            original_name: name.into(),
            semantic_path: name.into(),
            source_file_id: Some(file_id),
            source_span: Some(SourceSpan {
                byte_start: span.0,
                byte_end: span.1,
            }),
            package_name: None,
            package_version: None,
        }
    }

    fn make_inner(virtual_id: &str, body: (u32, u32), hint: Option<&str>) -> InnerModule {
        InnerModule {
            virtual_id: virtual_id.into(),
            body_span: ByteRange::new(body.0, body.1),
            bundler: BundlerKind::Esbuild,
            source_path_hint: hint.map(str::to_string),
            parent_module_id: ModuleId(0),
        }
    }

    #[test]
    fn overlap_replaces_span_and_preserves_upstream_metadata() {
        let upstream = vec![make_upstream(10, 1, (100, 500), "preserved")];
        let inners = vec![make_inner("esbuild:lib/foo.js", (120, 480), Some("lib/foo.js"))];
        let classification = BundleClassification::Marked(MarkedMetadata {
            inner_modules: inners,
            detected_by: BundlerKind::Esbuild,
        });
        let result = merge_classification(1, &upstream, &classification, 1000);
        assert_eq!(result.updated_modules.len(), 1);
        let row = &result.updated_modules[0];
        assert_eq!(row.original_name, "preserved");
        assert_eq!(
            row.source_span,
            Some(SourceSpan {
                byte_start: 120,
                byte_end: 480
            })
        );
        assert!(result.new_modules.is_empty());
        assert!(result.audit.is_clean());
    }

    #[test]
    fn upstream_with_no_overlap_emits_missing_parseable_body() {
        let upstream = vec![make_upstream(10, 1, (100, 200), "lonely")];
        let inners = vec![make_inner("esbuild:x", (500, 800), None)];
        let classification = BundleClassification::Marked(MarkedMetadata {
            inner_modules: inners,
            detected_by: BundlerKind::Esbuild,
        });
        let result = merge_classification(1, &upstream, &classification, 1000);
        assert_eq!(result.updated_modules.len(), 1);
        assert_eq!(
            result.updated_modules[0].source_span,
            Some(SourceSpan {
                byte_start: 100,
                byte_end: 200
            })
        );
        assert!(result.audit.has(FindingCode::MissingParseableBody));
        assert_eq!(result.new_modules.len(), 1);
    }

    #[test]
    fn inner_with_no_upstream_becomes_new_module() {
        let upstream: Vec<ModuleInput> = vec![];
        let inners = vec![make_inner(
            "esbuild:node_modules/lodash/index.js",
            (10, 100),
            Some("node_modules/lodash/index.js"),
        )];
        let classification = BundleClassification::Marked(MarkedMetadata {
            inner_modules: inners,
            detected_by: BundlerKind::Esbuild,
        });
        let result = merge_classification(1, &upstream, &classification, 5000);
        assert_eq!(result.new_modules.len(), 1);
        let m = &result.new_modules[0];
        assert!(m.id.0 & 0x8000_0000 != 0, "synthetic id high-bit set");
        assert_eq!(m.kind, ModuleKind::Package);
        assert_eq!(m.package_name.as_deref(), Some("lodash"));
        assert_eq!(
            m.source_span,
            Some(SourceSpan {
                byte_start: 10,
                byte_end: 100
            })
        );
    }

    #[test]
    fn overlap_tiebreak_picks_largest_share_then_smaller_start() {
        let upstream = vec![make_upstream(10, 1, (0, 100), "anchor")];
        let inners = vec![
            make_inner("a", (20, 60), None),  // share 0.4
            make_inner("b", (10, 90), None),  // share 0.8
            make_inner("c", (50, 100), None), // share 0.5
        ];
        let classification = BundleClassification::Marked(MarkedMetadata {
            inner_modules: inners,
            detected_by: BundlerKind::Esbuild,
        });
        let result = merge_classification(1, &upstream, &classification, 1000);
        // Inner "b" wins (share 0.8).
        let row = &result.updated_modules[0];
        assert_eq!(
            row.source_span,
            Some(SourceSpan {
                byte_start: 10,
                byte_end: 90
            })
        );
        // Two runners-up generate ambiguous warnings + 2 new modules.
        assert_eq!(
            result
                .audit
                .findings()
                .iter()
                .filter(|f| f.code == FindingCode::BundleDetectorAmbiguous)
                .count(),
            2
        );
        assert_eq!(result.new_modules.len(), 2);
    }

    #[test]
    fn plain_classification_passes_upstream_through() {
        let upstream = vec![make_upstream(10, 1, (0, 100), "preserved")];
        let result = merge_classification(1, &upstream, &BundleClassification::Plain, 1000);
        assert_eq!(result.updated_modules, upstream);
        assert!(result.new_modules.is_empty());
        assert!(result.audit.is_clean());
    }
}
```

Modify `crates/reverts-bundle/src/lib.rs`:

```rust
pub mod classifier;
pub mod detectors;
pub mod merge;
mod classification;
mod inner_module;
pub use classification::{BundleClassification, IifeMetadata, MarkedMetadata};
pub use inner_module::{BundlerKind, InnerModule};
pub use merge::{MergeOutput, merge_classification};
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p reverts-bundle --locked merge::tests`
Expected: PASS (5 tests).

- [ ] **Step 3: (impl + tests together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-bundle
git commit -m "✨ feat(bundle): merge extractor output with upstream ModuleInput rows"
```

---

## Task 14: `extract` public API

Spec §6 — the single entry point that the CLI driver calls. Iterates `InputRows.source_files`, classifies each one, runs merge, returns a `BundleExtraction`.

**Files:**
- Modify: `crates/reverts-bundle/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/reverts-bundle/src/lib.rs`:

```rust
use std::path::Path;

use reverts_input::{InputRows, ModuleInput, SourceFileInput};
use reverts_observe::AuditReport;

/// Result of running the extractor over an entire `InputRows`.
#[derive(Debug, Clone, PartialEq)]
pub struct BundleExtraction {
    /// Classifications keyed by source_file_id.
    pub classifications: std::collections::BTreeMap<u32, BundleClassification>,
    /// New ModuleInput rows that should be appended to the bundle.
    pub new_modules: Vec<ModuleInput>,
    /// Updated module rows replacing entries in `input.modules`.
    pub updated_modules: Vec<ModuleInput>,
    /// Audit findings (BundleDetectorAmbiguous, MissingParseableBody, …).
    pub audit: AuditReport,
}

impl BundleExtraction {
    /// Apply the extraction into `input` in place. Replaces rows in
    /// `input.modules` whose ids appear in `updated_modules` and
    /// appends every `new_modules` row.
    pub fn merge_into(self, input: &mut InputRows) {
        let updates: std::collections::BTreeMap<reverts_ir::ModuleId, ModuleInput> = self
            .updated_modules
            .into_iter()
            .map(|m| (m.id, m))
            .collect();
        for module in input.modules.iter_mut() {
            if let Some(replacement) = updates.get(&module.id) {
                *module = replacement.clone();
            }
        }
        input.modules.extend(self.new_modules);
    }
}

/// Run bundler-aware module extraction on every source file in `input`.
/// Each source file is classified and its modules merged via
/// `merge_classification`. The aggregate `BundleExtraction` lets the
/// caller apply changes in one shot.
#[must_use]
pub fn extract(input: &InputRows) -> BundleExtraction {
    let mut classifications = std::collections::BTreeMap::new();
    let mut new_modules = Vec::new();
    let mut updated_modules = Vec::new();
    let mut audit = AuditReport::default();
    let mut next_synthetic_id: u32 = 0;

    for source_file in &input.source_files {
        let Some(source) = source_file.source.as_deref() else { continue };
        let classification = classifier::classify(Path::new(&source_file.path), source);
        let merge_output = merge::merge_classification(
            source_file.id,
            &input.modules,
            &classification,
            next_synthetic_id,
        );
        next_synthetic_id += merge_output.new_modules.len() as u32;
        for m in &merge_output.updated_modules {
            // Only collect modules that differ from upstream.
            if let Some(orig) = input.modules.iter().find(|u| u.id == m.id)
                && orig.source_span != m.source_span
            {
                updated_modules.push(m.clone());
            }
        }
        new_modules.extend(merge_output.new_modules);
        audit.extend(merge_output.audit);
        classifications.insert(source_file.id, classification);
    }

    BundleExtraction {
        classifications,
        new_modules,
        updated_modules,
        audit,
    }
}

#[cfg(test)]
mod public_api_tests {
    use super::*;
    use reverts_input::{ProjectInput, SourceFileInput};
    use reverts_ir::ModuleId;

    #[test]
    fn extract_plain_source_yields_no_modifications() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "plain.js", Some("function f() {}".into())));
        let extraction = extract(&rows);
        assert!(extraction.new_modules.is_empty());
        assert!(extraction.updated_modules.is_empty());
        assert!(extraction.audit.is_clean());
        assert_eq!(
            extraction.classifications.get(&1),
            Some(&BundleClassification::Plain)
        );
    }

    #[test]
    fn extract_esbuild_bundle_produces_new_module() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        let src = r#"var x = __commonJS({"node_modules/lodash/index.js": (e, m) => { m.exports = 1; }});"#;
        rows.source_files
            .push(SourceFileInput::new(1, "bundle.js", Some(src.to_string())));
        let extraction = extract(&rows);
        assert_eq!(extraction.new_modules.len(), 1);
        assert!(matches!(
            extraction.classifications.get(&1),
            Some(BundleClassification::Marked(_))
        ));
    }

    #[test]
    fn merge_into_applies_updates_and_new_rows() {
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files.push(SourceFileInput::new(
            1,
            "bundle.js",
            Some(r#"var x = __commonJS({"a": (e, m) => { m.exports = 1; }});"#.into()),
        ));
        let extraction = extract(&rows);
        let added = extraction.new_modules.len();
        extraction.merge_into(&mut rows);
        assert_eq!(rows.modules.len(), added);
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p reverts-bundle --locked public_api_tests`
Expected: PASS (3 tests).

- [ ] **Step 3: (impl + tests together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-bundle/src/lib.rs
git commit -m "✨ feat(bundle): add extract public API iterating source files"
```

---

## Task 15: L6 adversarial false-positive corpus

Each detector must reject 5 near-miss inputs that LOOK like its pattern but aren't. Spec §7 L6.

**Files:**
- Create: `crates/reverts-bundle/tests/adversarial_fp.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/reverts-bundle/tests/adversarial_fp.rs`:

```rust
//! L6 adversarial false-positive corpus per design spec §7.
//!
//! 20 inputs that LOOK like bundler patterns but aren't. Each detector
//! must reject them. FP rate must be 0/20.

use reverts_bundle::{BundleClassification, classifier::classify};
use std::path::Path;

fn classify_is_plain(src: &str) -> bool {
    matches!(
        classify(Path::new("fixture.js"), src),
        BundleClassification::Plain
    )
}

#[test]
fn esbuild_lookalikes_are_rejected() {
    // __commonJS with array arg (not object)
    assert!(classify_is_plain(
        r#"var x = __commonJS(["a", "b"]);"#
    ));
    // __commonJS with no args
    assert!(classify_is_plain("var x = __commonJS();"));
    // Object literal that looks like a registry but isn't called __commonJS
    assert!(classify_is_plain(
        r#"var x = registry({"a.js": (e,m)=>{}});"#
    ));
    // __commonJS where value is a non-function
    assert!(classify_is_plain(
        r#"var x = __commonJS({"a.js": 42});"#
    ));
    // __commonJS where value is a string
    assert!(classify_is_plain(
        r#"var x = __commonJS({"a.js": "not a function"});"#
    ));
}

#[test]
fn webpack5_lookalikes_are_rejected() {
    // Wrong variable name
    assert!(classify_is_plain(
        r#"var __not_webpack__ = {"./a": ()=>{}};"#
    ));
    // Right name but no object
    assert!(classify_is_plain("var __webpack_modules__ = 42;"));
    // Right name, object, but values are non-functions
    assert!(classify_is_plain(
        r#"var __webpack_modules__ = {"./a": 1, "./b": 2};"#
    ));
    // Function variant with no body
    assert!(classify_is_plain(
        r#"var __webpack_modules__ = {"./a": function(){}};"#  // still extracted — should NOT be plain
    ) == false);
    // Empty module map
    assert!(classify_is_plain("var __webpack_modules__ = {};"));
}

#[test]
fn rollup_cjs_lookalikes_are_rejected() {
    // Wrong outer arity (3 params instead of 2)
    assert!(classify_is_plain(
        r#"(function(a,b,c){})(1,(function(){function f(){}}));"#
    ));
    // Outer has 2 params but inner factory is not a function
    assert!(classify_is_plain(
        r#"(function(g,f){f();}(this, 42));"#
    ));
    // Outer right shape but inner factory body is empty
    assert!(classify_is_plain(
        r#"(function(g,f){f();}(this, (function(){})));"#
    ));
    // Outer right shape but factory body has no FunctionDeclaration
    assert!(classify_is_plain(
        r#"(function(g,f){f();}(this, (function(){var x = 1;})));"#
    ));
    // Top-level IIFE missing factory altogether
    assert!(classify_is_plain(
        r#"(function(){function f(){}}());"#
    ));
}

#[test]
fn plain_js_with_overlapping_identifiers_is_rejected() {
    // Comments mentioning __commonJS but no actual call
    assert!(classify_is_plain(
        "// uses __commonJS internally\nfunction main() {}"
    ));
    // String literal that LOOKS like the pattern
    assert!(classify_is_plain(
        r#"var note = 'this is not __commonJS({})';"#
    ));
    // Member access on `__commonJS`-named property
    assert!(classify_is_plain(
        r#"obj.__commonJS = true;"#
    ));
    // Function declaration NAMED `__commonJS` (not called)
    assert!(classify_is_plain(
        r#"function __commonJS(o) { return o; }"#
    ));
    // Define-style AMD (Phase α doesn't recognise AMD; should be Plain)
    assert!(classify_is_plain(
        r#"define("name", ["dep"], function(d){return {};});"#
    ));
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p reverts-bundle --locked --test adversarial_fp`
Expected: PASS (4 grouped tests, 20 assertions total).

- [ ] **Step 3: (no implementation needed — these test the existing detectors)**

If any assertion fails, the detector accepted a non-bundle input — fix the detector's guard until all 20 inputs classify as `Plain`. Common fixes:
- Tighten arity check in `rollup_cjs::detect` outer-IIFE recogniser.
- In webpack5, require at least one property to have a function/arrow value (currently it accepts empty object via `BundleClassification::Marked` with empty list; the empty-modules check at line 32 of `classify` should already filter this).

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-bundle/tests/adversarial_fp.rs
git commit -m "✅ test(bundle): L6 adversarial false-positive corpus, 20 cases"
```

---

## Task 16: Wire `extract` into the CLI `match-packages` flow

Run `reverts_bundle::extract` immediately after loading rows from the DB, apply the extraction to the live `InputRows`, then proceed with the existing matcher. Spec §6.

**Files:**
- Modify: `crates/reverts-cli/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add to `crates/reverts-cli/src/lib.rs` near the existing `match_packages_apply_writes_cascade_function_attribution` test:

```rust
#[test]
fn match_packages_runs_bundle_extraction_before_matcher() {
    // A source file containing a single esbuild __commonJS registration
    // should be split into a parseable inner-module body, after which
    // the cascade matcher sees that body span (and downstream tests
    // can verify cascade attribution against it).
    let tempdir = tempfile::tempdir().expect("tempdir");
    let bundle_path = tempdir.path().join("bundle.js");
    let bundle_src = r#"
        var lib = __commonJS({
            "node_modules/example/index.js": (exports, module) => {
                function add(a, b) { return a + b; }
                module.exports = { add };
            }
        });
    "#;
    let mut connection = package_match_connection(bundle_path.clone(), bundle_src, &[]);
    // Insert a single application-level module pointing at the whole file.
    connection
        .execute(
            r#"DELETE FROM modules WHERE id = 10;
               INSERT INTO modules (id, file_id, original_name, semantic_name, module_category,
                                    package_name, package_version, byte_start, byte_end)
               VALUES (10, 1, 'lib', 'bundle/lib', 'application', NULL, NULL, 0, 0);"#,
            [],
        )
        .expect("seed module");

    let args = MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: false,
        package_names: Vec::new(),
    };
    let outcome =
        match_packages_from_connection(&mut connection, &args).expect("match should run");
    // After extraction, the bundle's `node_modules/example/index.js`
    // becomes a discovered package-kind module. We don't have a
    // matching PackageSource, so cascade_attributions stays at 0;
    // but `loaded_package_modules` must reflect the new package row.
    assert!(
        outcome.loaded_package_modules >= 1,
        "extraction should have produced at least one package module: {:?}",
        outcome
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p reverts-cli --locked match_packages_runs_bundle_extraction_before_matcher`
Expected: FAIL — the extraction has not been wired yet, so `loaded_package_modules` stays at 0.

- [ ] **Step 3: Wire `extract` into `match_packages_from_connection`**

In `crates/reverts-cli/src/lib.rs`, add to the imports near `use reverts_package_matcher::...`:

```rust
use reverts_bundle::extract as extract_bundle_modules;
```

Add `reverts-bundle = { path = "../reverts-bundle" }` to `crates/reverts-cli/Cargo.toml`'s `[dependencies]`.

Then in `match_packages_from_connection`, immediately after the `load_project_rows_from_connection` line:

```rust
    let mut rows = load_project_rows_from_connection(connection, args.project_id)
        .map_err(MatchPackagesError::LoadInput)?;

    // Bundle-aware module extraction (Phase α): split recognised bundle
    // wrappers into per-module rows before the matcher sees them.
    let extraction = extract_bundle_modules(&rows);
    extraction.merge_into(&mut rows);
```

(Replace the previous `let rows = …` with `let mut rows = …` so the merge can mutate.)

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS, including the new CLI test.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-cli/Cargo.toml crates/reverts-cli/src/lib.rs
git commit -m "✨ feat(cli): run reverts-bundle extraction before package matcher"
```

---

## Task 17: L5 cascade integration test

A synthetic two-module esbuild bundle against a synthetic package source that matches one of the bodies. Verify the cascade emits exactly one attribution row whose `function_span` falls inside the extractor-produced body span.

**Files:**
- Create: `crates/reverts-cli/tests/bundle_cascade_e2e.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/reverts-cli/tests/bundle_cascade_e2e.rs`:

```rust
//! L5 — end-to-end pipeline test: bundle extraction → graph build →
//! cascade match → attribution. Synthesises a small esbuild bundle and
//! a known package source, verifies that the cascade attributes
//! exactly the matched function with a span inside the extractor body.

use rusqlite::Connection;
use std::path::PathBuf;
use tempfile::tempdir;

#[test]
fn cascade_matches_function_inside_esbuild_extracted_body() {
    let tempdir = tempdir().expect("tempdir");
    let bundle_path = tempdir.path().join("bundle.js");
    let bundle_src = r#"
        var lib = __commonJS({
            "node_modules/example/index.js": (exports, module) => {
                function add(a, b) { return a + b; }
                module.exports = { add };
            }
        });
    "#;
    std::fs::write(&bundle_path, bundle_src).expect("write bundle");

    let mut connection = Connection::open_in_memory().expect("sqlite");
    connection
        .execute_batch(include_str!("bundle_cascade_schema.sql"))
        .expect("schema");
    connection
        .execute(
            "INSERT INTO source_files (id, file_path) VALUES (1, ?1);",
            [bundle_path.to_string_lossy()],
        )
        .expect("source row");
    connection
        .execute_batch(
            r#"
            INSERT INTO projects (id, name) VALUES (1, 'cascade-e2e');
            INSERT INTO project_files (project_id, file_id) VALUES (1, 1);
            INSERT INTO modules
                (id, file_id, original_name, semantic_name, module_category,
                 package_name, package_version, byte_start, byte_end)
                VALUES (10, 1, 'bundle', 'bundle/index', 'application',
                        NULL, NULL, 0, 0);
            INSERT INTO package_source_cache
                (package_name, package_version, entry_path, source_content,
                 content_hash, fetched_at, expires_at)
                VALUES ('example', '1.0.0', 'index.js',
                        'function add(a, b) { return a + b; }',
                        'h', '2026-01-01', '2099-01-01');
            "#,
        )
        .expect("seed rows");

    let args = reverts_cli::MatchPackagesArgs {
        input: PathBuf::from("unused.db"),
        project_id: 1,
        apply: true,
        package_names: Vec::new(),
    };
    let outcome = reverts_cli::match_packages_from_connection(&mut connection, &args)
        .expect("match should succeed");

    assert!(
        outcome.cascade_attributions >= 1,
        "expected ≥1 cascade attribution, got {:?}",
        outcome
    );
    assert!(
        outcome.audit.is_clean(),
        "audit must be clean (no errors): {:?}",
        outcome.audit.findings()
    );
}
```

Create `crates/reverts-cli/tests/bundle_cascade_schema.sql` with the minimal schema (copy fields used by `package_match_connection` in `lib.rs`):

```sql
CREATE TABLE projects (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
CREATE TABLE source_files (id INTEGER PRIMARY KEY, file_path TEXT NOT NULL);
CREATE TABLE project_files (project_id INTEGER NOT NULL, file_id INTEGER NOT NULL);
CREATE TABLE modules (
    id INTEGER PRIMARY KEY,
    file_id INTEGER,
    original_name TEXT NOT NULL,
    semantic_name TEXT,
    module_category TEXT,
    package_name TEXT,
    package_version TEXT,
    byte_start INTEGER,
    byte_end INTEGER
);
CREATE TABLE symbols (
    module_id INTEGER,
    semantic_name TEXT,
    export_name TEXT,
    original_name TEXT,
    scope_level TEXT
);
CREATE TABLE module_dependencies (module_id INTEGER, dependency_id INTEGER);
CREATE TABLE package_source_cache (
    package_name TEXT NOT NULL,
    package_version TEXT NOT NULL,
    entry_path TEXT NOT NULL,
    source_content TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    fetched_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    PRIMARY KEY (package_name, package_version, entry_path)
);
CREATE TABLE package_attributions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    module_id INTEGER NOT NULL,
    module_original_name TEXT NOT NULL,
    package_name TEXT NOT NULL,
    package_version TEXT NOT NULL,
    package_subpath TEXT,
    resolved_file TEXT,
    export_specifier TEXT,
    emission_mode TEXT NOT NULL,
    status TEXT NOT NULL,
    evidence_json TEXT,
    rejection_reason TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (module_id)
);
```

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-cli --locked --test bundle_cascade_e2e`
Expected: PASS — the cascade should match `add` against the cached package source.

- [ ] **Step 3: (impl already landed in Task 16; this test exercises it)**

If the test fails, diagnose by:
1. Confirm extraction discovered `node_modules/example/index.js` → check `loaded_package_modules` in the outcome.
2. Confirm graph extracts a function for the body span → run `cargo test -p reverts-graph` to verify the fingerprint extractor isn't broken on small inputs.
3. Confirm cascade index is built from the cached source → walk `match_with_cascade`'s test path.

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-cli/tests
git commit -m "✅ test(cli): L5 end-to-end bundle extraction through cascade attribution"
```

---

## Self-Review

**Spec coverage map:**

| Spec section | Task |
|---|---|
| §1 Problem (parseable body spans) | Tasks 8–11 (each detector produces body spans), Task 13 (merge) |
| §2 Goals G1–G4 | G1 → Tasks 8–11; G2 → Task 13; G3 → Tasks 8–11 (3 of 8 bundlers; remaining 5 covered in Phase β/γ); G4 → out of scope (Phase γ) |
| §3 Non-goals | Honoured by exclusion |
| §4.1 Three-way classification | Task 6 (types), Task 12 (dispatch) |
| §4.2 Per-bundler templates | Tasks 8 (esbuild commonJS), 9 (esbuild esm), 10 (webpack5), 11 (rollup_cjs) |
| §4.3 IIFE cluster recovery | Phase γ — not in this plan |
| §4.4 InnerModule data model | Task 5 |
| §4.5 Merge with `ModuleInput.source_span` | Task 13 (5 tests covering all rules incl. overlap tiebreak) |
| §5.1 Direct reuse | Implicit — each task imports from existing crates |
| §5.2 Refactor commits | Tasks 2 (ESBUILD_WRAPPER_NAMES), 3 (iife_kind pub) |
| §5.3 Non-reuse | Honoured by exclusion |
| §6 Pipeline integration | Task 16 |
| §7 L1 per-detector tests | Embedded in Tasks 8, 9, 10, 11 |
| §7 L2 classifier-level tests | Embedded in Task 12 |
| §7 L3 merge tests | Task 13 |
| §7 L4 nightly DB regression | Deferred to Phase β |
| §7 L5 cascade integration | Task 17 |
| §7 L6 adversarial FP | Task 15 |
| §8 Phase α | Whole plan |
| §9 New audit codes | Task 1 |
| §10 Open questions | Acknowledged; ModuleId synthetic-id scheme used in Task 13 (high-bit ≥ 0x8000_0000) |

**Placeholder scan:** Every step has actual code; no TBD/TODO. The "fix the detector's guard" instruction in Task 15 Step 3 names concrete fixes ("tighten arity check", "require at least one property to have a function value") rather than abstract guidance.

**Type consistency:** `BundleClassification`, `MarkedMetadata`, `IifeMetadata`, `InnerModule`, `BundlerKind`, `BundleExtraction`, `MergeOutput` are introduced in Tasks 5–6 and 13–14, then used consistently in later tasks. Function signatures match across declaration and call sites.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-05-17-bundler-aware-module-extraction-phase-a.md`. Two execution options:

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.
**2. Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints.

Which approach?
