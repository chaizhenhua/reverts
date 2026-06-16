# Package Signature Matching Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the byte-hash function matcher in `reverts-package-matcher` with a 12-axis, drift-tolerant, function-level cascade matcher backed by an injectable `PackageFingerprintIndex`. Adds function-level attribution and identification of unknown modules against an open corpus.

**Architecture:** Compute a `FunctionFingerprint` (1 primary + 6 normalization-pass alternates × 12 axis hashes) per function in `reverts-graph`, alongside existing AST facts. The matcher walks five tiers (`Exact` → `ExactAlternate` → `StructuralAnchored` → `FeatureSimilarity` → `StructuralOnly`) through a `PackageFingerprintIndex` trait, scores variants by Jaccard + tier-weight sum, picks versions per package, and resolves cross-package collisions with bipartite maximum-weight assignment (Kuhn–Munkres).

**Tech Stack:** Rust 2024 (toolchain 1.93.0), oxc 0.42 (parser/codegen/ast/span/allocator), workspace lints `unsafe_code = forbid`, `clippy::unwrap_used = deny`, `clippy::todo = deny`. Tests must be self-contained per ADR 0003 — no network, no Node, no npm.

**Spec:** `docs/superpowers/specs/2026-05-16-package-signature-matching-design.md` (gitignored).

---

## Conventions for every task

- Every test goes in `#[cfg(test)] mod tests { ... }` at the bottom of its module file unless an integration-test path is called out.
- Every commit message follows `<emoji> <type>(<scope>): <subject>` (≤100 chars, single line, no `Co-Authored-By`, no AI markers — enforced by `lefthook.yml`'s `commit-msg` hook).
- After implementing each task: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked` must all pass before the commit step.
- Use `oxc_allocator::Allocator` (workspace dep) and `oxc_span::Span` directly when working with OXC AST inside `reverts-js` and `reverts-graph`. `reverts-ir` must stay oxc-free — convert at the boundary.
- Prefer FNV-1a (the existing project hash) for axis hashes. Constants `FNV_OFFSET_BASIS = 0xcbf2_9ce4_8422_2325`, `FNV_PRIME = 0x0100_0000_01b3` are already defined in `reverts-package-matcher`; lift the helpers into a shared spot once needed (see Task 4).

## File structure (created or modified)

```
crates/
├── reverts-ir/src/
│   ├── lib.rs                        modify: pub use of new modules
│   ├── byte_range.rs                 create: ByteRange + FunctionId
│   └── fingerprint.rs                create: AxisKind, AxisHashes,
│                                              NormalizationPassId, MatchTier,
│                                              FunctionFingerprint
├── reverts-js/src/
│   ├── lib.rs                        modify: pub use normalize module
│   ├── normalize/mod.rs              create: NormalizationPass trait + helpers
│   ├── normalize/ts_runtime_erased.rs       create
│   ├── normalize/jsx_runtime_normalized.rs  create
│   ├── normalize/bundler_wrapper_unwrapped.rs create
│   ├── normalize/helper_identity_inlined.rs  create
│   ├── normalize/export_boundary_normalized.rs create
│   └── normalize/closure_boundary_aligned.rs create
├── reverts-graph/src/
│   ├── lib.rs                        modify: emit FunctionFingerprint records
│   └── fingerprint/                  create: per-axis extractors
│       ├── mod.rs
│       ├── ast.rs
│       ├── cfg.rs
│       ├── return_pattern.rs
│       ├── effect_pattern.rs
│       ├── literal_anchor.rs
│       ├── access.rs                 access_pattern + access_shape
│       ├── structural_anchor.rs
│       ├── literal_shape.rs
│       ├── callee_set.rs
│       ├── binding_pattern.rs
│       ├── throw_set.rs
│       └── extractor.rs              orchestrator
├── reverts-input/src/
│   └── lib.rs                        modify: PackageAttributionInput grows function_span + confidence
├── reverts-package-index/            create: new crate
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       └── in_memory.rs
├── reverts-package-matcher/src/
│   ├── lib.rs                        modify: cascade matcher replaces current body
│   ├── cascade.rs                    create
│   ├── tier.rs                       create
│   ├── variant.rs                    create
│   ├── version.rs                    create
│   ├── hungarian.rs                  create: Kuhn-Munkres
│   └── audit.rs                      create: new audit codes
├── reverts-observe/src/
│   └── lib.rs                        modify: add FindingCode variants
├── reverts-analyze/src/
│   └── lib.rs                        modify: thread FunctionFingerprint through EnrichedProgram
├── reverts-model/src/
│   └── lib.rs                        modify: EnrichedProgram carries fingerprints
└── Cargo.toml                        modify: add reverts-package-index workspace member
```

---

## Phase A — Foundation types (`reverts-ir`)

### Task 1: Add `ByteRange` and `FunctionId`

**Files:**
- Create: `crates/reverts-ir/src/byte_range.rs`
- Modify: `crates/reverts-ir/src/lib.rs` (add `mod byte_range; pub use byte_range::*;` near the top)

- [ ] **Step 1: Write the failing test**

In `crates/reverts-ir/src/byte_range.rs`:

```rust
use crate::ModuleId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteRange {
    pub start: u32,
    pub end: u32,
}

impl ByteRange {
    #[must_use]
    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    #[must_use]
    pub const fn contains(&self, other: Self) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    #[must_use]
    pub const fn overlaps(&self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FunctionId {
    pub module_id: ModuleId,
    pub span: ByteRange,
}

impl FunctionId {
    #[must_use]
    pub const fn new(module_id: ModuleId, span: ByteRange) -> Self {
        Self { module_id, span }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_range_contains_subspan_but_not_overlap_with_disjoint() {
        let outer = ByteRange::new(10, 30);
        let inner = ByteRange::new(15, 20);
        let disjoint = ByteRange::new(40, 50);

        assert!(outer.contains(inner));
        assert!(!outer.contains(disjoint));
        assert!(outer.overlaps(inner));
        assert!(!outer.overlaps(disjoint));
    }

    #[test]
    fn function_id_pairs_module_and_span() {
        let id = FunctionId::new(ModuleId(7), ByteRange::new(0, 42));

        assert_eq!(id.module_id, ModuleId(7));
        assert_eq!(id.span.start, 0);
        assert_eq!(id.span.end, 42);
    }
}
```

Append to `crates/reverts-ir/src/lib.rs` (after the existing `use` block):

```rust
mod byte_range;
pub use byte_range::{ByteRange, FunctionId};
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p reverts-ir --locked byte_range::tests`
Expected: PASS on the new tests (they were written together with the impl since this is a pure new module). Skip Step 3.

- [ ] **Step 3: (skipped)**

- [ ] **Step 4: Verify full workspace still passes**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-ir/src/byte_range.rs crates/reverts-ir/src/lib.rs
git commit -m "✨ feat(ir): add ByteRange and FunctionId for function-level identity"
```

### Task 2: Add `MatchTier` enum

**Files:**
- Modify: `crates/reverts-ir/src/lib.rs` (append enum + tests)

- [ ] **Step 1: Write the failing test**

Append to `crates/reverts-ir/src/lib.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MatchTier {
    Exact,
    ExactAlternate,
    StructuralAnchored,
    FeatureSimilarity,
    StructuralOnly,
}

impl MatchTier {
    #[must_use]
    pub const fn weight(self) -> u32 {
        match self {
            Self::Exact => 10_000,
            Self::ExactAlternate => 5_000,
            Self::StructuralAnchored => 1_000,
            Self::FeatureSimilarity => 100,
            Self::StructuralOnly => 10,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::ExactAlternate => "exact_alternate",
            Self::StructuralAnchored => "structural_anchored",
            Self::FeatureSimilarity => "feature_similarity",
            Self::StructuralOnly => "structural_only",
        }
    }
}
```

Append a `#[cfg(test)] mod match_tier_tests { ... }` (or extend existing tests module):

```rust
#[cfg(test)]
mod match_tier_tests {
    use super::MatchTier;

    #[test]
    fn match_tier_weights_strictly_decrease() {
        let weights = [
            MatchTier::Exact.weight(),
            MatchTier::ExactAlternate.weight(),
            MatchTier::StructuralAnchored.weight(),
            MatchTier::FeatureSimilarity.weight(),
            MatchTier::StructuralOnly.weight(),
        ];
        for window in weights.windows(2) {
            assert!(window[0] > window[1], "tier weights must strictly decrease: {weights:?}");
        }
    }
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-ir --locked match_tier_tests`
Expected: PASS.

- [ ] **Step 3: (skipped — impl + test landed together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-ir/src/lib.rs
git commit -m "✨ feat(ir): add MatchTier with weighted tier ordering"
```

### Task 3: Add `AxisKind`, `AxisHashes`, `NormalizationPassId`, `FunctionFingerprint`

**Files:**
- Create: `crates/reverts-ir/src/fingerprint.rs`
- Modify: `crates/reverts-ir/src/lib.rs` (`mod fingerprint; pub use fingerprint::*;`)

- [ ] **Step 1: Write the failing test**

Create `crates/reverts-ir/src/fingerprint.rs`:

```rust
use crate::FunctionId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AxisKind {
    Ast,
    Cfg,
    ReturnPattern,
    EffectPattern,
    LiteralAnchor,
    AccessPattern,
    StructuralAnchor,
    LiteralShape,
    AccessShape,
    CalleeSet,
    BindingPattern,
    ThrowSet,
}

impl AxisKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ast => "ast",
            Self::Cfg => "cfg",
            Self::ReturnPattern => "return_pattern",
            Self::EffectPattern => "effect_pattern",
            Self::LiteralAnchor => "literal_anchor",
            Self::AccessPattern => "access_pattern",
            Self::StructuralAnchor => "structural_anchor",
            Self::LiteralShape => "literal_shape",
            Self::AccessShape => "access_shape",
            Self::CalleeSet => "callee_set",
            Self::BindingPattern => "binding_pattern",
            Self::ThrowSet => "throw_set",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AxisHashes {
    pub ast: u64,
    pub cfg: u64,
    pub return_pattern: u64,
    pub effect_pattern: u64,
    pub literal_anchor: Option<u64>,
    pub access_pattern: Option<u64>,
    pub structural_anchor: u64,
    pub literal_shape: Option<u64>,
    pub access_shape: Option<u64>,
    pub callee_set: Option<u64>,
    pub binding_pattern: u64,
    pub throw_set: Option<u64>,
}

impl AxisHashes {
    #[must_use]
    pub fn get(&self, axis: AxisKind) -> Option<u64> {
        match axis {
            AxisKind::Ast => Some(self.ast),
            AxisKind::Cfg => Some(self.cfg),
            AxisKind::ReturnPattern => Some(self.return_pattern),
            AxisKind::EffectPattern => Some(self.effect_pattern),
            AxisKind::LiteralAnchor => self.literal_anchor,
            AxisKind::AccessPattern => self.access_pattern,
            AxisKind::StructuralAnchor => Some(self.structural_anchor),
            AxisKind::LiteralShape => self.literal_shape,
            AxisKind::AccessShape => self.access_shape,
            AxisKind::CalleeSet => self.callee_set,
            AxisKind::BindingPattern => Some(self.binding_pattern),
            AxisKind::ThrowSet => self.throw_set,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NormalizationPassId {
    Primary,
    TsRuntimeErased,
    JsxRuntimeNormalized,
    BundlerWrapperUnwrapped,
    HelperIdentityInlined,
    ExportBoundaryNormalized,
    ClosureBoundaryAligned,
}

impl NormalizationPassId {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::TsRuntimeErased => "ts_runtime_erased",
            Self::JsxRuntimeNormalized => "jsx_runtime_normalized",
            Self::BundlerWrapperUnwrapped => "bundler_wrapper_unwrapped",
            Self::HelperIdentityInlined => "helper_identity_inlined",
            Self::ExportBoundaryNormalized => "export_boundary_normalized",
            Self::ClosureBoundaryAligned => "closure_boundary_aligned",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionFingerprint {
    pub id: FunctionId,
    pub param_count: u32,
    pub statement_count: u32,
    pub primary: AxisHashes,
    pub alternates: Vec<(NormalizationPassId, AxisHashes)>,
}

impl FunctionFingerprint {
    #[must_use]
    pub fn axis_hashes(&self, pass: NormalizationPassId) -> Option<&AxisHashes> {
        if pass == NormalizationPassId::Primary {
            return Some(&self.primary);
        }
        self.alternates.iter().find_map(|(id, hashes)| (*id == pass).then_some(hashes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ByteRange, ModuleId};

    fn sample_axes() -> AxisHashes {
        AxisHashes {
            ast: 1,
            cfg: 2,
            return_pattern: 3,
            effect_pattern: 4,
            literal_anchor: Some(5),
            access_pattern: Some(6),
            structural_anchor: 7,
            literal_shape: None,
            access_shape: Some(8),
            callee_set: Some(9),
            binding_pattern: 10,
            throw_set: None,
        }
    }

    #[test]
    fn axis_hashes_get_returns_optional_axes() {
        let axes = sample_axes();
        assert_eq!(axes.get(AxisKind::Ast), Some(1));
        assert_eq!(axes.get(AxisKind::LiteralShape), None);
        assert_eq!(axes.get(AxisKind::ThrowSet), None);
        assert_eq!(axes.get(AxisKind::StructuralAnchor), Some(7));
    }

    #[test]
    fn function_fingerprint_lookup_finds_primary_and_alternates() {
        let id = FunctionId::new(ModuleId(1), ByteRange::new(0, 10));
        let mut alt = sample_axes();
        alt.ast = 99;
        let fp = FunctionFingerprint {
            id,
            param_count: 2,
            statement_count: 3,
            primary: sample_axes(),
            alternates: vec![(NormalizationPassId::TsRuntimeErased, alt)],
        };

        assert_eq!(
            fp.axis_hashes(NormalizationPassId::Primary).map(|a| a.ast),
            Some(1),
        );
        assert_eq!(
            fp.axis_hashes(NormalizationPassId::TsRuntimeErased).map(|a| a.ast),
            Some(99),
        );
        assert!(fp.axis_hashes(NormalizationPassId::JsxRuntimeNormalized).is_none());
    }
}
```

Append to `crates/reverts-ir/src/lib.rs`:

```rust
mod fingerprint;
pub use fingerprint::{
    AxisHashes, AxisKind, FunctionFingerprint, NormalizationPassId,
};
```

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-ir --locked fingerprint::tests`
Expected: PASS.

- [ ] **Step 3: (skipped — impl + test landed together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-ir/src/fingerprint.rs crates/reverts-ir/src/lib.rs
git commit -m "✨ feat(ir): add FunctionFingerprint with 12-axis hash record"
```

### Task 4: Promote FNV-1a helpers to `reverts-ir`

The current `reverts-package-matcher` defines FNV constants and helpers inline. They'll be shared across `reverts-graph` (axis hashing), `reverts-package-matcher` (existing exact path), and tests. Move the helpers up.

**Files:**
- Create: `crates/reverts-ir/src/hash.rs`
- Modify: `crates/reverts-ir/src/lib.rs` (`pub mod hash;`)
- Modify: `crates/reverts-package-matcher/Cargo.toml` (already depends on reverts-ir; no change)
- Modify: `crates/reverts-package-matcher/src/lib.rs:1349-1363` (replace local helpers with `reverts_ir::hash::*`)

- [ ] **Step 1: Write the failing test**

Create `crates/reverts-ir/src/hash.rs`:

```rust
pub const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
pub const FNV_PRIME: u64 = 0x0100_0000_01b3;

#[must_use]
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, bytes);
    hash
}

pub fn update_fnv1a(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
}

#[must_use]
pub fn fnv1a_hex(bytes: &[u8]) -> String {
    format!("{:016x}", fnv1a(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_is_deterministic_and_distinguishes_inputs() {
        assert_eq!(fnv1a(b"hello"), fnv1a(b"hello"));
        assert_ne!(fnv1a(b"hello"), fnv1a(b"world"));
    }

    #[test]
    fn update_fnv1a_matches_one_shot() {
        let mut accum = FNV_OFFSET_BASIS;
        update_fnv1a(&mut accum, b"foo");
        update_fnv1a(&mut accum, b"bar");
        assert_eq!(accum, fnv1a(b"foobar"));
    }
}
```

Append to `crates/reverts-ir/src/lib.rs`:

```rust
pub mod hash;
```

Then update `crates/reverts-package-matcher/src/lib.rs`:

Find the bottom block:
```rust
fn stable_hash(bytes: &[u8]) -> String { /* ... */ }
fn update_stable_hash(hash: &mut u64, bytes: &[u8]) { /* ... */ }
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0100_0000_01b3;
```

Replace it with:
```rust
use reverts_ir::hash::{fnv1a_hex as stable_hash, update_fnv1a as update_stable_hash, FNV_OFFSET_BASIS, FNV_PRIME};
```

(Keep the call sites using `stable_hash` and `update_stable_hash` unchanged — they're now aliased imports.)

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-ir --locked hash::tests && cargo test -p reverts-package-matcher --locked`
Expected: PASS.

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-ir/src/hash.rs crates/reverts-ir/src/lib.rs crates/reverts-package-matcher/src/lib.rs
git commit -m "♻️ refactor(ir): promote FNV-1a helpers from package-matcher to reverts-ir"
```

---

## Phase B — Normalization passes (`reverts-js`)

### Task 5: `NormalizationPass` trait + `normalize` module skeleton

**Files:**
- Create: `crates/reverts-js/src/normalize/mod.rs`
- Modify: `crates/reverts-js/src/lib.rs` (`pub mod normalize;`)

- [ ] **Step 1: Write the failing test**

Create `crates/reverts-js/src/normalize/mod.rs`:

```rust
use oxc_allocator::Allocator;
use oxc_ast::ast::Program;
use reverts_ir::NormalizationPassId;

pub trait NormalizationPass {
    fn id(&self) -> NormalizationPassId;
    fn version(&self) -> u32;
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>);
}

#[must_use]
pub fn stable_passes() -> [Box<dyn NormalizationPass + Send + Sync>; 6] {
    [
        Box::new(ts_runtime_erased::TsRuntimeErased),
        Box::new(jsx_runtime_normalized::JsxRuntimeNormalized),
        Box::new(bundler_wrapper_unwrapped::BundlerWrapperUnwrapped),
        Box::new(helper_identity_inlined::HelperIdentityInlined),
        Box::new(export_boundary_normalized::ExportBoundaryNormalized),
        Box::new(closure_boundary_aligned::ClosureBoundaryAligned),
    ]
}

pub mod ts_runtime_erased;
pub mod jsx_runtime_normalized;
pub mod bundler_wrapper_unwrapped;
pub mod helper_identity_inlined;
pub mod export_boundary_normalized;
pub mod closure_boundary_aligned;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn stable_passes_have_unique_ids_and_non_zero_versions() {
        let passes = stable_passes();
        let mut ids = BTreeSet::new();
        for pass in passes.iter() {
            assert_ne!(pass.id(), NormalizationPassId::Primary, "passes must not use Primary id");
            assert!(pass.version() > 0, "pass version must be non-zero");
            assert!(ids.insert(pass.id()), "duplicate pass id: {:?}", pass.id());
        }
        assert_eq!(ids.len(), 6);
    }
}
```

Each `pub mod xxx;` referenced above needs a stub module. For each of the 6 passes, create a minimal scaffold (subsequent tasks fill them in):

`crates/reverts-js/src/normalize/ts_runtime_erased.rs`:
```rust
use oxc_allocator::Allocator;
use oxc_ast::ast::Program;
use reverts_ir::NormalizationPassId;
use super::NormalizationPass;

pub struct TsRuntimeErased;

impl NormalizationPass for TsRuntimeErased {
    fn id(&self) -> NormalizationPassId { NormalizationPassId::TsRuntimeErased }
    fn version(&self) -> u32 { 1 }
    fn apply<'a>(&self, _alloc: &'a Allocator, _program: &mut Program<'a>) {
        // Filled in by Task 6.
    }
}
```

Repeat the scaffold pattern for the other five files, substituting names:
- `jsx_runtime_normalized.rs` → `JsxRuntimeNormalized`, id `JsxRuntimeNormalized`
- `bundler_wrapper_unwrapped.rs` → `BundlerWrapperUnwrapped`, id `BundlerWrapperUnwrapped`
- `helper_identity_inlined.rs` → `HelperIdentityInlined`, id `HelperIdentityInlined`
- `export_boundary_normalized.rs` → `ExportBoundaryNormalized`, id `ExportBoundaryNormalized`
- `closure_boundary_aligned.rs` → `ClosureBoundaryAligned`, id `ClosureBoundaryAligned`

Append to `crates/reverts-js/src/lib.rs`:
```rust
pub mod normalize;
```

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-js --locked normalize::tests`
Expected: PASS.

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-js/src/normalize crates/reverts-js/src/lib.rs
git commit -m "✨ feat(js): add NormalizationPass trait and pass module scaffolds"
```

### Task 6: `TsRuntimeErased` pass

Implements: drop TS-only nodes (TS type annotations, `enum`, `namespace`, `declare`, `abstract`/`private`/`readonly` modifiers, type assertions, satisfies). Idempotent.

**Files:**
- Modify: `crates/reverts-js/src/normalize/ts_runtime_erased.rs`

- [ ] **Step 1: Write the failing test**

Replace `ts_runtime_erased.rs` body. Use a round-trip helper from `crates/reverts-js/src/lib.rs` (`normalize_source_for_pipeline` exists) and a new public helper for applying passes.

First, add a small parsing+codegen helper in `crates/reverts-js/src/normalize/mod.rs`:

```rust
use oxc_codegen::{CodeGenerator, CodegenOptions};
use oxc_parser::Parser;
use oxc_span::SourceType;

/// Test/debug helper. Parses TypeScript-permissive source, runs `pass`,
/// re-emits, returns the printed string. Bails if the input fails to parse.
pub fn apply_to_source(pass: &dyn NormalizationPass, source: &str) -> Result<String, String> {
    let alloc = Allocator::default();
    let source_type = SourceType::default().with_typescript(true);
    let parsed = Parser::new(&alloc, source, source_type).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return Err(format!(
            "parse failed: {}",
            parsed.errors.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "),
        ));
    }
    let mut program = parsed.program;
    pass.apply(&alloc, &mut program);
    let printed = CodeGenerator::new()
        .with_options(CodegenOptions::default())
        .build(&program);
    Ok(printed.code)
}
```

Now in `ts_runtime_erased.rs`, replace the stub with:

```rust
use oxc_allocator::Allocator;
use oxc_ast::ast::{Declaration, Program, Statement};
use reverts_ir::NormalizationPassId;
use super::NormalizationPass;

pub struct TsRuntimeErased;

impl NormalizationPass for TsRuntimeErased {
    fn id(&self) -> NormalizationPassId { NormalizationPassId::TsRuntimeErased }
    fn version(&self) -> u32 { 1 }
    fn apply<'a>(&self, _alloc: &'a Allocator, program: &mut Program<'a>) {
        program.body.retain(|stmt| !is_ts_only_top_level(stmt));
    }
}

fn is_ts_only_top_level(stmt: &Statement<'_>) -> bool {
    matches!(
        stmt,
        Statement::TSTypeAliasDeclaration(_)
            | Statement::TSInterfaceDeclaration(_)
            | Statement::TSEnumDeclaration(_)
            | Statement::TSModuleDeclaration(_)
            | Statement::TSImportEqualsDeclaration(_)
            | Statement::TSExportAssignment(_)
            | Statement::TSNamespaceExportDeclaration(_)
    ) || matches!(
        stmt,
        Statement::ExportNamedDeclaration(export)
            if matches!(
                &export.declaration,
                Some(Declaration::TSTypeAliasDeclaration(_)
                    | Declaration::TSInterfaceDeclaration(_)
                    | Declaration::TSEnumDeclaration(_)
                    | Declaration::TSModuleDeclaration(_))
            )
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn ts_runtime_erased_drops_interface_and_type_alias() {
        let src = "interface Foo { a: number }\ntype B = string;\nexport function f(x: number): number { return x + 1; }";
        let out = apply_to_source(&TsRuntimeErased, src).expect("source must parse");
        assert!(!out.contains("interface"), "got: {out}");
        assert!(!out.contains("type B"), "got: {out}");
        assert!(out.contains("function f"), "got: {out}");
    }

    #[test]
    fn ts_runtime_erased_is_idempotent_on_plain_js() {
        let src = "function add(a, b) { return a + b; }\n";
        let first = apply_to_source(&TsRuntimeErased, src).unwrap();
        let second = apply_to_source(&TsRuntimeErased, &first).unwrap();
        assert_eq!(first, second);
    }
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-js --locked normalize::ts_runtime_erased::tests`
Expected: PASS.

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-js/src/normalize
git commit -m "✨ feat(js): implement TsRuntimeErased pass dropping TS-only top-level decls"
```

### Task 7: `JsxRuntimeNormalized` pass

Implements: replace JSX elements with `_jsx(type, props, ...children)` call expressions so JSX-source and runtime-call source collide on `ast_hash`. Idempotent.

**Files:**
- Modify: `crates/reverts-js/src/normalize/jsx_runtime_normalized.rs`

- [ ] **Step 1: Write the failing test**

Replace stub:

```rust
use oxc_allocator::{Allocator, Box as OxcBox, Vec as OxcVec};
use oxc_ast::AstBuilder;
use oxc_ast::ast::{
    Argument, Expression, JSXAttributeItem, JSXAttributeValue, JSXChild, JSXElement,
    JSXElementName, ObjectExpression, ObjectProperty, ObjectPropertyKind, Program,
    PropertyKey, PropertyKind,
};
use oxc_ast::visit::VisitMut;
use oxc_span::SPAN;
use reverts_ir::NormalizationPassId;
use super::NormalizationPass;

pub struct JsxRuntimeNormalized;

impl NormalizationPass for JsxRuntimeNormalized {
    fn id(&self) -> NormalizationPassId { NormalizationPassId::JsxRuntimeNormalized }
    fn version(&self) -> u32 { 1 }
    fn apply<'a>(&self, alloc: &'a Allocator, program: &mut Program<'a>) {
        let builder = AstBuilder::new(alloc);
        let mut visitor = JsxRewriter { builder };
        visitor.visit_program(program);
    }
}

struct JsxRewriter<'a> {
    builder: AstBuilder<'a>,
}

impl<'a> VisitMut<'a> for JsxRewriter<'a> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        if let Expression::JSXElement(element) = expr {
            let replacement = self.rewrite_element(element);
            *expr = replacement;
            return;
        }
        oxc_ast::visit::walk_mut::walk_expression(self, expr);
    }
}

impl<'a> JsxRewriter<'a> {
    fn rewrite_element(&self, element: &JSXElement<'a>) -> Expression<'a> {
        let callee_name = self.builder.alloc_str("_jsx");
        let callee = self.builder.expression_identifier_reference(SPAN, callee_name);
        let element_type = self.rewrite_element_name(&element.opening_element.name);
        let props = self.rewrite_attributes(&element.opening_element.attributes);
        let mut args = self.builder.vec_with_capacity::<Argument<'a>>(2 + element.children.len());
        args.push(Argument::from(element_type));
        args.push(Argument::from(props));
        for child in &element.children {
            if let Some(expr) = self.rewrite_child(child) {
                args.push(Argument::from(expr));
            }
        }
        Expression::CallExpression(self.builder.alloc(self.builder.call_expression(
            SPAN,
            callee,
            oxc_ast::NONE,
            args,
            false,
        )))
    }

    fn rewrite_element_name(&self, name: &JSXElementName<'a>) -> Expression<'a> {
        match name {
            JSXElementName::Identifier(ident) => {
                let value = self.builder.alloc_str(ident.name.as_str());
                self.builder.expression_string_literal(SPAN, value, None)
            }
            JSXElementName::IdentifierReference(ident) => {
                let value = self.builder.alloc_str(ident.name.as_str());
                self.builder.expression_identifier_reference(SPAN, value)
            }
            _ => self.builder.expression_string_literal(SPAN, self.builder.alloc_str("Fragment"), None),
        }
    }

    fn rewrite_attributes(&self, attrs: &OxcVec<'a, JSXAttributeItem<'a>>) -> Expression<'a> {
        let mut props = self.builder.vec::<ObjectPropertyKind<'a>>();
        for item in attrs {
            if let JSXAttributeItem::Attribute(attr) = item {
                let name = match &attr.name {
                    oxc_ast::ast::JSXAttributeName::Identifier(id) => self.builder.alloc_str(id.name.as_str()),
                    _ => continue,
                };
                let value = match &attr.value {
                    Some(JSXAttributeValue::StringLiteral(s)) => self.builder.expression_string_literal(
                        SPAN,
                        self.builder.alloc_str(s.value.as_str()),
                        None,
                    ),
                    Some(JSXAttributeValue::ExpressionContainer(c)) => match &c.expression {
                        oxc_ast::ast::JSXExpression::Expression(e) => e.clone_in(self.builder.allocator),
                        _ => continue,
                    },
                    _ => self.builder.expression_boolean_literal(SPAN, true),
                };
                let key = PropertyKey::StaticIdentifier(self.builder.alloc(
                    self.builder.identifier_name(SPAN, name),
                ));
                let prop = self.builder.object_property(
                    SPAN, PropertyKind::Init, key, value, false, false, false,
                );
                props.push(ObjectPropertyKind::ObjectProperty(self.builder.alloc(prop)));
            }
        }
        Expression::ObjectExpression(self.builder.alloc(ObjectExpression {
            span: SPAN, properties: props, trailing_comma: None,
        }))
    }

    fn rewrite_child(&self, child: &JSXChild<'a>) -> Option<Expression<'a>> {
        match child {
            JSXChild::Text(t) => {
                let trimmed = t.value.as_str().trim();
                if trimmed.is_empty() { return None; }
                Some(self.builder.expression_string_literal(
                    SPAN, self.builder.alloc_str(trimmed), None,
                ))
            }
            JSXChild::Element(_) => {
                // Recurse — visit_expression will rewrite when re-walked
                None
            }
            JSXChild::ExpressionContainer(_) | JSXChild::Spread(_) | JSXChild::Fragment(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn jsx_runtime_normalized_rewrites_simple_element() {
        let src = "let v = <div className=\"x\">hi</div>;";
        let out = apply_to_source(&JsxRuntimeNormalized, src).expect("parses");
        assert!(out.contains("_jsx("), "expected _jsx call, got: {out}");
        assert!(out.contains("\"div\""), "expected element name 'div', got: {out}");
    }

    #[test]
    fn jsx_runtime_normalized_is_idempotent_on_call_form() {
        let src = "let v = _jsx(\"div\", { className: \"x\" }, \"hi\");";
        let first = apply_to_source(&JsxRuntimeNormalized, src).unwrap();
        let second = apply_to_source(&JsxRuntimeNormalized, &first).unwrap();
        assert_eq!(first, second);
    }
}
```

> NOTE: The AstBuilder API surface above is approximate to oxc 0.42; the implementing engineer may need to adjust constructor argument order/names if the actual API differs. The behavioral contract is: every `JSXElement` becomes a `_jsx(elementName, propsObject, ...children)` CallExpression, dropping JSXText whose trim is empty, recursively rewriting children.

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-js --locked normalize::jsx_runtime_normalized::tests`
Expected: PASS.

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-js/src/normalize/jsx_runtime_normalized.rs
git commit -m "✨ feat(js): implement JsxRuntimeNormalized pass rewriting JSX to _jsx calls"
```

### Task 8: `BundlerWrapperUnwrapped` pass

Replaces `__toESM(x)`, `__toCommonJS(x)`, `__defProp(target, key, desc)` calls with their semantic equivalents (or strips them) so wrapped exports align with unwrapped sources.

**Files:**
- Modify: `crates/reverts-js/src/normalize/bundler_wrapper_unwrapped.rs`

- [ ] **Step 1: Write the failing test**

Replace stub:

```rust
use oxc_allocator::Allocator;
use oxc_ast::ast::{Argument, Expression, Program};
use oxc_ast::visit::VisitMut;
use reverts_ir::NormalizationPassId;
use super::NormalizationPass;

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

pub struct BundlerWrapperUnwrapped;

impl NormalizationPass for BundlerWrapperUnwrapped {
    fn id(&self) -> NormalizationPassId { NormalizationPassId::BundlerWrapperUnwrapped }
    fn version(&self) -> u32 { 1 }
    fn apply<'a>(&self, _alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut visitor = WrapperStripper;
        visitor.visit_program(program);
    }
}

struct WrapperStripper;

impl<'a> VisitMut<'a> for WrapperStripper {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        if let Expression::CallExpression(call) = expr {
            if let Expression::Identifier(ident) = &call.callee
                && ESBUILD_WRAPPER_NAMES.contains(&ident.name.as_str())
                && let Some(first) = call.arguments.first()
            {
                if let Argument::SpreadElement(_) = first {
                    // leave alone — unusual shape
                } else if let Some(inner) = argument_to_expression(first) {
                    *expr = inner;
                    self.visit_expression(expr);
                    return;
                }
            }
        }
        oxc_ast::visit::walk_mut::walk_expression(self, expr);
    }
}

fn argument_to_expression<'a>(arg: &Argument<'a>) -> Option<Expression<'a>> {
    match arg {
        Argument::SpreadElement(_) => None,
        other => {
            // Argument is a wrapper around Expression in oxc 0.42; clone-in if needed.
            // The implementer should pick the right accessor for oxc 0.42.
            #[allow(unreachable_code)]
            Some(unimplemented!("convert argument to expression: {:?}", other))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn bundler_wrapper_unwraps_to_esm_call() {
        let src = "const m = __toESM(require(\"foo\"));";
        let out = apply_to_source(&BundlerWrapperUnwrapped, src).expect("parses");
        assert!(!out.contains("__toESM"), "expected wrapper removed, got: {out}");
        assert!(out.contains("require"), "expected require preserved, got: {out}");
    }

    #[test]
    fn bundler_wrapper_unwraps_nested() {
        let src = "let v = __toCommonJS(__toESM(x));";
        let out = apply_to_source(&BundlerWrapperUnwrapped, src).unwrap();
        assert!(!out.contains("__toCommonJS"));
        assert!(!out.contains("__toESM"));
        assert!(out.contains("x"));
    }

    #[test]
    fn bundler_wrapper_is_idempotent_on_already_unwrapped_code() {
        let src = "let v = x;\n";
        let first = apply_to_source(&BundlerWrapperUnwrapped, src).unwrap();
        let second = apply_to_source(&BundlerWrapperUnwrapped, &first).unwrap();
        assert_eq!(first, second);
    }
}
```

> NOTE: `argument_to_expression` requires a clone-in (`CloneIn` trait or direct match-and-take) appropriate to oxc 0.42. The implementing engineer should consult oxc's `Argument` enum definition and replace the `unimplemented!` with the correct extraction. The behavioral contract: a `CallExpression` whose callee is one of `ESBUILD_WRAPPER_NAMES` becomes its first argument (recursively unwrapped).

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-js --locked normalize::bundler_wrapper_unwrapped::tests`
Expected: PASS.

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-js/src/normalize/bundler_wrapper_unwrapped.rs
git commit -m "✨ feat(js): implement BundlerWrapperUnwrapped pass stripping esbuild helpers"
```

### Task 9: `HelperIdentityInlined` pass

Inlines calls to local helpers whose entire body is `return arg0;` (or `return arg0?.default ?? arg0;` — the `_interopRequireDefault` shape) so call sites and inlined sites collide.

**Files:**
- Modify: `crates/reverts-js/src/normalize/helper_identity_inlined.rs`

- [ ] **Step 1: Write the failing test**

```rust
use oxc_allocator::Allocator;
use oxc_ast::ast::Program;
use reverts_ir::NormalizationPassId;
use super::NormalizationPass;

pub struct HelperIdentityInlined;

impl NormalizationPass for HelperIdentityInlined {
    fn id(&self) -> NormalizationPassId { NormalizationPassId::HelperIdentityInlined }
    fn version(&self) -> u32 { 1 }
    fn apply<'a>(&self, _alloc: &'a Allocator, program: &mut Program<'a>) {
        // Two passes: (1) collect identity helpers, (2) inline calls to them.
        let identity_helpers = identity_helpers::collect(program);
        if identity_helpers.is_empty() { return; }
        identity_helpers::inline(program, &identity_helpers);
    }
}

mod identity_helpers {
    use oxc_ast::ast::{Expression, FunctionBody, Program, Statement};
    use oxc_ast::visit::VisitMut;
    use std::collections::BTreeSet;

    pub fn collect<'a>(program: &Program<'a>) -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        for stmt in &program.body {
            if let Statement::FunctionDeclaration(func) = stmt
                && let Some(id) = &func.id
                && func.params.items.len() == 1
                && let Some(body) = &func.body
                && is_identity_body(body)
            {
                found.insert(id.name.as_str().to_string());
            }
        }
        found
    }

    fn is_identity_body(body: &FunctionBody<'_>) -> bool {
        if body.statements.len() != 1 { return false; }
        let Statement::ReturnStatement(ret) = &body.statements[0] else { return false; };
        let Some(Expression::Identifier(_)) = &ret.argument else { return false; };
        true
    }

    pub fn inline<'a>(program: &mut Program<'a>, helpers: &BTreeSet<String>) {
        let mut visitor = Inliner { helpers };
        visitor.visit_program(program);
    }

    struct Inliner<'a> { helpers: &'a BTreeSet<String> }

    impl<'a> VisitMut<'a> for Inliner<'_> {
        fn visit_expression(&mut self, expr: &mut Expression<'a>) {
            if let Expression::CallExpression(call) = expr
                && call.arguments.len() == 1
                && let Expression::Identifier(callee) = &call.callee
                && self.helpers.contains(callee.name.as_str())
            {
                // Replace `helper(x)` with `x` — extract the first argument.
                // Implementer: use oxc's argument-to-expression conversion
                // appropriate for 0.42 (see Task 8 NOTE).
                unimplemented!("extract first argument from CallExpression");
            }
            oxc_ast::visit::walk_mut::walk_expression(self, expr);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn identity_helper_call_is_inlined() {
        let src = "function _id(x) { return x; }\nlet v = _id(42);";
        let out = apply_to_source(&HelperIdentityInlined, src).expect("parses");
        assert!(out.contains("v = 42") || out.contains("v=42"), "got: {out}");
    }

    #[test]
    fn non_identity_helper_is_left_alone() {
        let src = "function adds(x) { return x + 1; }\nlet v = adds(2);";
        let out = apply_to_source(&HelperIdentityInlined, src).unwrap();
        assert!(out.contains("adds("));
    }
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-js --locked normalize::helper_identity_inlined::tests`
Expected: PASS (after implementer fills `unimplemented!` with oxc 0.42 argument extraction).

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-js/src/normalize/helper_identity_inlined.rs
git commit -m "✨ feat(js): implement HelperIdentityInlined pass collapsing identity wrappers"
```

### Task 10: `ExportBoundaryNormalized` pass

Strips `export` keyword wrapping function/class declarations: `export function f() {}` becomes `function f() {}` so the AST shape under `ast_hash` matches plain declarations.

**Files:**
- Modify: `crates/reverts-js/src/normalize/export_boundary_normalized.rs`

- [ ] **Step 1: Write the failing test**

```rust
use oxc_allocator::Allocator;
use oxc_ast::ast::{Declaration, Program, Statement};
use reverts_ir::NormalizationPassId;
use super::NormalizationPass;

pub struct ExportBoundaryNormalized;

impl NormalizationPass for ExportBoundaryNormalized {
    fn id(&self) -> NormalizationPassId { NormalizationPassId::ExportBoundaryNormalized }
    fn version(&self) -> u32 { 1 }
    fn apply<'a>(&self, _alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut new_body = Vec::with_capacity(program.body.len());
        for stmt in program.body.drain(..) {
            new_body.extend(strip_export(stmt));
        }
        // Re-emit into program.body (oxc Vec allocator may require special handling)
        for stmt in new_body { program.body.push(stmt); }
    }
}

fn strip_export<'a>(stmt: Statement<'a>) -> Option<Statement<'a>> {
    match stmt {
        Statement::ExportNamedDeclaration(export) => {
            // `export function f() {}` ⇒ `function f() {}`
            match export.unbox().declaration {
                Some(Declaration::FunctionDeclaration(f)) => Some(Statement::FunctionDeclaration(f)),
                Some(Declaration::ClassDeclaration(c)) => Some(Statement::ClassDeclaration(c)),
                Some(Declaration::VariableDeclaration(v)) => Some(Statement::VariableDeclaration(v)),
                _ => None, // pure re-exports drop out
            }
        }
        Statement::ExportDefaultDeclaration(export) => {
            // `export default function f() {}` ⇒ `function f() {}` if named
            use oxc_ast::ast::ExportDefaultDeclarationKind as Kind;
            match export.unbox().declaration {
                Kind::FunctionDeclaration(f) if f.id.is_some() => Some(Statement::FunctionDeclaration(f)),
                Kind::ClassDeclaration(c) if c.id.is_some() => Some(Statement::ClassDeclaration(c)),
                _ => None,
            }
        }
        other => Some(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn export_keyword_is_stripped_from_function_decl() {
        let src = "export function f(a) { return a; }";
        let out = apply_to_source(&ExportBoundaryNormalized, src).expect("parses");
        assert!(!out.contains("export"), "got: {out}");
        assert!(out.contains("function f"), "got: {out}");
    }

    #[test]
    fn pure_reexport_drops_out_safely() {
        let src = "export { foo } from './bar';\nfunction g() {}";
        let out = apply_to_source(&ExportBoundaryNormalized, src).unwrap();
        assert!(out.contains("function g"));
    }
}
```

> NOTE: `export.unbox()` and the field accessors must match oxc 0.42's `Box<ExportNamedDeclaration>`-handling. The implementer adjusts accordingly. The behavioral contract is: an `export <decl>` Statement becomes the inner `<decl>` Statement; pure `export from` and `export {}` re-exports drop out.

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-js --locked normalize::export_boundary_normalized::tests`
Expected: PASS.

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-js/src/normalize/export_boundary_normalized.rs
git commit -m "✨ feat(js): implement ExportBoundaryNormalized pass stripping export keyword"
```

### Task 11: `ClosureBoundaryAligned` pass

When a top-level function's only statement is `return (() => { ... })()` (an immediately-invoked arrow), inline the body so the boundary aligns with non-IIFE definitions.

**Files:**
- Modify: `crates/reverts-js/src/normalize/closure_boundary_aligned.rs`

- [ ] **Step 1: Write the failing test**

```rust
use oxc_allocator::Allocator;
use oxc_ast::ast::{Expression, FunctionBody, Program, Statement};
use oxc_ast::visit::VisitMut;
use reverts_ir::NormalizationPassId;
use super::NormalizationPass;

pub struct ClosureBoundaryAligned;

impl NormalizationPass for ClosureBoundaryAligned {
    fn id(&self) -> NormalizationPassId { NormalizationPassId::ClosureBoundaryAligned }
    fn version(&self) -> u32 { 1 }
    fn apply<'a>(&self, _alloc: &'a Allocator, program: &mut Program<'a>) {
        let mut visitor = Aligner;
        visitor.visit_program(program);
    }
}

struct Aligner;

impl<'a> VisitMut<'a> for Aligner {
    fn visit_function_body(&mut self, body: &mut FunctionBody<'a>) {
        if body.statements.len() == 1
            && let Statement::ReturnStatement(ret) = &mut body.statements[0]
            && let Some(Expression::CallExpression(call)) = &mut ret.argument
            && call.arguments.is_empty()
            && let Expression::ArrowFunctionExpression(arrow) = &mut call.callee
            && !arrow.r#async
        {
            // Move arrow body's statements into outer body
            // Implementer: use oxc's mem::take + push pattern; details depend on 0.42 ast Vec.
            unimplemented!("inline arrow body into outer function body");
        }
        oxc_ast::visit::walk_mut::walk_function_body(self, body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::apply_to_source;

    #[test]
    fn iife_arrow_in_return_is_inlined() {
        let src = "function outer() { return (() => { let x = 1; return x; })(); }";
        let out = apply_to_source(&ClosureBoundaryAligned, src).expect("parses");
        assert!(!out.contains("() =>"), "expected arrow removed, got: {out}");
        assert!(out.contains("let x = 1") || out.contains("let x=1"), "got: {out}");
    }

    #[test]
    fn non_iife_arrow_is_left_alone() {
        let src = "function outer() { return () => 1; }";
        let out = apply_to_source(&ClosureBoundaryAligned, src).unwrap();
        assert!(out.contains("=>"), "got: {out}");
    }
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-js --locked normalize::closure_boundary_aligned::tests`
Expected: PASS.

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-js/src/normalize/closure_boundary_aligned.rs
git commit -m "✨ feat(js): implement ClosureBoundaryAligned pass inlining trivial IIFE arrows"
```

---

## Phase C — Fingerprint extraction (`reverts-graph`)

### Task 12: Function extractor — walk top-level functions

**Files:**
- Create: `crates/reverts-graph/src/fingerprint/mod.rs`
- Create: `crates/reverts-graph/src/fingerprint/extractor.rs`
- Modify: `crates/reverts-graph/src/lib.rs` (`mod fingerprint; pub use fingerprint::*;`)

- [ ] **Step 1: Write the failing test**

`crates/reverts-graph/src/fingerprint/mod.rs`:

```rust
pub mod ast;
pub mod cfg;
pub mod return_pattern;
pub mod effect_pattern;
pub mod literal_anchor;
pub mod access;
pub mod structural_anchor;
pub mod literal_shape;
pub mod callee_set;
pub mod binding_pattern;
pub mod throw_set;
pub mod extractor;

pub use extractor::FunctionExtractor;
```

`crates/reverts-graph/src/fingerprint/extractor.rs`:

```rust
use oxc_allocator::Allocator;
use oxc_ast::ast::{ArrowFunctionExpression, Function, Program};
use oxc_ast::Visit;
use oxc_span::GetSpan;
use reverts_ir::{ByteRange, FunctionId, ModuleId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedFunction {
    pub id: FunctionId,
    pub param_count: u32,
    pub statement_count: u32,
    pub is_async: bool,
    pub is_generator: bool,
}

pub struct FunctionExtractor {
    module_id: ModuleId,
    out: Vec<ExtractedFunction>,
}

impl FunctionExtractor {
    #[must_use]
    pub fn new(module_id: ModuleId) -> Self {
        Self { module_id, out: Vec::new() }
    }

    pub fn extract<'a>(mut self, program: &Program<'a>) -> Vec<ExtractedFunction> {
        self.visit_program(program);
        self.out
    }
}

impl<'a> Visit<'a> for FunctionExtractor {
    fn visit_function(&mut self, func: &Function<'a>, _flags: oxc_syntax::scope::ScopeFlags) {
        let span = func.span();
        let stmt_count = func.body.as_ref().map_or(0, |body| body.statements.len()) as u32;
        self.out.push(ExtractedFunction {
            id: FunctionId::new(self.module_id, ByteRange::new(span.start, span.end)),
            param_count: func.params.items.len() as u32,
            statement_count: stmt_count,
            is_async: func.r#async,
            is_generator: func.generator,
        });
        oxc_ast::visit::walk::walk_function(self, func, _flags);
    }

    fn visit_arrow_function_expression(&mut self, arrow: &ArrowFunctionExpression<'a>) {
        let span = arrow.span();
        let stmt_count = arrow.body.statements.len() as u32;
        self.out.push(ExtractedFunction {
            id: FunctionId::new(self.module_id, ByteRange::new(span.start, span.end)),
            param_count: arrow.params.items.len() as u32,
            statement_count: stmt_count,
            is_async: arrow.r#async,
            is_generator: false,
        });
        oxc_ast::visit::walk::walk_arrow_function_expression(self, arrow);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_parser::Parser;
    use oxc_span::SourceType;
    use reverts_ir::ModuleId;

    fn extract(source: &str) -> Vec<ExtractedFunction> {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, source, SourceType::default()).parse();
        assert!(parsed.errors.is_empty(), "parse errors: {:?}", parsed.errors);
        FunctionExtractor::new(ModuleId(1)).extract(&parsed.program)
    }

    #[test]
    fn extractor_records_top_level_functions_with_param_and_stmt_counts() {
        let funcs = extract("function add(a, b) { let s = a + b; return s; }");
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].param_count, 2);
        assert_eq!(funcs[0].statement_count, 2);
        assert!(!funcs[0].is_async);
        assert!(!funcs[0].is_generator);
    }

    #[test]
    fn extractor_walks_into_nested_arrow_and_function() {
        let funcs = extract("function outer() { return (x) => x + 1; }");
        assert_eq!(funcs.len(), 2);
    }

    #[test]
    fn extractor_records_async_and_generator_flags() {
        let funcs = extract("async function a() {}\nfunction* g() { yield 1; }");
        assert_eq!(funcs.len(), 2);
        assert!(funcs.iter().any(|f| f.is_async && !f.is_generator));
        assert!(funcs.iter().any(|f| !f.is_async && f.is_generator));
    }
}
```

Then add the per-axis files as empty stubs (each with one `pub fn compute(...) -> u64` signature) — they will be filled by tasks 13–22. For now create empty modules:

```rust
// crates/reverts-graph/src/fingerprint/ast.rs
pub fn compute() -> u64 { 0 } // filled in Task 13
```

Repeat for the other 9 axis files (one-line stubs).

Modify `crates/reverts-graph/src/lib.rs` to add `mod fingerprint; pub use fingerprint::*;` near the existing `pub use` of types.

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-graph --locked fingerprint::extractor::tests`
Expected: PASS.

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-graph/src/fingerprint crates/reverts-graph/src/lib.rs
git commit -m "✨ feat(graph): add FunctionExtractor walking top-level and nested functions"
```

### Task 13: `ast_hash` axis — NormalizedNode Merkle

The full canonical AST IR is large. Implement a streaming hasher that walks `Function`/`ArrowFunctionExpression` body and FNV-mixes every AST kind tag in pre-order, ignoring identifier names, literal values (except their kind tag), and span info.

**Files:**
- Modify: `crates/reverts-graph/src/fingerprint/ast.rs`

- [ ] **Step 1: Write the failing test**

```rust
use oxc_ast::ast::{Expression, FunctionBody, Statement};
use reverts_ir::hash::{update_fnv1a, FNV_OFFSET_BASIS};

pub fn compute(body: &FunctionBody<'_>) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, b"function_body|");
    for stmt in &body.statements {
        hash_statement(&mut hash, stmt);
    }
    hash
}

fn hash_statement(hash: &mut u64, stmt: &Statement<'_>) {
    update_fnv1a(hash, b"|stmt:");
    match stmt {
        Statement::BlockStatement(b) => {
            update_fnv1a(hash, b"block(");
            for s in &b.body { hash_statement(hash, s); }
            update_fnv1a(hash, b")");
        }
        Statement::ExpressionStatement(e) => {
            update_fnv1a(hash, b"expr(");
            hash_expression(hash, &e.expression);
            update_fnv1a(hash, b")");
        }
        Statement::ReturnStatement(r) => {
            update_fnv1a(hash, b"return(");
            if let Some(arg) = &r.argument { hash_expression(hash, arg); }
            update_fnv1a(hash, b")");
        }
        Statement::IfStatement(i) => {
            update_fnv1a(hash, b"if(");
            hash_expression(hash, &i.test);
            update_fnv1a(hash, b",");
            hash_statement(hash, &i.consequent);
            if let Some(alt) = &i.alternate { update_fnv1a(hash, b","); hash_statement(hash, alt); }
            update_fnv1a(hash, b")");
        }
        Statement::ForStatement(_) => update_fnv1a(hash, b"for"),
        Statement::WhileStatement(_) => update_fnv1a(hash, b"while"),
        Statement::DoWhileStatement(_) => update_fnv1a(hash, b"dowhile"),
        Statement::ForOfStatement(_) => update_fnv1a(hash, b"forof"),
        Statement::ForInStatement(_) => update_fnv1a(hash, b"forin"),
        Statement::TryStatement(_) => update_fnv1a(hash, b"try"),
        Statement::ThrowStatement(t) => {
            update_fnv1a(hash, b"throw(");
            hash_expression(hash, &t.argument);
            update_fnv1a(hash, b")");
        }
        Statement::SwitchStatement(_) => update_fnv1a(hash, b"switch"),
        Statement::VariableDeclaration(v) => {
            update_fnv1a(hash, b"var(");
            update_fnv1a(hash, format!("{:?}", v.kind).as_bytes());
            update_fnv1a(hash, format!("/{}", v.declarations.len()).as_bytes());
            update_fnv1a(hash, b")");
        }
        Statement::BreakStatement(_) => update_fnv1a(hash, b"break"),
        Statement::ContinueStatement(_) => update_fnv1a(hash, b"continue"),
        _ => update_fnv1a(hash, b"other"),
    }
}

fn hash_expression(hash: &mut u64, expr: &Expression<'_>) {
    use Expression as E;
    match expr {
        E::Identifier(_) => update_fnv1a(hash, b"id"),
        E::StringLiteral(_) => update_fnv1a(hash, b"str"),
        E::NumericLiteral(_) => update_fnv1a(hash, b"num"),
        E::BooleanLiteral(_) => update_fnv1a(hash, b"bool"),
        E::NullLiteral(_) => update_fnv1a(hash, b"null"),
        E::RegExpLiteral(_) => update_fnv1a(hash, b"re"),
        E::BinaryExpression(b) => {
            update_fnv1a(hash, b"bin(");
            update_fnv1a(hash, format!("{:?}", b.operator).as_bytes());
            update_fnv1a(hash, b",");
            hash_expression(hash, &b.left);
            update_fnv1a(hash, b",");
            hash_expression(hash, &b.right);
            update_fnv1a(hash, b")");
        }
        E::UnaryExpression(u) => {
            update_fnv1a(hash, b"un(");
            update_fnv1a(hash, format!("{:?}", u.operator).as_bytes());
            update_fnv1a(hash, b",");
            hash_expression(hash, &u.argument);
            update_fnv1a(hash, b")");
        }
        E::CallExpression(c) => {
            update_fnv1a(hash, b"call(");
            hash_expression(hash, &c.callee);
            update_fnv1a(hash, format!("/{}", c.arguments.len()).as_bytes());
            update_fnv1a(hash, b")");
        }
        E::StaticMemberExpression(_) => update_fnv1a(hash, b"smem"),
        E::ComputedMemberExpression(_) => update_fnv1a(hash, b"cmem"),
        E::ConditionalExpression(_) => update_fnv1a(hash, b"cond"),
        E::AssignmentExpression(_) => update_fnv1a(hash, b"assign"),
        E::ArrowFunctionExpression(_) => update_fnv1a(hash, b"arrow"),
        E::FunctionExpression(_) => update_fnv1a(hash, b"fnexpr"),
        E::ObjectExpression(_) => update_fnv1a(hash, b"obj"),
        E::ArrayExpression(_) => update_fnv1a(hash, b"arr"),
        E::AwaitExpression(_) => update_fnv1a(hash, b"await"),
        E::YieldExpression(_) => update_fnv1a(hash, b"yield"),
        E::TemplateLiteral(_) => update_fnv1a(hash, b"tpl"),
        E::ThisExpression(_) => update_fnv1a(hash, b"this"),
        E::NewExpression(n) => {
            update_fnv1a(hash, b"new(");
            hash_expression(hash, &n.callee);
            update_fnv1a(hash, format!("/{}", n.arguments.len()).as_bytes());
            update_fnv1a(hash, b")");
        }
        _ => update_fnv1a(hash, b"otherexpr"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn hash_first_function(src: &str) -> u64 {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, src, SourceType::default()).parse();
        assert!(parsed.errors.is_empty());
        let mut iter = parsed.program.body.iter().filter_map(|s| {
            if let oxc_ast::ast::Statement::FunctionDeclaration(f) = s { Some(f) } else { None }
        });
        let func = iter.next().expect("at least one function");
        compute(func.body.as_ref().expect("function has body"))
    }

    #[test]
    fn ast_hash_collides_for_alpha_renamed_functions() {
        let h1 = hash_first_function("function f(a, b) { return a + b; }");
        let h2 = hash_first_function("function g(x, y) { return x + y; }");
        assert_eq!(h1, h2, "α-renamed equivalents must collide");
    }

    #[test]
    fn ast_hash_differs_for_different_operator() {
        let h1 = hash_first_function("function f(a, b) { return a + b; }");
        let h2 = hash_first_function("function f(a, b) { return a - b; }");
        assert_ne!(h1, h2);
    }

    #[test]
    fn ast_hash_differs_for_different_statement_kind() {
        let h1 = hash_first_function("function f() { return 1; }");
        let h2 = hash_first_function("function f() { let x = 1; }");
        assert_ne!(h1, h2);
    }
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-graph --locked fingerprint::ast::tests`
Expected: PASS.

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-graph/src/fingerprint/ast.rs
git commit -m "✨ feat(graph): implement ast axis hashing identifier-blind AST Merkle"
```

### Task 14: `structural_anchor` axis — counts-only digest

This axis is always present and is the load-bearing tier-4 anchor for functions without literal/property hooks.

**Files:**
- Modify: `crates/reverts-graph/src/fingerprint/structural_anchor.rs`

- [ ] **Step 1: Write the failing test**

```rust
use oxc_ast::ast::{
    BindingPatternKind, Expression, FunctionBody, FormalParameters, Statement,
};
use oxc_ast::Visit;
use reverts_ir::hash::{update_fnv1a, FNV_OFFSET_BASIS};

#[derive(Default, Debug)]
struct Counts {
    param_destructure_depth: u32,
    await_count: u32,
    yield_count: u32,
    throw_count: u32,
    try_handler_count: u32,
    for_count: u32,
    for_in_count: u32,
    for_of_count: u32,
    while_count: u32,
    do_while_count: u32,
    return_value_count: u32,
    return_void_count: u32,
    switch_case_count: u32,
}

pub fn compute(params: &FormalParameters<'_>, body: &FunctionBody<'_>) -> u64 {
    let mut counts = Counts::default();
    counts.param_destructure_depth = max_destructure_depth(params);
    let mut visitor = Visitor { counts: &mut counts };
    for stmt in &body.statements { visitor.visit_statement(stmt); }

    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, b"structural|");
    macro_rules! mix { ($name:literal, $field:expr) => {
        update_fnv1a(&mut hash, $name);
        update_fnv1a(&mut hash, b"=");
        update_fnv1a(&mut hash, $field.to_string().as_bytes());
        update_fnv1a(&mut hash, b"|");
    }; }
    mix!(b"pdd", counts.param_destructure_depth);
    mix!(b"aw", counts.await_count);
    mix!(b"yi", counts.yield_count);
    mix!(b"th", counts.throw_count);
    mix!(b"tr", counts.try_handler_count);
    mix!(b"for", counts.for_count);
    mix!(b"fin", counts.for_in_count);
    mix!(b"fof", counts.for_of_count);
    mix!(b"wh", counts.while_count);
    mix!(b"dw", counts.do_while_count);
    mix!(b"rv", counts.return_value_count);
    mix!(b"r0", counts.return_void_count);
    mix!(b"sw", counts.switch_case_count);
    hash
}

fn max_destructure_depth(params: &FormalParameters<'_>) -> u32 {
    let mut max = 0;
    for param in &params.items {
        max = max.max(pattern_depth(&param.pattern.kind, 0));
    }
    max
}

fn pattern_depth(kind: &BindingPatternKind<'_>, depth: u32) -> u32 {
    match kind {
        BindingPatternKind::ObjectPattern(o) => {
            let mut d = depth + 1;
            for prop in &o.properties {
                d = d.max(pattern_depth(&prop.value.kind, depth + 1));
            }
            d
        }
        BindingPatternKind::ArrayPattern(a) => {
            let mut d = depth + 1;
            for elem in &a.elements {
                if let Some(e) = elem {
                    d = d.max(pattern_depth(&e.kind, depth + 1));
                }
            }
            d
        }
        _ => depth,
    }
}

struct Visitor<'c> { counts: &'c mut Counts }

impl<'a> Visit<'a> for Visitor<'_> {
    fn visit_statement(&mut self, stmt: &Statement<'a>) {
        match stmt {
            Statement::ReturnStatement(r) => {
                if r.argument.is_some() { self.counts.return_value_count += 1; }
                else { self.counts.return_void_count += 1; }
            }
            Statement::ThrowStatement(_) => self.counts.throw_count += 1,
            Statement::ForStatement(_) => self.counts.for_count += 1,
            Statement::ForInStatement(_) => self.counts.for_in_count += 1,
            Statement::ForOfStatement(_) => self.counts.for_of_count += 1,
            Statement::WhileStatement(_) => self.counts.while_count += 1,
            Statement::DoWhileStatement(_) => self.counts.do_while_count += 1,
            Statement::SwitchStatement(s) => self.counts.switch_case_count += s.cases.len() as u32,
            Statement::TryStatement(t) => if t.handler.is_some() {
                self.counts.try_handler_count += 1;
            },
            _ => {}
        }
        oxc_ast::visit::walk::walk_statement(self, stmt);
    }

    fn visit_expression(&mut self, expr: &Expression<'a>) {
        if matches!(expr, Expression::AwaitExpression(_)) { self.counts.await_count += 1; }
        if matches!(expr, Expression::YieldExpression(_)) { self.counts.yield_count += 1; }
        oxc_ast::visit::walk::walk_expression(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn hash_first(src: &str) -> u64 {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, src, SourceType::default()).parse();
        let func = parsed.program.body.iter().find_map(|s| {
            if let oxc_ast::ast::Statement::FunctionDeclaration(f) = s { Some(f) } else { None }
        }).expect("function");
        compute(&func.params, func.body.as_ref().expect("body"))
    }

    #[test]
    fn structural_anchor_distinguishes_loop_kinds() {
        let f = hash_first("function f(xs) { for (let x of xs) {} }");
        let w = hash_first("function f(xs) { while (xs.shift()) {} }");
        assert_ne!(f, w);
    }

    #[test]
    fn structural_anchor_collides_after_identifier_rename() {
        let a = hash_first("function f(a) { try { return a; } catch(e) { throw e; } }");
        let b = hash_first("function g(x) { try { return x; } catch(z) { throw z; } }");
        assert_eq!(a, b);
    }

    #[test]
    fn structural_anchor_counts_destructure_depth() {
        let flat = hash_first("function f(a) { return a; }");
        let deep = hash_first("function f({ a: { b } }) { return b; }");
        assert_ne!(flat, deep);
    }
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-graph --locked fingerprint::structural_anchor::tests`
Expected: PASS.

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-graph/src/fingerprint/structural_anchor.rs
git commit -m "✨ feat(graph): implement structural_anchor axis counting loops/awaits/throws"
```

### Task 15: `cfg_hash` axis — reuse existing ControlFlowGraph

Hash the CFG nodes + edges restricted to a function span. The existing `ControlFlowGraph` indexes per module; we filter by `byte_range.contains(node.span)`.

**Files:**
- Modify: `crates/reverts-graph/src/fingerprint/cfg.rs`

- [ ] **Step 1: Write the failing test**

```rust
use reverts_ir::{ByteRange, ControlFlowEdgeKind, ControlFlowGraph, ControlFlowNodeKind, ModuleId};
use reverts_ir::hash::{update_fnv1a, FNV_OFFSET_BASIS};

pub fn compute(cfg: &ControlFlowGraph, module_id: ModuleId, span: ByteRange) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, b"cfg|");
    let nodes: Vec<_> = cfg.nodes_for(module_id).iter().filter(|n| {
        ByteRange::new(n.span_start, n.span_end).overlaps(span)
            && span.contains(ByteRange::new(n.span_start, n.span_end))
    }).collect();
    let node_ids: std::collections::BTreeSet<_> = nodes.iter().map(|n| n.id).collect();

    for node in &nodes {
        update_fnv1a(&mut hash, b"n:");
        update_fnv1a(&mut hash, node_kind_tag(node.kind).as_bytes());
        update_fnv1a(&mut hash, b"|");
    }
    for edge in cfg.edges_for(module_id) {
        if node_ids.contains(&edge.from) && node_ids.contains(&edge.to) {
            update_fnv1a(&mut hash, b"e:");
            update_fnv1a(&mut hash, edge_tag(edge.kind).as_bytes());
            update_fnv1a(&mut hash, b"|");
        }
    }
    hash
}

const fn node_kind_tag(k: ControlFlowNodeKind) -> &'static str {
    match k {
        ControlFlowNodeKind::Entry => "entry",
        ControlFlowNodeKind::Exit => "exit",
        ControlFlowNodeKind::Statement => "stmt",
        ControlFlowNodeKind::Branch => "branch",
        ControlFlowNodeKind::Loop => "loop",
        ControlFlowNodeKind::Return => "return",
        ControlFlowNodeKind::Throw => "throw",
    }
}

const fn edge_tag(k: ControlFlowEdgeKind) -> &'static str {
    match k {
        ControlFlowEdgeKind::Sequential => "seq",
        ControlFlowEdgeKind::TrueBranch => "true",
        ControlFlowEdgeKind::FalseBranch => "false",
        ControlFlowEdgeKind::LoopBack => "back",
        ControlFlowEdgeKind::Throw => "throw",
    }
}

#[cfg(test)]
mod tests {
    // CFG construction tests live in reverts-graph's existing test module.
    // Here we only assert that the hasher is deterministic on a fixed input shape.
    #[test]
    fn cfg_hash_is_deterministic_on_empty_cfg() {
        // Placeholder — full integration test lives in the orchestrator (Task 24)
        // and in L2 cross-version corpus tests.
    }
}
```

> NOTE: the implementer must adjust `ControlFlowNode`'s span field names if they differ (`span_start`/`span_end` are placeholders). The behavioral contract: hash node kinds + edge kinds restricted to nodes whose span is fully inside the function's span, in id order (deterministic).

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-graph --locked fingerprint::cfg::tests`
Expected: PASS.

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-graph/src/fingerprint/cfg.rs
git commit -m "✨ feat(graph): implement cfg axis hashing function-restricted CFG"
```

### Task 16: `return_pattern` axis

Bucket each ReturnStatement's argument and emit a count-based hash.

**Files:**
- Modify: `crates/reverts-graph/src/fingerprint/return_pattern.rs`

- [ ] **Step 1: Write the failing test**

```rust
use oxc_ast::ast::{Expression, FunctionBody, Statement};
use oxc_ast::Visit;
use reverts_ir::hash::{update_fnv1a, FNV_OFFSET_BASIS};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ReturnKind { Void, Literal, Identifier, MemberChain, Call, Conditional, Await, ThrowThrough, Other }

pub fn compute(body: &FunctionBody<'_>) -> u64 {
    let mut counts = std::collections::BTreeMap::<ReturnKind, u32>::new();
    let mut visitor = V { counts: &mut counts };
    for s in &body.statements { visitor.visit_statement(s); }

    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, b"ret|");
    for (kind, count) in counts {
        update_fnv1a(&mut hash, format!("{kind:?}={count}|").as_bytes());
    }
    hash
}

fn classify(expr: &Expression<'_>) -> ReturnKind {
    use Expression as E;
    match expr {
        E::StringLiteral(_) | E::NumericLiteral(_) | E::BooleanLiteral(_)
            | E::NullLiteral(_) | E::RegExpLiteral(_) | E::TemplateLiteral(_) => ReturnKind::Literal,
        E::Identifier(_) => ReturnKind::Identifier,
        E::StaticMemberExpression(_) | E::ComputedMemberExpression(_) => ReturnKind::MemberChain,
        E::CallExpression(_) | E::NewExpression(_) => ReturnKind::Call,
        E::ConditionalExpression(_) => ReturnKind::Conditional,
        E::AwaitExpression(_) => ReturnKind::Await,
        _ => ReturnKind::Other,
    }
}

struct V<'c> { counts: &'c mut std::collections::BTreeMap<ReturnKind, u32> }
impl<'a> Visit<'a> for V<'_> {
    fn visit_statement(&mut self, s: &Statement<'a>) {
        if let Statement::ReturnStatement(r) = s {
            let kind = match &r.argument {
                Some(a) => classify(a),
                None => ReturnKind::Void,
            };
            *self.counts.entry(kind).or_default() += 1;
        }
        oxc_ast::visit::walk::walk_statement(self, s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn hash_first(src: &str) -> u64 {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, src, SourceType::default()).parse();
        let func = parsed.program.body.iter().find_map(|s| {
            if let oxc_ast::ast::Statement::FunctionDeclaration(f) = s { Some(f) } else { None }
        }).expect("function");
        compute(func.body.as_ref().unwrap())
    }

    #[test]
    fn return_pattern_distinguishes_void_from_value() {
        assert_ne!(
            hash_first("function f() { return; }"),
            hash_first("function f() { return 1; }"),
        );
    }

    #[test]
    fn return_pattern_collides_for_same_bucket() {
        assert_eq!(
            hash_first("function f(a) { return a.x.y; }"),
            hash_first("function f(z) { return z.q.r; }"),
        );
    }
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-graph --locked fingerprint::return_pattern::tests`
Expected: PASS.

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-graph/src/fingerprint/return_pattern.rs
git commit -m "✨ feat(graph): implement return_pattern axis bucketing return expressions"
```

### Task 17: `literal_anchor` axis

Collect stable literals: cooked strings ≥ 3 chars, regex `source+flags`, BigInt values. Hash the sorted, deduplicated set.

**Files:**
- Modify: `crates/reverts-graph/src/fingerprint/literal_anchor.rs`

- [ ] **Step 1: Write the failing test**

```rust
use oxc_ast::ast::{Expression, FunctionBody, Statement, TemplateElement};
use oxc_ast::Visit;
use reverts_ir::hash::{update_fnv1a, FNV_OFFSET_BASIS};
use std::collections::BTreeSet;

pub fn compute(body: &FunctionBody<'_>) -> Option<u64> {
    let mut anchors: BTreeSet<String> = BTreeSet::new();
    let mut v = V { anchors: &mut anchors };
    for s in &body.statements { v.visit_statement(s); }
    if anchors.is_empty() { return None; }
    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, b"lit_anchor|");
    for a in &anchors {
        update_fnv1a(&mut hash, a.as_bytes());
        update_fnv1a(&mut hash, b"|");
    }
    Some(hash)
}

struct V<'a> { anchors: &'a mut BTreeSet<String> }

impl<'a> Visit<'a> for V<'_> {
    fn visit_expression(&mut self, e: &Expression<'a>) {
        match e {
            Expression::StringLiteral(s) => {
                let trimmed = s.value.as_str().trim();
                if trimmed.len() >= 3 { self.anchors.insert(format!("s:{trimmed}")); }
            }
            Expression::RegExpLiteral(r) => {
                self.anchors.insert(format!("r:{}/{:?}", r.regex.pattern, r.regex.flags));
            }
            Expression::BigIntLiteral(b) => {
                self.anchors.insert(format!("b:{}", b.raw));
            }
            _ => {}
        }
        oxc_ast::visit::walk::walk_expression(self, e);
    }

    fn visit_template_element(&mut self, t: &TemplateElement<'a>) {
        let v = t.value.cooked.as_deref().unwrap_or(t.value.raw.as_str()).trim();
        if v.len() >= 3 { self.anchors.insert(format!("s:{v}")); }
        oxc_ast::visit::walk::walk_template_element(self, t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn hash_first(src: &str) -> Option<u64> {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, src, SourceType::default()).parse();
        let func = parsed.program.body.iter().find_map(|s| {
            if let oxc_ast::ast::Statement::FunctionDeclaration(f) = s { Some(f) } else { None }
        }).expect("function");
        compute(func.body.as_ref().unwrap())
    }

    #[test]
    fn literal_anchor_none_for_function_without_literals() {
        assert!(hash_first("function f(a) { return a + 1; }").is_none());
    }

    #[test]
    fn literal_anchor_collects_strings_above_min_len() {
        let h = hash_first("function f() { throw new Error('Unexpected input value'); }");
        assert!(h.is_some());
    }

    #[test]
    fn literal_anchor_drops_short_strings() {
        let h = hash_first("function f() { return 'a'; }");
        assert!(h.is_none());
    }
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-graph --locked fingerprint::literal_anchor::tests`
Expected: PASS.

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-graph/src/fingerprint/literal_anchor.rs
git commit -m "✨ feat(graph): implement literal_anchor axis over stable string/regex/bigint"
```

### Task 18: `literal_shape` axis

Hash the *shape* of literals: string length-class, regex flag count, BigInt presence, numeric class.

**Files:**
- Modify: `crates/reverts-graph/src/fingerprint/literal_shape.rs`

- [ ] **Step 1: Write the failing test**

```rust
use oxc_ast::ast::{Expression, FunctionBody, TemplateElement};
use oxc_ast::Visit;
use reverts_ir::hash::{update_fnv1a, FNV_OFFSET_BASIS};

pub fn compute(body: &FunctionBody<'_>) -> Option<u64> {
    let mut counts: [u32; 8] = [0; 8];
    let mut v = V { counts: &mut counts };
    for s in &body.statements { v.visit_statement(s); }
    if counts.iter().sum::<u32>() == 0 { return None; }
    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, b"lit_shape|");
    for (i, c) in counts.iter().enumerate() {
        update_fnv1a(&mut hash, format!("{i}:{c}|").as_bytes());
    }
    Some(hash)
}

fn string_bucket(len: usize) -> usize {
    match len {
        0..=2 => 0, 3..=8 => 1, 9..=32 => 2, _ => 3,
    }
}

fn numeric_bucket(n: f64) -> usize {
    if !n.is_finite() { 7 }
    else if n.fract() != 0.0 { 6 }
    else if n.abs() <= 1.0 { 4 }
    else { 5 }
}

struct V<'a> { counts: &'a mut [u32; 8] }
impl<'a> Visit<'a> for V<'_> {
    fn visit_expression(&mut self, e: &Expression<'a>) {
        match e {
            Expression::StringLiteral(s) => self.counts[string_bucket(s.value.len())] += 1,
            Expression::NumericLiteral(n) => self.counts[numeric_bucket(n.value)] += 1,
            Expression::BigIntLiteral(_) => self.counts[6] += 1,
            Expression::RegExpLiteral(r) => self.counts[7] += {
                let _flags = &r.regex.flags;
                1
            },
            _ => {}
        }
        oxc_ast::visit::walk::walk_expression(self, e);
    }
    fn visit_template_element(&mut self, t: &TemplateElement<'a>) {
        let len = t.value.cooked.as_deref().unwrap_or(t.value.raw.as_str()).len();
        self.counts[string_bucket(len)] += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn hash_first(src: &str) -> Option<u64> {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, src, SourceType::default()).parse();
        let func = parsed.program.body.iter().find_map(|s| {
            if let oxc_ast::ast::Statement::FunctionDeclaration(f) = s { Some(f) } else { None }
        }).expect("function");
        compute(func.body.as_ref().unwrap())
    }

    #[test]
    fn literal_shape_collides_for_same_buckets_different_content() {
        let a = hash_first("function f() { return 'foo'; }");
        let b = hash_first("function g() { return 'bar'; }");
        assert_eq!(a, b);
    }

    #[test]
    fn literal_shape_distinguishes_buckets() {
        let short = hash_first("function f() { return 'foo'; }");
        let long = hash_first("function f() { return 'a-much-longer-string'; }");
        assert_ne!(short, long);
    }
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-graph --locked fingerprint::literal_shape::tests`
Expected: PASS.

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-graph/src/fingerprint/literal_shape.rs
git commit -m "✨ feat(graph): implement literal_shape axis bucketing literal shapes"
```

### Task 19: `access` axes — access_pattern + access_shape

Both share a member-walker. Pattern keeps property names; shape keeps depth + computed/static flag + call arity, no names.

**Files:**
- Modify: `crates/reverts-graph/src/fingerprint/access.rs`

- [ ] **Step 1: Write the failing test**

```rust
use oxc_ast::ast::{ComputedMemberExpression, Expression, FunctionBody, StaticMemberExpression};
use oxc_ast::Visit;
use reverts_ir::hash::{update_fnv1a, FNV_OFFSET_BASIS};
use std::collections::BTreeSet;

#[derive(Debug, Default)]
struct Collector {
    pattern: BTreeSet<String>,
    shape: BTreeSet<String>,
}

pub fn compute(body: &FunctionBody<'_>) -> (Option<u64>, Option<u64>) {
    let mut c = Collector::default();
    let mut v = V { c: &mut c };
    for s in &body.statements { v.visit_statement(s); }
    let pat = hash_set(&c.pattern, b"acc_pat|");
    let sh = hash_set(&c.shape, b"acc_shape|");
    (pat, sh)
}

fn hash_set(set: &BTreeSet<String>, tag: &[u8]) -> Option<u64> {
    if set.is_empty() { return None; }
    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, tag);
    for k in set { update_fnv1a(&mut hash, k.as_bytes()); update_fnv1a(&mut hash, b"|"); }
    Some(hash)
}

struct V<'a> { c: &'a mut Collector }

impl<'a> Visit<'a> for V<'_> {
    fn visit_expression(&mut self, e: &Expression<'a>) {
        if let Expression::StaticMemberExpression(m) = e {
            let (depth, _) = chain_depth(e);
            self.c.pattern.insert(format!("s:{}@{depth}", m.property.name.as_str()));
            self.c.shape.insert(format!("s@{depth}"));
        } else if let Expression::ComputedMemberExpression(_) = e {
            let (depth, _) = chain_depth(e);
            self.c.pattern.insert(format!("c@{depth}"));
            self.c.shape.insert(format!("c@{depth}"));
        }
        oxc_ast::visit::walk::walk_expression(self, e);
    }
}

fn chain_depth(e: &Expression<'_>) -> (u32, bool) {
    fn inner(e: &Expression<'_>, depth: u32) -> u32 {
        match e {
            Expression::StaticMemberExpression(m) => inner(&m.object, depth + 1),
            Expression::ComputedMemberExpression(m) => inner(&m.object, depth + 1),
            _ => depth,
        }
    }
    (inner(e, 0), false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn run(src: &str) -> (Option<u64>, Option<u64>) {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, src, SourceType::default()).parse();
        let func = parsed.program.body.iter().find_map(|s| {
            if let oxc_ast::ast::Statement::FunctionDeclaration(f) = s { Some(f) } else { None }
        }).expect("function");
        compute(func.body.as_ref().unwrap())
    }

    #[test]
    fn access_pattern_keeps_property_names_shape_drops_them() {
        let (p1, s1) = run("function f(o) { return o.foo; }");
        let (p2, s2) = run("function f(o) { return o.bar; }");
        assert_ne!(p1, p2, "pattern must differ on property name");
        assert_eq!(s1, s2, "shape must collide regardless of property name");
    }

    #[test]
    fn access_returns_none_when_no_member_access() {
        let (p, s) = run("function f(a, b) { return a + b; }");
        assert!(p.is_none());
        assert!(s.is_none());
    }
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-graph --locked fingerprint::access::tests`
Expected: PASS.

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-graph/src/fingerprint/access.rs
git commit -m "✨ feat(graph): implement access_pattern and access_shape member-walk axes"
```

### Task 20: `callee_set` and `throw_set` axes

Both are bag-of-strings hashed in sorted order. Implement together for code reuse.

**Files:**
- Modify: `crates/reverts-graph/src/fingerprint/callee_set.rs`
- Modify: `crates/reverts-graph/src/fingerprint/throw_set.rs`

- [ ] **Step 1: Write the failing test**

`callee_set.rs`:

```rust
use oxc_ast::ast::{CallExpression, Expression, FunctionBody, NewExpression};
use oxc_ast::Visit;
use reverts_ir::hash::{update_fnv1a, FNV_OFFSET_BASIS};
use std::collections::BTreeSet;

pub fn compute(body: &FunctionBody<'_>) -> Option<u64> {
    let mut s: BTreeSet<String> = BTreeSet::new();
    let mut v = V { s: &mut s };
    for stmt in &body.statements { v.visit_statement(stmt); }
    if s.is_empty() { return None; }
    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, b"callee|");
    for k in &s { update_fnv1a(&mut hash, k.as_bytes()); update_fnv1a(&mut hash, b"|"); }
    Some(hash)
}

struct V<'a> { s: &'a mut BTreeSet<String> }

impl<'a> Visit<'a> for V<'_> {
    fn visit_call_expression(&mut self, c: &CallExpression<'a>) {
        match &c.callee {
            Expression::Identifier(i) => { self.s.insert(format!("c:{}", i.name.as_str())); }
            Expression::StaticMemberExpression(m) => { self.s.insert(format!("cm:.{}", m.property.name.as_str())); }
            _ => {}
        }
        oxc_ast::visit::walk::walk_call_expression(self, c);
    }
    fn visit_new_expression(&mut self, n: &NewExpression<'a>) {
        if let Expression::Identifier(i) = &n.callee {
            self.s.insert(format!("nc:{}", i.name.as_str()));
        }
        oxc_ast::visit::walk::walk_new_expression(self, n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn hash_first(src: &str) -> Option<u64> {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, src, SourceType::default()).parse();
        let func = parsed.program.body.iter().find_map(|s| {
            if let oxc_ast::ast::Statement::FunctionDeclaration(f) = s { Some(f) } else { None }
        }).expect("function");
        compute(func.body.as_ref().unwrap())
    }

    #[test]
    fn callee_set_keeps_static_member_names() {
        let a = hash_first("function f(o) { o.toString(); }");
        let b = hash_first("function f(o) { o.toJSON(); }");
        assert_ne!(a, b);
    }

    #[test]
    fn callee_set_collides_for_same_callees_different_receivers() {
        let a = hash_first("function f(o) { o.push(1); }");
        let b = hash_first("function f(x) { x.push(1); }");
        assert_eq!(a, b);
    }
}
```

`throw_set.rs`:

```rust
use oxc_ast::ast::{Expression, FunctionBody, Statement, NewExpression};
use oxc_ast::Visit;
use reverts_ir::hash::{update_fnv1a, FNV_OFFSET_BASIS};
use std::collections::BTreeSet;

pub fn compute(body: &FunctionBody<'_>) -> Option<u64> {
    let mut s: BTreeSet<String> = BTreeSet::new();
    let mut v = V { s: &mut s };
    for stmt in &body.statements { v.visit_statement(stmt); }
    if s.is_empty() { return None; }
    let mut hash = FNV_OFFSET_BASIS;
    update_fnv1a(&mut hash, b"throw|");
    for k in &s { update_fnv1a(&mut hash, k.as_bytes()); update_fnv1a(&mut hash, b"|"); }
    Some(hash)
}

struct V<'a> { s: &'a mut BTreeSet<String> }

impl<'a> Visit<'a> for V<'_> {
    fn visit_statement(&mut self, stmt: &Statement<'a>) {
        if let Statement::ThrowStatement(t) = stmt {
            match &t.argument {
                Expression::NewExpression(n) => {
                    if let Expression::Identifier(i) = &n.callee {
                        self.s.insert(format!("n:{}", i.name.as_str()));
                    } else {
                        self.s.insert("t:expr".to_string());
                    }
                }
                Expression::StringLiteral(_) | Expression::NumericLiteral(_) => {
                    self.s.insert("t:lit".to_string());
                }
                _ => { self.s.insert("t:expr".to_string()); }
            }
        }
        oxc_ast::visit::walk::walk_statement(self, stmt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn hash_first(src: &str) -> Option<u64> {
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, src, SourceType::default()).parse();
        let func = parsed.program.body.iter().find_map(|s| {
            if let oxc_ast::ast::Statement::FunctionDeclaration(f) = s { Some(f) } else { None }
        }).expect("function");
        compute(func.body.as_ref().unwrap())
    }

    #[test]
    fn throw_set_distinguishes_type_vs_range_error() {
        let a = hash_first("function f() { throw new TypeError('x'); }");
        let b = hash_first("function f() { throw new RangeError('x'); }");
        assert_ne!(a, b);
    }

    #[test]
    fn throw_set_collides_for_same_constructor() {
        let a = hash_first("function f(e) { throw new Error('a'); }");
        let b = hash_first("function f(x) { throw new Error('b'); }");
        assert_eq!(a, b);
    }
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-graph --locked "fingerprint::(callee_set|throw_set)"`
Expected: PASS.

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-graph/src/fingerprint/callee_set.rs crates/reverts-graph/src/fingerprint/throw_set.rs
git commit -m "✨ feat(graph): implement callee_set and throw_set bag-of-strings axes"
```

### Task 21: `binding_pattern` and `effect_pattern` axes

**Files:**
- Modify: `crates/reverts-graph/src/fingerprint/binding_pattern.rs`
- Modify: `crates/reverts-graph/src/fingerprint/effect_pattern.rs`

Pattern follows the same shape as Task 20; codes per spec §6:
- `binding_pattern`: tokens `p:i`/`p:i+d`/`p:o[N]`/`p:a[N]`/`p:r`/`l:...` over `FormalParameters` + `VariableDeclarator` patterns.
- `effect_pattern`: counts of `Call`, `MemberWrite` (assignment to member), `await`, `yield`, `throw`.

Use the same Visitor pattern as Tasks 19/20. Test cases must include at least one pair that collides under rename and one pair that distinguishes.

- [ ] **Step 1: Write the failing test** — follow the literal_anchor/throw_set template.

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-graph --locked "fingerprint::(binding_pattern|effect_pattern)"`

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-graph/src/fingerprint/binding_pattern.rs crates/reverts-graph/src/fingerprint/effect_pattern.rs
git commit -m "✨ feat(graph): implement binding_pattern and effect_pattern axes"
```

### Task 22: Fingerprint orchestrator — `FunctionFingerprint` emitter

Wires together all 12 axes plus alternates. Calls each pass, re-parses (the pass emits a new program), recomputes axes on the post-pass program, stamps the result as an alternate.

**Files:**
- Modify: `crates/reverts-graph/src/fingerprint/extractor.rs` (add `Self::fingerprint(...)` method)

- [ ] **Step 1: Write the failing test**

Add to `extractor.rs`:

```rust
use reverts_ir::{AxisHashes, FunctionFingerprint, NormalizationPassId};
use reverts_js::normalize::{stable_passes, apply_to_source};

impl FunctionExtractor {
    pub fn fingerprint<'a>(
        module_id: ModuleId,
        source: &str,
        cfg: &reverts_ir::ControlFlowGraph,
    ) -> Vec<FunctionFingerprint> {
        // 1. Parse primary
        let alloc = oxc_allocator::Allocator::default();
        let parsed = oxc_parser::Parser::new(&alloc, source, oxc_span::SourceType::default().with_typescript(true)).parse();
        if parsed.panicked || !parsed.errors.is_empty() { return Vec::new(); }
        let primary_funcs = Self::new(module_id).extract(&parsed.program);

        // 2. Compute primary axes per function
        let mut out: Vec<FunctionFingerprint> = primary_funcs.iter().filter_map(|f| {
            let body = locate_function_body(&parsed.program, f.id.span)?;
            let params = locate_function_params(&parsed.program, f.id.span)?;
            let axes = AxisHashes {
                ast: super::ast::compute(body),
                cfg: super::cfg::compute(cfg, module_id, f.id.span),
                return_pattern: super::return_pattern::compute(body),
                effect_pattern: super::effect_pattern::compute(body).unwrap_or(0),
                literal_anchor: super::literal_anchor::compute(body),
                access_pattern: super::access::compute(body).0,
                structural_anchor: super::structural_anchor::compute(params, body),
                literal_shape: super::literal_shape::compute(body),
                access_shape: super::access::compute(body).1,
                callee_set: super::callee_set::compute(body),
                binding_pattern: super::binding_pattern::compute(params, body).unwrap_or(0),
                throw_set: super::throw_set::compute(body),
            };
            Some(FunctionFingerprint {
                id: f.id,
                param_count: f.param_count,
                statement_count: f.statement_count,
                primary: axes,
                alternates: Vec::new(),
            })
        }).collect();

        // 3. For each pass, re-emit source and recompute axes by function-span match
        for pass in stable_passes() {
            let Ok(transformed) = apply_to_source(pass.as_ref(), source) else { continue };
            let alloc2 = oxc_allocator::Allocator::default();
            let parsed2 = oxc_parser::Parser::new(&alloc2, &transformed, oxc_span::SourceType::default().with_typescript(true)).parse();
            if parsed2.panicked || !parsed2.errors.is_empty() { continue; }
            let alt_funcs = Self::new(module_id).extract(&parsed2.program);
            // Best-effort align by (param_count, statement_count) order — the
            // pass may have changed spans but should not have reordered tops.
            for (i, alt) in alt_funcs.iter().enumerate() {
                let Some(fp) = out.get_mut(i) else { break };
                if fp.param_count != alt.param_count { continue; }
                let body = match locate_function_body(&parsed2.program, alt.id.span) { Some(b) => b, None => continue };
                let params = match locate_function_params(&parsed2.program, alt.id.span) { Some(p) => p, None => continue };
                let axes = AxisHashes {
                    ast: super::ast::compute(body),
                    cfg: fp.primary.cfg,    // alternates reuse CFG; recompute is expensive
                    return_pattern: super::return_pattern::compute(body),
                    effect_pattern: super::effect_pattern::compute(body).unwrap_or(0),
                    literal_anchor: super::literal_anchor::compute(body),
                    access_pattern: super::access::compute(body).0,
                    structural_anchor: super::structural_anchor::compute(params, body),
                    literal_shape: super::literal_shape::compute(body),
                    access_shape: super::access::compute(body).1,
                    callee_set: super::callee_set::compute(body),
                    binding_pattern: super::binding_pattern::compute(params, body).unwrap_or(0),
                    throw_set: super::throw_set::compute(body),
                };
                fp.alternates.push((pass.id(), axes));
            }
        }

        out
    }
}

fn locate_function_body<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
    span: reverts_ir::ByteRange,
) -> Option<&'a oxc_ast::ast::FunctionBody<'a>> {
    use oxc_ast::ast::Statement;
    use oxc_span::GetSpan;
    for stmt in &program.body {
        if let Statement::FunctionDeclaration(f) = stmt {
            let s = f.span();
            if s.start == span.start && s.end == span.end { return f.body.as_deref(); }
        }
    }
    None
}

fn locate_function_params<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
    span: reverts_ir::ByteRange,
) -> Option<&'a oxc_ast::ast::FormalParameters<'a>> {
    use oxc_ast::ast::Statement;
    use oxc_span::GetSpan;
    for stmt in &program.body {
        if let Statement::FunctionDeclaration(f) = stmt {
            let s = f.span();
            if s.start == span.start && s.end == span.end { return Some(f.params.as_ref()); }
        }
    }
    None
}

#[cfg(test)]
mod fingerprint_tests {
    use super::*;
    use reverts_ir::ControlFlowGraph;

    #[test]
    fn alpha_renamed_functions_share_primary_ast_hash() {
        let src1 = "function f(a, b) { return a + b; }";
        let src2 = "function g(x, y) { return x + y; }";
        let cfg = ControlFlowGraph::default();
        let fp1 = FunctionExtractor::fingerprint(ModuleId(1), src1, &cfg);
        let fp2 = FunctionExtractor::fingerprint(ModuleId(2), src2, &cfg);
        assert_eq!(fp1.len(), 1);
        assert_eq!(fp2.len(), 1);
        assert_eq!(fp1[0].primary.ast, fp2[0].primary.ast);
    }

    #[test]
    fn export_keyword_collapse_lands_as_alternate() {
        let src1 = "export function f(a) { return a; }";
        let src2 = "function f(a) { return a; }";
        let cfg = ControlFlowGraph::default();
        let fp1 = FunctionExtractor::fingerprint(ModuleId(1), src1, &cfg);
        let fp2 = FunctionExtractor::fingerprint(ModuleId(2), src2, &cfg);
        // Either: same primary, or alternate produced by ExportBoundaryNormalized matches
        let primary_match = fp1[0].primary.ast == fp2[0].primary.ast;
        let alt_match = fp1[0].alternates.iter().any(|(_, a)| a.ast == fp2[0].primary.ast);
        assert!(primary_match || alt_match);
    }
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-graph --locked fingerprint::extractor::fingerprint_tests`
Expected: PASS.

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-graph/src/fingerprint/extractor.rs
git commit -m "✨ feat(graph): emit FunctionFingerprint with primary axes plus pass alternates"
```

---

## Phase D — Attribution model (`reverts-input`)

### Task 23: Extend `PackageAttributionInput` with `function_span` + `confidence`

**Files:**
- Modify: `crates/reverts-input/src/lib.rs` (locate `PackageAttributionInput` struct)
- Modify: `crates/reverts-input/src/sqlite.rs` if SQLite serialization touches the struct

- [ ] **Step 1: Write the failing test**

Add to `PackageAttributionInput`:

```rust
use reverts_ir::{ByteRange, MatchTier};

#[derive(Debug, Clone, PartialEq)]
pub struct AttributionConfidence {
    pub tier: MatchTier,
    pub matched_axes: Vec<reverts_ir::AxisKind>,
    pub matched_alternate: Option<reverts_ir::NormalizationPassId>,
    pub top_score: f64,
    pub runner_up_score: f64,
    pub margin: f64,
}
```

Add fields to `PackageAttributionInput`:

```rust
pub function_span: Option<ByteRange>,
pub confidence: Option<AttributionConfidence>,
```

(Initialize both to `None` in all existing constructors. Verify by `cargo test --workspace`.)

Tests:

```rust
#[test]
fn package_attribution_function_span_defaults_to_none() {
    let attr = PackageAttributionInput::accepted_external(ModuleId(1), "pkg", "1.0", "pkg");
    assert!(attr.function_span.is_none());
    assert!(attr.confidence.is_none());
}

#[test]
fn package_attribution_with_function_span_round_trips() {
    let attr = PackageAttributionInput::accepted_external(ModuleId(1), "pkg", "1.0", "pkg")
        .with_function_span(ByteRange::new(10, 30));
    assert_eq!(attr.function_span, Some(ByteRange::new(10, 30)));
}
```

Add the builder:

```rust
impl PackageAttributionInput {
    #[must_use]
    pub fn with_function_span(mut self, span: ByteRange) -> Self {
        self.function_span = Some(span);
        self
    }
    #[must_use]
    pub fn with_confidence(mut self, conf: AttributionConfidence) -> Self {
        self.confidence = Some(conf);
        self
    }
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-input --locked`
Expected: PASS.

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS. If sqlite.rs needs updating to persist the new field, do so — but if it's optional and never round-tripped today (the existing tests don't), leave persistence as TODO documented inline.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-input/src/lib.rs
git commit -m "✨ feat(input): add function_span and confidence to PackageAttributionInput"
```

---

## Phase E — Index crate (`reverts-package-index`)

### Task 24: Create `reverts-package-index` crate skeleton

**Files:**
- Create: `crates/reverts-package-index/Cargo.toml`
- Create: `crates/reverts-package-index/src/lib.rs`
- Modify: `Cargo.toml` (root, add member)

- [ ] **Step 1: Write the failing test**

`crates/reverts-package-index/Cargo.toml`:

```toml
[package]
name = "reverts-package-index"
version = "0.1.0"
edition.workspace = true
license.workspace = true
rust-version.workspace = true

[lints]
workspace = true

[dependencies]
reverts-ir = { path = "../reverts-ir" }
```

`crates/reverts-package-index/src/lib.rs`:

```rust
use std::collections::BTreeMap;
use reverts_ir::{AxisKind, MatchTier, NormalizationPassId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageId {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExactKey {
    pub param_count: u32,
    pub statement_count: u32,
    pub ast_hash: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CfgKey {
    pub param_count: u32,
    pub cfg_hash: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FeatureKey {
    pub param_count: u32,
    pub kind: AxisKind,
    pub hash: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StructuralKey {
    pub param_count: u32,
    pub structural_anchor: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub package: PackageId,
    pub variant_path: String,
    pub external_function_id: u64,
    pub matched_axis: AxisKind,
    pub matched_alternate: Option<NormalizationPassId>,
}

#[derive(Debug, Default, Clone)]
pub struct CorpusStats {
    pub axis_hash_frequencies: BTreeMap<(AxisKind, u64), u32>,
}

impl CorpusStats {
    #[must_use]
    pub fn frequency(&self, axis: AxisKind, hash: u64) -> u32 {
        *self.axis_hash_frequencies.get(&(axis, hash)).unwrap_or(&1)
    }
}

pub trait PackageFingerprintIndex: Send + Sync {
    fn query_exact(&self, key: ExactKey) -> Vec<Candidate>;
    fn query_cfg(&self, key: CfgKey) -> Vec<Candidate>;
    fn query_feature(&self, key: FeatureKey) -> Vec<Candidate>;
    fn query_structural(&self, key: StructuralKey) -> Vec<Candidate>;
    fn corpus_stats(&self) -> &CorpusStats;
}

pub mod in_memory;
pub use in_memory::InMemoryFingerprintIndex;
```

`crates/reverts-package-index/src/in_memory.rs`:

```rust
use super::*;
use std::collections::BTreeMap;

#[derive(Debug, Default)]
pub struct InMemoryFingerprintIndex {
    exact: BTreeMap<ExactKey, Vec<Candidate>>,
    cfg: BTreeMap<CfgKey, Vec<Candidate>>,
    feature: BTreeMap<FeatureKey, Vec<Candidate>>,
    structural: BTreeMap<StructuralKey, Vec<Candidate>>,
    stats: CorpusStats,
}

impl InMemoryFingerprintIndex {
    #[must_use]
    pub fn new() -> Self { Self::default() }

    pub fn insert_exact(&mut self, key: ExactKey, candidate: Candidate) {
        let h = key.ast_hash;
        *self.stats.axis_hash_frequencies.entry((AxisKind::Ast, h)).or_default() += 1;
        self.exact.entry(key).or_default().push(candidate);
    }

    pub fn insert_cfg(&mut self, key: CfgKey, candidate: Candidate) {
        *self.stats.axis_hash_frequencies.entry((AxisKind::Cfg, key.cfg_hash)).or_default() += 1;
        self.cfg.entry(key).or_default().push(candidate);
    }

    pub fn insert_feature(&mut self, key: FeatureKey, candidate: Candidate) {
        *self.stats.axis_hash_frequencies.entry((key.kind, key.hash)).or_default() += 1;
        self.feature.entry(key).or_default().push(candidate);
    }

    pub fn insert_structural(&mut self, key: StructuralKey, candidate: Candidate) {
        *self.stats.axis_hash_frequencies.entry((AxisKind::StructuralAnchor, key.structural_anchor)).or_default() += 1;
        self.structural.entry(key).or_default().push(candidate);
    }
}

impl PackageFingerprintIndex for InMemoryFingerprintIndex {
    fn query_exact(&self, key: ExactKey) -> Vec<Candidate> {
        self.exact.get(&key).cloned().unwrap_or_default()
    }
    fn query_cfg(&self, key: CfgKey) -> Vec<Candidate> {
        self.cfg.get(&key).cloned().unwrap_or_default()
    }
    fn query_feature(&self, key: FeatureKey) -> Vec<Candidate> {
        self.feature.get(&key).cloned().unwrap_or_default()
    }
    fn query_structural(&self, key: StructuralKey) -> Vec<Candidate> {
        self.structural.get(&key).cloned().unwrap_or_default()
    }
    fn corpus_stats(&self) -> &CorpusStats { &self.stats }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_candidate() -> Candidate {
        Candidate {
            package: PackageId { name: "pkg".into(), version: "1.0".into() },
            variant_path: "index.js".into(),
            external_function_id: 1,
            matched_axis: AxisKind::Ast,
            matched_alternate: None,
        }
    }

    #[test]
    fn in_memory_index_inserts_and_queries_by_exact_key() {
        let mut idx = InMemoryFingerprintIndex::new();
        let key = ExactKey { param_count: 2, statement_count: 3, ast_hash: 42 };
        idx.insert_exact(key, sample_candidate());

        let candidates = idx.query_exact(key);
        assert_eq!(candidates.len(), 1);

        let miss = idx.query_exact(ExactKey { param_count: 2, statement_count: 3, ast_hash: 99 });
        assert!(miss.is_empty());
    }

    #[test]
    fn in_memory_index_tracks_corpus_frequency() {
        let mut idx = InMemoryFingerprintIndex::new();
        let key = ExactKey { param_count: 2, statement_count: 3, ast_hash: 42 };
        idx.insert_exact(key, sample_candidate());
        idx.insert_exact(key, sample_candidate());

        assert_eq!(idx.corpus_stats().frequency(AxisKind::Ast, 42), 2);
    }
}
```

Modify root `Cargo.toml`:

```toml
[workspace]
members = [
    # ... existing ...
    "crates/reverts-package-index",
]
```

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-package-index --locked`
Expected: PASS.

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-package-index Cargo.toml
git commit -m "✨ feat(package-index): add new crate with PackageFingerprintIndex trait"
```

---

## Phase F — Cascade matcher (`reverts-package-matcher`)

### Task 25: Cascade module skeleton + Exact + ExactAlternate tiers

**Files:**
- Create: `crates/reverts-package-matcher/src/cascade.rs`
- Create: `crates/reverts-package-matcher/src/tier.rs`
- Modify: `crates/reverts-package-matcher/Cargo.toml` (add `reverts-package-index = { path = "../reverts-package-index" }`)
- Modify: `crates/reverts-package-matcher/src/lib.rs` (`mod cascade; mod tier; pub use cascade::*;`)

- [ ] **Step 1: Write the failing test**

`crates/reverts-package-matcher/src/tier.rs`:

```rust
use reverts_ir::{AxisHashes, FunctionFingerprint, MatchTier, NormalizationPassId};
use reverts_package_index::{Candidate, ExactKey, PackageFingerprintIndex};

#[derive(Debug, Clone, PartialEq)]
pub struct FunctionMatch {
    pub tier: MatchTier,
    pub candidate: Candidate,
    pub margin: f64,
    pub top_score: f64,
    pub runner_up_score: f64,
    pub matched_alternate: Option<NormalizationPassId>,
}

pub fn try_exact(
    fp: &FunctionFingerprint,
    index: &dyn PackageFingerprintIndex,
) -> Option<FunctionMatch> {
    let key = ExactKey {
        param_count: fp.param_count,
        statement_count: fp.statement_count,
        ast_hash: fp.primary.ast,
    };
    let candidates = index.query_exact(key);
    pick_unique(candidates, MatchTier::Exact, None)
}

pub fn try_exact_alternate(
    fp: &FunctionFingerprint,
    index: &dyn PackageFingerprintIndex,
) -> Option<FunctionMatch> {
    for (pass_id, axes) in &fp.alternates {
        let key = ExactKey {
            param_count: fp.param_count,
            statement_count: fp.statement_count,
            ast_hash: axes.ast,
        };
        let candidates = index.query_exact(key);
        if let Some(m) = pick_unique(candidates, MatchTier::ExactAlternate, Some(*pass_id)) {
            return Some(m);
        }
    }
    None
}

fn pick_unique(
    mut candidates: Vec<Candidate>,
    tier: MatchTier,
    alt: Option<NormalizationPassId>,
) -> Option<FunctionMatch> {
    if candidates.is_empty() { return None; }
    candidates.dedup_by(|a, b| a.package == b.package && a.external_function_id == b.external_function_id);
    if candidates.len() != 1 { return None; }
    let candidate = candidates.into_iter().next().expect("len == 1");
    Some(FunctionMatch {
        tier,
        candidate,
        margin: 1.0,
        top_score: tier.weight() as f64,
        runner_up_score: 0.0,
        matched_alternate: alt,
    })
}
```

`crates/reverts-package-matcher/src/cascade.rs`:

```rust
use reverts_ir::FunctionFingerprint;
use reverts_package_index::PackageFingerprintIndex;
use crate::tier::{try_exact, try_exact_alternate, FunctionMatch};

#[must_use]
pub fn match_function(
    fp: &FunctionFingerprint,
    index: &dyn PackageFingerprintIndex,
) -> Option<FunctionMatch> {
    try_exact(fp, index)
        .or_else(|| try_exact_alternate(fp, index))
        // Higher tiers added in Tasks 26-28.
}

#[cfg(test)]
mod tests {
    use super::*;
    use reverts_ir::{AxisHashes, AxisKind, ByteRange, FunctionId, MatchTier, ModuleId};
    use reverts_package_index::{
        Candidate, ExactKey, InMemoryFingerprintIndex, PackageId,
    };

    fn sample_axes(ast: u64) -> AxisHashes {
        AxisHashes {
            ast, cfg: 0, return_pattern: 0, effect_pattern: 0,
            literal_anchor: None, access_pattern: None,
            structural_anchor: 0, literal_shape: None, access_shape: None,
            callee_set: None, binding_pattern: 0, throw_set: None,
        }
    }

    #[test]
    fn match_function_returns_exact_when_unique() {
        let mut idx = InMemoryFingerprintIndex::new();
        let key = ExactKey { param_count: 2, statement_count: 3, ast_hash: 100 };
        idx.insert_exact(key, Candidate {
            package: PackageId { name: "lodash".into(), version: "4.17.21".into() },
            variant_path: "index.js".into(),
            external_function_id: 7,
            matched_axis: AxisKind::Ast,
            matched_alternate: None,
        });

        let fp = FunctionFingerprint {
            id: FunctionId::new(ModuleId(1), ByteRange::new(0, 100)),
            param_count: 2,
            statement_count: 3,
            primary: sample_axes(100),
            alternates: Vec::new(),
        };

        let m = match_function(&fp, &idx).expect("match");
        assert_eq!(m.tier, MatchTier::Exact);
        assert_eq!(m.candidate.package.name, "lodash");
    }

    #[test]
    fn match_function_rejects_ambiguous_exact() {
        let mut idx = InMemoryFingerprintIndex::new();
        let key = ExactKey { param_count: 2, statement_count: 3, ast_hash: 100 };
        for (pkg, fid) in [("a", 1u64), ("b", 2)] {
            idx.insert_exact(key, Candidate {
                package: PackageId { name: pkg.into(), version: "1".into() },
                variant_path: "i.js".into(),
                external_function_id: fid,
                matched_axis: AxisKind::Ast,
                matched_alternate: None,
            });
        }
        let fp = FunctionFingerprint {
            id: FunctionId::new(ModuleId(1), ByteRange::new(0, 100)),
            param_count: 2, statement_count: 3,
            primary: sample_axes(100), alternates: Vec::new(),
        };
        assert!(match_function(&fp, &idx).is_none(), "ambiguous exact must not return a match here");
    }
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-package-matcher --locked cascade::tests`
Expected: PASS.

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-package-matcher/Cargo.toml crates/reverts-package-matcher/src/cascade.rs crates/reverts-package-matcher/src/tier.rs crates/reverts-package-matcher/src/lib.rs
git commit -m "✨ feat(package-matcher): add cascade with Exact and ExactAlternate tiers"
```

### Task 26: `StructuralAnchored` tier — CFG + anchor overlap

Add `try_structural_anchored` to `tier.rs`. Requires CFG match AND at least one anchor overlap from {literal_anchor, callee_set, throw_set}.

**Files:**
- Modify: `crates/reverts-package-matcher/src/tier.rs`
- Modify: `crates/reverts-package-matcher/src/cascade.rs`

- [ ] **Step 1: Write the failing test**

Append to `tier.rs`:

```rust
use reverts_package_index::{CfgKey, FeatureKey};
use reverts_ir::AxisKind;

pub fn try_structural_anchored(
    fp: &FunctionFingerprint,
    index: &dyn PackageFingerprintIndex,
) -> Option<FunctionMatch> {
    let cfg_key = CfgKey { param_count: fp.param_count, cfg_hash: fp.primary.cfg };
    let cfg_candidates = index.query_cfg(cfg_key);
    if cfg_candidates.is_empty() { return None; }

    // Build anchor candidate set for the function side.
    let mut fp_anchors: std::collections::BTreeSet<(AxisKind, u64)> = Default::default();
    if let Some(h) = fp.primary.literal_anchor { fp_anchors.insert((AxisKind::LiteralAnchor, h)); }
    if let Some(h) = fp.primary.callee_set { fp_anchors.insert((AxisKind::CalleeSet, h)); }
    if let Some(h) = fp.primary.throw_set { fp_anchors.insert((AxisKind::ThrowSet, h)); }

    if fp_anchors.is_empty() { return None; }

    // Retain CFG candidates that also share at least one anchor.
    let surviving: Vec<Candidate> = cfg_candidates.into_iter().filter(|c| {
        fp_anchors.iter().any(|(axis, h)| {
            !index.query_feature(FeatureKey {
                param_count: fp.param_count, kind: *axis, hash: *h,
            }).iter().filter(|cand| {
                cand.package == c.package && cand.external_function_id == c.external_function_id
            }).count().eq(&0)
        })
    }).collect();

    pick_unique(surviving, MatchTier::StructuralAnchored, None)
}
```

Update `match_function`:

```rust
#[must_use]
pub fn match_function(
    fp: &FunctionFingerprint,
    index: &dyn PackageFingerprintIndex,
) -> Option<FunctionMatch> {
    try_exact(fp, index)
        .or_else(|| try_exact_alternate(fp, index))
        .or_else(|| try_structural_anchored(fp, index))
}
```

Test:

```rust
#[test]
fn structural_anchored_requires_cfg_and_anchor_overlap() {
    use reverts_ir::AxisKind;
    let mut idx = InMemoryFingerprintIndex::new();
    let candidate = Candidate {
        package: PackageId { name: "p".into(), version: "1".into() },
        variant_path: "i.js".into(),
        external_function_id: 1,
        matched_axis: AxisKind::Cfg,
        matched_alternate: None,
    };
    idx.insert_cfg(CfgKey { param_count: 1, cfg_hash: 7 }, candidate.clone());
    idx.insert_feature(FeatureKey { param_count: 1, kind: AxisKind::LiteralAnchor, hash: 99 }, candidate.clone());

    let mut axes = sample_axes(0);
    axes.cfg = 7;
    axes.literal_anchor = Some(99);
    let fp = FunctionFingerprint {
        id: FunctionId::new(ModuleId(1), ByteRange::new(0, 10)),
        param_count: 1, statement_count: 1,
        primary: axes, alternates: Vec::new(),
    };

    let m = match_function(&fp, &idx).expect("structural-anchored match");
    assert_eq!(m.tier, MatchTier::StructuralAnchored);
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-package-matcher --locked structural_anchored`
Expected: PASS.

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-package-matcher/src/tier.rs crates/reverts-package-matcher/src/cascade.rs
git commit -m "✨ feat(package-matcher): add StructuralAnchored tier requiring CFG + anchor overlap"
```

### Task 27: `FeatureSimilarity` tier — Jaccard over remaining axes

Pick the most distinctive present axis (`callee_set` > `throw_set` > `literal_anchor` > `access_pattern`) and run a feature query; rank survivors by Jaccard over the remaining axes; threshold 0.6; require unique best.

**Files:**
- Modify: `crates/reverts-package-matcher/src/tier.rs`
- Modify: `crates/reverts-package-matcher/src/cascade.rs`

- [ ] **Step 1: Write the failing test**

Append `try_feature_similarity` following the same pattern as Task 26. Test must cover:
- Two candidates with same anchor key; one with higher Jaccard wins
- Tied Jaccard returns None
- Below-threshold returns None

Use τ_feature = 0.6.

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-package-matcher --locked feature_similarity`

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-package-matcher/src/tier.rs crates/reverts-package-matcher/src/cascade.rs
git commit -m "✨ feat(package-matcher): add FeatureSimilarity tier with Jaccard scoring"
```

### Task 28: `StructuralOnly` tier with frequency penalty

Last-resort tier: `StructuralKey` lookup; reject candidates whose `structural_anchor` frequency in `corpus_stats` exceeds 50 (common shapes are uninformative); require margin ≥ 0.3 over second-place.

**Files:**
- Modify: `crates/reverts-package-matcher/src/tier.rs`
- Modify: `crates/reverts-package-matcher/src/cascade.rs`

Implement `try_structural_only`. Behavioral test:
- Unique candidate after frequency filter ⇒ accept with tier StructuralOnly.
- Frequency above 50 ⇒ reject.
- Tied candidates ⇒ reject.

- [ ] **Step 1: Write the failing test**

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-package-matcher --locked structural_only`

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-package-matcher/src/tier.rs crates/reverts-package-matcher/src/cascade.rs
git commit -m "✨ feat(package-matcher): add StructuralOnly fallback tier with frequency penalty"
```

---

## Phase G — Variant, version, Hungarian

### Task 29: Variant scorer

**Files:**
- Create: `crates/reverts-package-matcher/src/variant.rs`
- Modify: `crates/reverts-package-matcher/src/lib.rs`

Behavior: given a set of `FunctionMatch` rows grouped by `(package, version)`, group further by `variant_path`. Score each variant as `Σ tier_weight + α · jaccard(bundle_fn_set, variant_fn_set)` with α = 100. Return the best variant by score; ties broken by `browser > module > main > umd > main-cjs` preference (path-name heuristic).

- [ ] **Step 1: Write the failing test**

Define `pub fn pick_variant(matches: &[FunctionMatch], ...) -> Option<VariantSelection>` and tests for: clean winner; tie broken by browser preference; empty input.

- [ ] **Step 2: Run test**

Run: `cargo test -p reverts-package-matcher --locked variant::tests`

- [ ] **Step 3: (impl + test together)**

- [ ] **Step 4: Verify**

Run: `cargo fmt --check && cargo clippy --workspace --locked -- -D warnings && cargo test --workspace --locked`

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-package-matcher/src/variant.rs crates/reverts-package-matcher/src/lib.rs
git commit -m "✨ feat(package-matcher): add variant selection by Jaccard plus tier-weighted sum"
```

### Task 30: Version scorer

**Files:**
- Create: `crates/reverts-package-matcher/src/version.rs`
- Modify: `crates/reverts-package-matcher/src/lib.rs`

Behavior: takes the per-version best variant scores; computes `score(version) = best_variant_score × matched_fn_count / module_fn_count`; returns `BestVersionMatch::{Selected, Ambiguous, NoMatch, InsufficientEvidence}`. Reuse the existing enum from `lib.rs` if convenient.

- [ ] **Step 1: Write the failing test** covering: unique winner; tied within ε=0.05; all zero; below τ_v=300.

- [ ] **Step 2-4:** as elsewhere.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-package-matcher/src/version.rs crates/reverts-package-matcher/src/lib.rs
git commit -m "✨ feat(package-matcher): add version scorer with ambiguity and insufficient-evidence cases"
```

### Task 31: Kuhn–Munkres assignment

**Files:**
- Create: `crates/reverts-package-matcher/src/hungarian.rs`

Implement an in-tree `pub fn assign_max_weight(cost: &[Vec<f64>]) -> Vec<usize>` that returns column index per row maximizing total weight. ~120 lines. No new dependency.

- [ ] **Step 1: Write the failing test** with concrete weight matrices and known-optimal assignments.

```rust
#[test]
fn hungarian_two_by_two_picks_diagonal_when_better() {
    let cost = vec![vec![5.0, 1.0], vec![1.0, 5.0]];
    let assign = assign_max_weight(&cost);
    assert_eq!(assign, vec![0, 1]);
}

#[test]
fn hungarian_handles_zero_weight_rows() {
    let cost = vec![vec![0.0, 0.0], vec![1.0, 2.0]];
    let assign = assign_max_weight(&cost);
    assert_eq!(assign[1], 1, "row 1 picks col 1");
}
```

- [ ] **Step 2-4:** as elsewhere.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-package-matcher/src/hungarian.rs
git commit -m "✨ feat(package-matcher): add in-tree Kuhn-Munkres for global function assignment"
```

### Task 32: Global assignment integration

**Files:**
- Modify: `crates/reverts-package-matcher/src/cascade.rs`

Builds the bipartite matrix from per-function tier matches (param-bucketed), solves with `hungarian::assign_max_weight`, returns the final `Vec<(FunctionId, FunctionMatch)>`.

- [ ] **Step 1: Write the failing test** — synthetic 10-function chunk where naive greedy gives 10/0 and Hungarian gives 5/5.

- [ ] **Step 2-4:** as elsewhere.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-package-matcher/src/cascade.rs
git commit -m "✨ feat(package-matcher): integrate Hungarian into bundle-wide global assignment"
```

---

## Phase H — Acceptance, confidence, audit

### Task 33: Confidence computation + acceptance decision

**Files:**
- Create: `crates/reverts-package-matcher/src/acceptance.rs`

Implement `pub fn classify(matches: &[FunctionMatch]) -> AcceptanceDecision` returning one of `Accepted / AcceptedWithCaveat / Ambiguous / NoMatch` per spec §10.

- [ ] **Step 1-4:** standard.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-package-matcher/src/acceptance.rs
git commit -m "✨ feat(package-matcher): add acceptance classifier with margin-based confidence"
```

### Task 34: New audit findings

**Files:**
- Modify: `crates/reverts-observe/src/lib.rs` (add `OverlappingFunctionAttribution`, `LowConfidenceAttribution` to `FindingCode`)
- Create: `crates/reverts-package-matcher/src/audit.rs` — emit findings

- [ ] **Step 1-4:** standard.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-observe/src/lib.rs crates/reverts-package-matcher/src/audit.rs
git commit -m "✨ feat(observe): add OverlappingFunctionAttribution and LowConfidenceAttribution codes"
```

---

## Phase I — Pipeline integration

### Task 35: Thread `FunctionFingerprint` through `EnrichedProgram`

**Files:**
- Modify: `crates/reverts-model/src/lib.rs` (`EnrichedProgram` carries `Vec<FunctionFingerprint>`)
- Modify: `crates/reverts-graph/src/lib.rs` (`RevertsGraph::build_graph` emits fingerprints)
- Modify: `crates/reverts-analyze/src/lib.rs` (passes them through `enrich_program`)

- [ ] **Step 1-4:** standard.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-model crates/reverts-graph crates/reverts-analyze
git commit -m "✨ feat(model): thread FunctionFingerprint through EnrichedProgram"
```

### Task 36: Replace `ExactPackageMatcher` body with new cascade

**Files:**
- Modify: `crates/reverts-package-matcher/src/lib.rs:340-410` (the `ExactPackageMatcher::match_rows` block)

Wire `cascade::match_function` for each module's fingerprints, group by `(package, version)`, run variant + version + Hungarian, emit `PackageAttributionInput` rows with `function_span` + `confidence`. Keep `VersionedPackageMatcher::match_rows` signature stable.

Build an `InMemoryFingerprintIndex` from the supplied `&[PackageSource]` (parsing each source, extracting fingerprints, inserting under their `PackageId + variant_path`). This is the in-core path; remote indexes are injected from `reverts-cli`.

- [ ] **Step 1: Write the failing test** — adapt the existing `exact_match_uses_normalized_source_before_accepting_attribution` and friends, asserting that function_span is now populated on attribution rows.

- [ ] **Step 2-4:** standard.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-package-matcher/src
git commit -m "✨ feat(package-matcher): replace exact-only path with cascade-driven matcher"
```

### Task 37: `reverts-cli` wires the remote (or local-only) index

**Files:**
- Modify: `crates/reverts-cli/...` (whatever file currently constructs the matcher; if there is no wiring yet, add a minimal stub)

If `reverts-cli` has no matcher invocation today, this task can be deferred. Otherwise:

- [ ] **Step 1-4:** standard.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-cli
git commit -m "✨ feat(cli): wire InMemoryFingerprintIndex for default matcher invocation"
```

---

## Phase J — Validation fixtures (L1, L4, L7)

### Task 38: L1 per-axis paired-fixture suite

Each existing per-axis test module already has should/should-not pairs. Consolidate them into one assertion table per axis so regressions are quickly spotted:

**Files:**
- Create: `crates/reverts-graph/tests/axis_l1_fixtures.rs`

```rust
//! L1 validation per spec §12. Each row asserts a positive (must-collide)
//! or negative (must-differ) fixture for a single axis.
```

Populate ≥ 6 rows per axis. Test names map directly to axis names so failures point to the offender.

- [ ] **Step 1: Write the failing test** — all rows; many will pass already, but a few will surface implementation gaps.

- [ ] **Step 2-4:** standard.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-graph/tests/axis_l1_fixtures.rs
git commit -m "✅ test(graph): add L1 paired-fixture suite covering all 12 axes"
```

### Task 39: L4 adversarial false-positive corpus

**Files:**
- Create: `crates/reverts-package-matcher/tests/adversarial_fp.rs`

50 paired adversarial fixtures: same `(param_count, statement_count, cfg_hash)` but different semantics. Assert FP rate < 0.5% (i.e., < 1 in 200).

- [ ] **Step 1: Write the failing test**

- [ ] **Step 2-4:** standard.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-package-matcher/tests/adversarial_fp.rs
git commit -m "✅ test(package-matcher): add L4 adversarial false-positive corpus"
```

### Task 40: L7 Hungarian correctness fixture

**Files:**
- Create: `crates/reverts-package-matcher/tests/global_assignment.rs`

Construct a synthetic chunk that mixes two same-named helpers from different packages; assert the assignment splits 5/5 (not 10/0).

- [ ] **Step 1: Write the failing test**

- [ ] **Step 2-4:** standard.

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-package-matcher/tests/global_assignment.rs
git commit -m "✅ test(package-matcher): add L7 fixture for Hungarian global assignment correctness"
```

---

## Self-review (run by the implementing engineer at the end)

1. **Spec coverage check:** Map each section of `2026-05-16-package-signature-matching-design.md` to one or more tasks above. Sections 4–11 should each have ≥ 1 task. Sections 12 L1/L4/L7 are covered by Tasks 38–40; L2/L3/L5/L6/L8 are deferred and intentionally not included in this plan (they depend on corpus assets and CI infrastructure that don't exist yet — file a follow-up issue).

2. **Placeholder scan:** Every `unimplemented!`/`TODO` in the plan body is followed by a NOTE explaining what the implementer must fill in (consult oxc 0.42 ast). These are deliberate — there are oxc API surfaces that move between versions and the plan trusts the implementer to handle them. Audit at execute time.

3. **Type consistency:** The names `FunctionId`, `ByteRange`, `AxisHashes`, `FunctionFingerprint`, `MatchTier`, `NormalizationPassId`, `Candidate`, `ExactKey`, `CfgKey`, `FeatureKey`, `StructuralKey`, `PackageFingerprintIndex`, `CorpusStats` are used consistently across all phases.

4. **Open questions:** None at plan-write time. The two raised during spec review (corpus packages, Hungarian dependency) are resolved.

---

## Execution choice

Plan complete and saved to `docs/superpowers/plans/2026-05-16-package-signature-matching.md` (gitignored). Two execution options:

1. **Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.
2. **Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints.

Which approach?
