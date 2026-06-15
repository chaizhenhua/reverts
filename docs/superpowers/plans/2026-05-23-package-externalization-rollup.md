# Package Externalization Rollup — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Raise the share of `module_category='package'` modules that emit as external imports from 38% to ≥90% on the `~/.reverts/.reverts.db` dataset.

**Architecture:** Two phases.
1. **Phase A (validation, no schema change):** A read-only diagnostic that loads attributions + hints out of the DB, applies the rollup oracle from the design doc, and reports the projected acceptance ratio per package and globally. Gate: must measure ≥90% projected acceptance before phase B begins.
2. **Phase B (production wiring, gated on A):** Extend the attribution schema, add the new acceptance branch to the rejection path in `reverts-cli/src/lib.rs`, and teach the emitter to dissolve internal modules of externalized packages while rewriting consumers to the package's public surface.

**Tech Stack:** Rust 1.93.0 edition 2024, `rusqlite` (read-only), `serde_json`, existing crates `reverts-input`, `reverts-package`, `reverts-cli`.

**Spec:** `docs/superpowers/specs/2026-05-23-package-externalization-rollup-design.md`

---

## Phase A — Rollup Projection Tool

A new binary `reverts-rollup-probe` lives in `crates/reverts-analyze/`. It reads `~/.reverts/.reverts.db` (or a path passed via CLI), reconstructs the "could this module be rolled up?" decision purely from existing rows, and prints a funnel. No DB writes.

### File Structure

| File | Responsibility |
|---|---|
| `crates/reverts-analyze/Cargo.toml` | add `rusqlite`, `serde_json`, `clap`; declare `[[bin]] reverts-rollup-probe` |
| `crates/reverts-analyze/src/rollup/mod.rs` | crate-internal module wiring |
| `crates/reverts-analyze/src/rollup/db.rs` | read attributions, hints, module dep edges, evidence — pure reads, returns plain structs |
| `crates/reverts-analyze/src/rollup/oracle.rs` | the externalizability oracle: per `(package_name, package_version)` → `Externalizable {top_specifier, public_members}` or `NotExternalizable {reason}` |
| `crates/reverts-analyze/src/rollup/projection.rs` | apply oracle to each currently-rejected attribution → `Rolled(top_specifier)` \| `Dissolved` \| `StillRejected(reason)` |
| `crates/reverts-analyze/src/rollup/report.rs` | aggregate counts; render text + JSON funnel |
| `crates/reverts-analyze/src/bin/reverts-rollup-probe.rs` | CLI entry: parse `--db PATH --json OUT?`, call modules, print report, exit non-zero if projected acceptance < 0.90 |
| `crates/reverts-analyze/tests/rollup_fixture.rs` | integration test driven by a small inline SQLite DB |
| `crates/reverts-analyze/tests/rollup_real_db.rs` | optional acceptance test gated on `REVERTS_DB` env var; asserts ≥90% on the real DB |

### Task A1: Cargo wiring

**Files:**
- Modify: `crates/reverts-analyze/Cargo.toml`

- [ ] **Step 1: Update Cargo.toml**

Append after the existing `[dependencies]` block:

```toml
[dependencies]
reverts-graph = { path = "../reverts-graph" }
reverts-input = { path = "../reverts-input" }
reverts-ir = { path = "../reverts-ir" }
reverts-js = { path = "../reverts-js" }
reverts-model = { path = "../reverts-model" }
reverts-observe = { path = "../reverts-observe" }
reverts-package = { path = "../reverts-package" }
rusqlite = { version = "0.32", features = ["bundled"] }
serde_json = "1"
clap = { version = "4", features = ["derive"] }
anyhow = "1"

[[bin]]
name = "reverts-rollup-probe"
path = "src/bin/reverts-rollup-probe.rs"
```

(Leave existing `[lints]` and `[package]` sections untouched. If `rusqlite`/`serde_json`/`clap`/`anyhow` versions are already pinned in `Cargo.toml` workspace `[workspace.dependencies]`, switch to `*.workspace = true` instead.)

- [ ] **Step 2: Verify it builds**

Run: `cargo check -p reverts-analyze --locked`
Expected: success (no source files yet — only Cargo.toml changed).

- [ ] **Step 3: Commit**

```bash
git add crates/reverts-analyze/Cargo.toml
git commit -m "📦 build(analyze): add rollup-probe binary dependencies"
```

### Task A2: DB reader skeleton + test fixture

**Files:**
- Create: `crates/reverts-analyze/src/rollup/mod.rs`
- Create: `crates/reverts-analyze/src/rollup/db.rs`
- Create: `crates/reverts-analyze/tests/rollup_fixture.rs`
- Modify: `crates/reverts-analyze/src/lib.rs` (add `pub mod rollup;` at top)

- [ ] **Step 1: Write the failing test**

Create `crates/reverts-analyze/tests/rollup_fixture.rs`:

```rust
use reverts_analyze::rollup::db::{load_snapshot, Snapshot};
use rusqlite::Connection;

fn seed() -> Connection {
    let conn = Connection::open_in_memory().expect("open");
    conn.execute_batch(
        r#"
        CREATE TABLE modules (
            id INTEGER PRIMARY KEY,
            module_category TEXT NOT NULL DEFAULT 'unknown',
            package_name TEXT,
            package_version TEXT
        );
        CREATE TABLE package_attributions (
            id INTEGER PRIMARY KEY,
            module_id INTEGER NOT NULL,
            package_name TEXT NOT NULL,
            package_version TEXT,
            export_specifier TEXT,
            emission_mode TEXT NOT NULL,
            status TEXT NOT NULL,
            evidence_json TEXT,
            rejection_reason TEXT
        );
        CREATE TABLE package_externalization_hints (
            package_name TEXT NOT NULL,
            package_version TEXT NOT NULL,
            entry_path TEXT NOT NULL,
            export_specifier TEXT NOT NULL,
            public_members_json TEXT NOT NULL DEFAULT '[]',
            PRIMARY KEY (package_name, package_version, entry_path, export_specifier)
        );
        INSERT INTO modules(id, module_category, package_name, package_version) VALUES
            (1, 'package', 'lodash', '4.2.0'),
            (2, 'package', 'lodash', '4.2.0'),
            (3, 'package', 'lodash', '4.2.0');
        INSERT INTO package_attributions(module_id, package_name, package_version, export_specifier, emission_mode, status, evidence_json) VALUES
            (1, 'lodash', '4.2.0', 'lodash/property.js', 'external_import', 'accepted',
             '{"external_import_proof":"matched_package_source"}'),
            (2, 'lodash', '4.2.0', NULL, 'application_source', 'rejected',
             '{"matcher":"package_ownership_matcher","ownership_match":{"match_strategy":"dependency_closure_ownership","external_importable":false}}'),
            (3, 'lodash', '4.2.0', NULL, 'application_source', 'rejected',
             '{"matcher":"package_ownership_matcher","ownership_match":{"match_strategy":"dependency_closure_ownership","external_importable":false}}');
        INSERT INTO package_externalization_hints(package_name, package_version, entry_path, export_specifier, public_members_json) VALUES
            ('lodash', '4.2.0', 'lodash.js', 'lodash', '["property","get","set"]');
        "#,
    ).expect("seed");
    conn
}

#[test]
fn load_snapshot_returns_three_modules_and_one_hint() {
    let conn = seed();
    let snap: Snapshot = load_snapshot(&conn).expect("load");
    assert_eq!(snap.modules.len(), 3);
    assert_eq!(snap.attributions.len(), 3);
    assert_eq!(snap.hints.len(), 1);
}
```

- [ ] **Step 2: Run test to verify failure**

Run: `cargo test -p reverts-analyze --locked --test rollup_fixture -- --nocapture`
Expected: FAIL — `rollup` module not found.

- [ ] **Step 3: Add module declaration to lib.rs**

In `crates/reverts-analyze/src/lib.rs`, add as the very first non-comment line:

```rust
pub mod rollup;
```

- [ ] **Step 4: Create rollup/mod.rs**

Write `crates/reverts-analyze/src/rollup/mod.rs`:

```rust
pub mod db;
```

- [ ] **Step 5: Create db.rs with the loader**

Write `crates/reverts-analyze/src/rollup/db.rs`:

```rust
use anyhow::{Context, Result};
use rusqlite::Connection;

#[derive(Debug, Clone)]
pub struct ModuleRow {
    pub id: i64,
    pub category: String,
    pub package_name: Option<String>,
    pub package_version: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AttributionRow {
    pub module_id: i64,
    pub package_name: String,
    pub package_version: Option<String>,
    pub export_specifier: Option<String>,
    pub emission_mode: String,
    pub status: String,
    pub evidence_json: Option<String>,
    pub rejection_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HintRow {
    pub package_name: String,
    pub package_version: String,
    pub export_specifier: String,
    pub public_members: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub modules: Vec<ModuleRow>,
    pub attributions: Vec<AttributionRow>,
    pub hints: Vec<HintRow>,
}

pub fn load_snapshot(conn: &Connection) -> Result<Snapshot> {
    let modules = conn
        .prepare("SELECT id, module_category, package_name, package_version FROM modules")?
        .query_map([], |row| {
            Ok(ModuleRow {
                id: row.get(0)?,
                category: row.get(1)?,
                package_name: row.get(2)?,
                package_version: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
        .context("read modules")?;

    let attributions = conn
        .prepare(
            "SELECT module_id, package_name, package_version, export_specifier, emission_mode, status, evidence_json, rejection_reason FROM package_attributions",
        )?
        .query_map([], |row| {
            Ok(AttributionRow {
                module_id: row.get(0)?,
                package_name: row.get(1)?,
                package_version: row.get(2)?,
                export_specifier: row.get(3)?,
                emission_mode: row.get(4)?,
                status: row.get(5)?,
                evidence_json: row.get(6)?,
                rejection_reason: row.get(7)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
        .context("read package_attributions")?;

    let hints = conn
        .prepare(
            "SELECT package_name, package_version, export_specifier, public_members_json FROM package_externalization_hints",
        )?
        .query_map([], |row| {
            let members_json: String = row.get(3)?;
            let public_members: Vec<String> =
                serde_json::from_str(&members_json).unwrap_or_default();
            Ok(HintRow {
                package_name: row.get(0)?,
                package_version: row.get(1)?,
                export_specifier: row.get(2)?,
                public_members,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
        .context("read package_externalization_hints")?;

    Ok(Snapshot {
        modules,
        attributions,
        hints,
    })
}
```

- [ ] **Step 6: Run test to verify it passes**

Run: `cargo test -p reverts-analyze --locked --test rollup_fixture -- --nocapture`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/reverts-analyze/src/lib.rs crates/reverts-analyze/src/rollup/mod.rs crates/reverts-analyze/src/rollup/db.rs crates/reverts-analyze/tests/rollup_fixture.rs
git commit -m "🧭 feat(analyze): add rollup snapshot loader"
```

### Task A3: Externalizability oracle

**Files:**
- Create: `crates/reverts-analyze/src/rollup/oracle.rs`
- Modify: `crates/reverts-analyze/src/rollup/mod.rs` (add `pub mod oracle;`)
- Modify: `crates/reverts-analyze/tests/rollup_fixture.rs` (append oracle test)

- [ ] **Step 1: Add the failing oracle test**

Append to `crates/reverts-analyze/tests/rollup_fixture.rs`:

```rust
use reverts_analyze::rollup::oracle::{build_oracle, OracleVerdict};

#[test]
fn oracle_marks_lodash_externalizable_when_one_accepted_and_hint_present() {
    let conn = seed();
    let snap = load_snapshot(&conn).expect("load");
    let oracle = build_oracle(&snap, OracleConfig { direct_match_floor: 0.30 });
    let verdict = oracle
        .lookup("lodash", "4.2.0")
        .expect("lodash should appear");
    match verdict {
        OracleVerdict::Externalizable { top_specifier, public_members } => {
            assert_eq!(top_specifier, "lodash");
            assert_eq!(public_members, &["property", "get", "set"]);
        }
        OracleVerdict::NotExternalizable { reason } => panic!("expected ext, got {reason}"),
    }
}

use reverts_analyze::rollup::oracle::OracleConfig;
```

(Re-order imports in the test file as needed.)

- [ ] **Step 2: Run test to verify failure**

Run: `cargo test -p reverts-analyze --locked --test rollup_fixture -- --nocapture`
Expected: FAIL — `oracle` not in scope.

- [ ] **Step 3: Implement oracle**

Write `crates/reverts-analyze/src/rollup/oracle.rs`:

```rust
use std::collections::BTreeMap;

use crate::rollup::db::{AttributionRow, HintRow, ModuleRow, Snapshot};

#[derive(Debug, Clone, Copy)]
pub struct OracleConfig {
    pub direct_match_floor: f64,
}

impl Default for OracleConfig {
    fn default() -> Self {
        Self {
            direct_match_floor: 0.30,
        }
    }
}

#[derive(Debug, Clone)]
pub enum OracleVerdict {
    Externalizable {
        top_specifier: String,
        public_members: Vec<String>,
    },
    NotExternalizable {
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct Oracle {
    table: BTreeMap<(String, String), OracleVerdict>,
}

impl Oracle {
    pub fn lookup(&self, name: &str, version: &str) -> Option<&OracleVerdict> {
        self.table.get(&(name.to_string(), version.to_string()))
    }
    pub fn iter(&self) -> impl Iterator<Item = (&(String, String), &OracleVerdict)> {
        self.table.iter()
    }
}

pub fn build_oracle(snap: &Snapshot, cfg: OracleConfig) -> Oracle {
    let mut by_pkg_version: BTreeMap<(String, String), Vec<&ModuleRow>> = BTreeMap::new();
    for module in &snap.modules {
        if module.category != "package" {
            continue;
        }
        let (Some(name), Some(version)) = (&module.package_name, &module.package_version) else {
            continue;
        };
        by_pkg_version
            .entry((name.clone(), version.clone()))
            .or_default()
            .push(module);
    }

    let mut accepted_direct_by_pkg: BTreeMap<(String, String), usize> = BTreeMap::new();
    for attribution in &snap.attributions {
        if attribution.status != "accepted" || attribution.emission_mode != "external_import" {
            continue;
        }
        let Some(version) = &attribution.package_version else {
            continue;
        };
        if !is_direct_subpath_proof(attribution) {
            continue;
        }
        *accepted_direct_by_pkg
            .entry((attribution.package_name.clone(), version.clone()))
            .or_default() += 1;
    }

    let hint_index = build_hint_index(&snap.hints);

    let mut table = BTreeMap::new();
    for ((name, version), modules) in by_pkg_version {
        let total = modules.len();
        let accepted = accepted_direct_by_pkg
            .get(&(name.clone(), version.clone()))
            .copied()
            .unwrap_or(0);
        let key = (name.clone(), version.clone());
        let verdict = if accepted == 0 {
            OracleVerdict::NotExternalizable {
                reason: "no direct-subpath acceptance for this version".into(),
            }
        } else if (accepted as f64) / (total.max(1) as f64) < cfg.direct_match_floor {
            OracleVerdict::NotExternalizable {
                reason: format!(
                    "direct match ratio {}/{} below floor {:.2}",
                    accepted, total, cfg.direct_match_floor
                ),
            }
        } else if let Some(hint) = hint_index.top_level_for(&name, &version) {
            OracleVerdict::Externalizable {
                top_specifier: hint.top_specifier.clone(),
                public_members: hint.public_members.clone(),
            }
        } else {
            OracleVerdict::NotExternalizable {
                reason: "no top-level externalization hint".into(),
            }
        };
        table.insert(key, verdict);
    }

    Oracle { table }
}

fn is_direct_subpath_proof(attribution: &AttributionRow) -> bool {
    let Some(spec) = &attribution.export_specifier else {
        return false;
    };
    // A direct-subpath attribution has a non-empty export specifier and
    // its evidence claims a matched_package_source proof.
    if spec.trim().is_empty() {
        return false;
    }
    let Some(json) = &attribution.evidence_json else {
        return false;
    };
    json.contains("\"external_import_proof\":\"matched_package_source\"")
}

struct HintIndex {
    top_level: BTreeMap<(String, String), TopLevel>,
}

#[derive(Clone)]
struct TopLevel {
    top_specifier: String,
    public_members: Vec<String>,
}

impl HintIndex {
    fn top_level_for(&self, name: &str, version: &str) -> Option<&TopLevel> {
        self.top_level.get(&(name.to_string(), version.to_string()))
    }
}

fn build_hint_index(rows: &[HintRow]) -> HintIndex {
    let mut top_level: BTreeMap<(String, String), TopLevel> = BTreeMap::new();
    for row in rows {
        let is_top = row.export_specifier == row.package_name;
        if !is_top {
            continue;
        }
        top_level.insert(
            (row.package_name.clone(), row.package_version.clone()),
            TopLevel {
                top_specifier: row.export_specifier.clone(),
                public_members: row.public_members.clone(),
            },
        );
    }
    HintIndex { top_level }
}
```

- [ ] **Step 4: Wire module**

Update `crates/reverts-analyze/src/rollup/mod.rs`:

```rust
pub mod db;
pub mod oracle;
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p reverts-analyze --locked --test rollup_fixture -- --nocapture`
Expected: PASS.

- [ ] **Step 6: Add negative test (no hint → not externalizable)**

Append to `crates/reverts-analyze/tests/rollup_fixture.rs`:

```rust
#[test]
fn oracle_rejects_when_hint_missing() {
    let conn = rusqlite::Connection::open_in_memory().expect("open");
    conn.execute_batch(r#"
        CREATE TABLE modules (id INTEGER PRIMARY KEY, module_category TEXT, package_name TEXT, package_version TEXT);
        CREATE TABLE package_attributions (
            id INTEGER PRIMARY KEY, module_id INTEGER, package_name TEXT NOT NULL,
            package_version TEXT, export_specifier TEXT, emission_mode TEXT NOT NULL,
            status TEXT NOT NULL, evidence_json TEXT, rejection_reason TEXT
        );
        CREATE TABLE package_externalization_hints (
            package_name TEXT NOT NULL, package_version TEXT NOT NULL,
            entry_path TEXT NOT NULL, export_specifier TEXT NOT NULL,
            public_members_json TEXT NOT NULL DEFAULT '[]',
            PRIMARY KEY (package_name, package_version, entry_path, export_specifier));
        INSERT INTO modules VALUES (1,'package','rare','1.0.0'),(2,'package','rare','1.0.0'),(3,'package','rare','1.0.0'),(4,'package','rare','1.0.0');
        INSERT INTO package_attributions(module_id, package_name, package_version, export_specifier, emission_mode, status, evidence_json) VALUES
            (1,'rare','1.0.0','rare/a.js','external_import','accepted','{"external_import_proof":"matched_package_source"}'),
            (2,'rare','1.0.0','rare/b.js','external_import','accepted','{"external_import_proof":"matched_package_source"}');
    "#).unwrap();
    let snap = load_snapshot(&conn).unwrap();
    let oracle = build_oracle(&snap, OracleConfig::default());
    let verdict = oracle.lookup("rare", "1.0.0").expect("present");
    assert!(matches!(verdict, OracleVerdict::NotExternalizable { .. }));
}
```

- [ ] **Step 7: Run all tests**

Run: `cargo test -p reverts-analyze --locked --test rollup_fixture -- --nocapture`
Expected: PASS (both tests).

- [ ] **Step 8: Commit**

```bash
git add crates/reverts-analyze/src/rollup/mod.rs crates/reverts-analyze/src/rollup/oracle.rs crates/reverts-analyze/tests/rollup_fixture.rs
git commit -m "🧭 feat(analyze): add package externalizability oracle"
```

### Task A4: Projection per attribution

**Files:**
- Create: `crates/reverts-analyze/src/rollup/projection.rs`
- Modify: `crates/reverts-analyze/src/rollup/mod.rs`
- Modify: `crates/reverts-analyze/tests/rollup_fixture.rs`

- [ ] **Step 1: Add failing projection test**

Append to `tests/rollup_fixture.rs`:

```rust
use reverts_analyze::rollup::projection::{project, Projection, ProjectionKind};

#[test]
fn projection_promotes_closure_modules_when_pkg_externalizable() {
    let conn = seed();
    let snap = load_snapshot(&conn).unwrap();
    let oracle = build_oracle(&snap, OracleConfig::default());
    let projections: Vec<Projection> = project(&snap, &oracle);

    let by_module: std::collections::BTreeMap<i64, &Projection> =
        projections.iter().map(|p| (p.module_id, p)).collect();

    // module 1 is already accepted with subpath
    assert!(matches!(by_module[&1].kind, ProjectionKind::AlreadyAccepted));
    // modules 2 and 3 are rejected closure-owned ⇒ promoted to top-level rollup
    assert!(matches!(by_module[&2].kind, ProjectionKind::RolledUp { ref top_specifier } if top_specifier == "lodash"));
    assert!(matches!(by_module[&3].kind, ProjectionKind::RolledUp { ref top_specifier } if top_specifier == "lodash"));
}
```

- [ ] **Step 2: Run to verify fail**

Run: `cargo test -p reverts-analyze --locked --test rollup_fixture`
Expected: FAIL — `projection` missing.

- [ ] **Step 3: Implement projection**

Write `crates/reverts-analyze/src/rollup/projection.rs`:

```rust
use crate::rollup::db::{AttributionRow, Snapshot};
use crate::rollup::oracle::{Oracle, OracleVerdict};

#[derive(Debug, Clone)]
pub enum ProjectionKind {
    AlreadyAccepted,
    RolledUp { top_specifier: String },
    StillRejected { reason: String },
    Untouched,
}

#[derive(Debug, Clone)]
pub struct Projection {
    pub module_id: i64,
    pub package_name: String,
    pub package_version: Option<String>,
    pub kind: ProjectionKind,
}

pub fn project(snap: &Snapshot, oracle: &Oracle) -> Vec<Projection> {
    snap.attributions
        .iter()
        .map(|attribution| project_one(attribution, oracle))
        .collect()
}

fn project_one(attribution: &AttributionRow, oracle: &Oracle) -> Projection {
    let kind = if attribution.status == "accepted"
        && attribution.emission_mode == "external_import"
    {
        ProjectionKind::AlreadyAccepted
    } else if attribution.status == "rejected" && is_closure_ownership_rejection(attribution) {
        match attribution.package_version.as_deref() {
            Some(version) => match oracle.lookup(&attribution.package_name, version) {
                Some(OracleVerdict::Externalizable { top_specifier, .. }) => {
                    ProjectionKind::RolledUp {
                        top_specifier: top_specifier.clone(),
                    }
                }
                Some(OracleVerdict::NotExternalizable { reason }) => {
                    ProjectionKind::StillRejected {
                        reason: reason.clone(),
                    }
                }
                None => ProjectionKind::StillRejected {
                    reason: "package version not in oracle".into(),
                },
            },
            None => ProjectionKind::StillRejected {
                reason: "rejected attribution has no package_version".into(),
            },
        }
    } else {
        ProjectionKind::Untouched
    };

    Projection {
        module_id: attribution.module_id,
        package_name: attribution.package_name.clone(),
        package_version: attribution.package_version.clone(),
        kind,
    }
}

fn is_closure_ownership_rejection(attribution: &AttributionRow) -> bool {
    let Some(json) = &attribution.evidence_json else {
        return false;
    };
    json.contains("\"match_strategy\":\"dependency_closure_ownership\"")
        && json.contains("\"external_importable\":false")
}
```

- [ ] **Step 4: Register module**

Update `src/rollup/mod.rs`:

```rust
pub mod db;
pub mod oracle;
pub mod projection;
```

- [ ] **Step 5: Run all tests**

Run: `cargo test -p reverts-analyze --locked --test rollup_fixture`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/reverts-analyze/src/rollup/mod.rs crates/reverts-analyze/src/rollup/projection.rs crates/reverts-analyze/tests/rollup_fixture.rs
git commit -m "🧭 feat(analyze): project rolled-up attributions per module"
```

### Task A5: Aggregate report

**Files:**
- Create: `crates/reverts-analyze/src/rollup/report.rs`
- Modify: `crates/reverts-analyze/src/rollup/mod.rs`
- Modify: `crates/reverts-analyze/tests/rollup_fixture.rs`

- [ ] **Step 1: Failing report test**

Append to `tests/rollup_fixture.rs`:

```rust
use reverts_analyze::rollup::report::{summarize, RollupReport};

#[test]
fn report_aggregates_global_and_per_package() {
    let conn = seed();
    let snap = load_snapshot(&conn).unwrap();
    let oracle = build_oracle(&snap, OracleConfig::default());
    let projections = project(&snap, &oracle);
    let report: RollupReport = summarize(&snap, &projections);
    // 3 package modules, 1 already accepted, 2 rolled up ⇒ projected = 3/3 = 100%
    assert_eq!(report.global.package_modules, 3);
    assert_eq!(report.global.already_accepted, 1);
    assert_eq!(report.global.rolled_up, 2);
    assert!((report.global.projected_ratio - 1.0).abs() < 1e-9);
    let lodash = report.per_package.iter().find(|p| p.package_name == "lodash").unwrap();
    assert_eq!(lodash.projected_external, 3);
}
```

- [ ] **Step 2: Fail it**

Run: `cargo test -p reverts-analyze --locked --test rollup_fixture`
Expected: FAIL.

- [ ] **Step 3: Implement report**

Write `crates/reverts-analyze/src/rollup/report.rs`:

```rust
use std::collections::BTreeMap;

use serde::Serialize;

use crate::rollup::db::Snapshot;
use crate::rollup::projection::{Projection, ProjectionKind};

#[derive(Debug, Clone, Serialize, Default)]
pub struct GlobalCounts {
    pub package_modules: usize,
    pub already_accepted: usize,
    pub rolled_up: usize,
    pub still_rejected: usize,
    pub projected_external: usize,
    pub projected_ratio: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackageCounts {
    pub package_name: String,
    pub package_modules: usize,
    pub already_accepted: usize,
    pub rolled_up: usize,
    pub still_rejected: usize,
    pub projected_external: usize,
    pub projected_ratio: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RollupReport {
    pub global: GlobalCounts,
    pub per_package: Vec<PackageCounts>,
}

pub fn summarize(snap: &Snapshot, projections: &[Projection]) -> RollupReport {
    let package_module_ids: std::collections::BTreeSet<i64> = snap
        .modules
        .iter()
        .filter(|m| m.category == "package")
        .map(|m| m.id)
        .collect();

    let mut per_pkg_modules: BTreeMap<String, usize> = BTreeMap::new();
    for m in &snap.modules {
        if m.category != "package" {
            continue;
        }
        if let Some(name) = &m.package_name {
            *per_pkg_modules.entry(name.clone()).or_default() += 1;
        }
    }

    let mut per_pkg_accepted: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_pkg_rolled: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_pkg_still: BTreeMap<String, usize> = BTreeMap::new();
    let mut global = GlobalCounts::default();
    global.package_modules = package_module_ids.len();

    for proj in projections {
        if !package_module_ids.contains(&proj.module_id) {
            continue;
        }
        match &proj.kind {
            ProjectionKind::AlreadyAccepted => {
                global.already_accepted += 1;
                *per_pkg_accepted.entry(proj.package_name.clone()).or_default() += 1;
            }
            ProjectionKind::RolledUp { .. } => {
                global.rolled_up += 1;
                *per_pkg_rolled.entry(proj.package_name.clone()).or_default() += 1;
            }
            ProjectionKind::StillRejected { .. } => {
                global.still_rejected += 1;
                *per_pkg_still.entry(proj.package_name.clone()).or_default() += 1;
            }
            ProjectionKind::Untouched => {}
        }
    }

    global.projected_external = global.already_accepted + global.rolled_up;
    global.projected_ratio = if global.package_modules == 0 {
        0.0
    } else {
        global.projected_external as f64 / global.package_modules as f64
    };

    let mut per_package: Vec<PackageCounts> = per_pkg_modules
        .into_iter()
        .map(|(name, total)| {
            let acc = per_pkg_accepted.get(&name).copied().unwrap_or(0);
            let roll = per_pkg_rolled.get(&name).copied().unwrap_or(0);
            let still = per_pkg_still.get(&name).copied().unwrap_or(0);
            let projected = acc + roll;
            let ratio = if total == 0 {
                0.0
            } else {
                projected as f64 / total as f64
            };
            PackageCounts {
                package_name: name,
                package_modules: total,
                already_accepted: acc,
                rolled_up: roll,
                still_rejected: still,
                projected_external: projected,
                projected_ratio: ratio,
            }
        })
        .collect();
    per_package.sort_by(|a, b| b.package_modules.cmp(&a.package_modules));
    RollupReport {
        global,
        per_package,
    }
}
```

- [ ] **Step 4: Add `serde` dep**

Append to `[dependencies]` in `crates/reverts-analyze/Cargo.toml`:

```toml
serde = { version = "1", features = ["derive"] }
```

(Skip if already workspace-pinned; use `serde.workspace = true` instead.)

- [ ] **Step 5: Wire module**

Update `src/rollup/mod.rs`:

```rust
pub mod db;
pub mod oracle;
pub mod projection;
pub mod report;
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p reverts-analyze --locked --test rollup_fixture`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/reverts-analyze/Cargo.toml crates/reverts-analyze/src/rollup/mod.rs crates/reverts-analyze/src/rollup/report.rs crates/reverts-analyze/tests/rollup_fixture.rs
git commit -m "🧭 feat(analyze): aggregate rollup projection report"
```

### Task A6: CLI binary

**Files:**
- Create: `crates/reverts-analyze/src/bin/reverts-rollup-probe.rs`

- [ ] **Step 1: Write CLI**

Create `crates/reverts-analyze/src/bin/reverts-rollup-probe.rs`:

```rust
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use reverts_analyze::rollup::db::load_snapshot;
use reverts_analyze::rollup::oracle::{build_oracle, OracleConfig};
use reverts_analyze::rollup::projection::project;
use reverts_analyze::rollup::report::summarize;
use rusqlite::Connection;

#[derive(Parser)]
#[command(version, about = "Project rollup-promoted external import ratio against a reverts DB")]
struct Cli {
    /// SQLite path. Defaults to ~/.reverts/.reverts.db.
    #[arg(long)]
    db: Option<PathBuf>,
    /// Write the full JSON report to this path.
    #[arg(long)]
    json: Option<PathBuf>,
    /// Required projected ratio. Process exits non-zero below this floor.
    #[arg(long, default_value_t = 0.90)]
    target_ratio: f64,
    /// Direct-match ratio threshold for the oracle.
    #[arg(long, default_value_t = 0.30)]
    direct_match_floor: f64,
}

fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    let db_path = cli.db.unwrap_or_else(default_db_path);
    let conn = Connection::open(&db_path)
        .with_context(|| format!("open {}", db_path.display()))?;
    let snap = load_snapshot(&conn)?;
    let oracle = build_oracle(
        &snap,
        OracleConfig {
            direct_match_floor: cli.direct_match_floor,
        },
    );
    let projections = project(&snap, &oracle);
    let report = summarize(&snap, &projections);

    println!("# Rollup projection ({})", db_path.display());
    println!(
        "package modules: {}  already_accepted: {}  rolled_up: {}  still_rejected: {}",
        report.global.package_modules,
        report.global.already_accepted,
        report.global.rolled_up,
        report.global.still_rejected,
    );
    println!(
        "projected external import ratio: {:.4} (target {:.2})",
        report.global.projected_ratio, cli.target_ratio,
    );
    println!();
    println!("# Top packages by module count");
    for pkg in report.per_package.iter().take(40) {
        println!(
            "  {:>5}  {:>5}  {:>5}  {:>5}  {:>6.1}%  {}",
            pkg.package_modules,
            pkg.already_accepted,
            pkg.rolled_up,
            pkg.still_rejected,
            pkg.projected_ratio * 100.0,
            pkg.package_name,
        );
    }
    if let Some(path) = cli.json {
        fs::write(&path, serde_json::to_string_pretty(&report)?)
            .with_context(|| format!("write {}", path.display()))?;
    }

    if report.global.projected_ratio + 1e-9 < cli.target_ratio {
        eprintln!(
            "projected ratio {:.4} below target {:.2}",
            report.global.projected_ratio, cli.target_ratio
        );
        return Ok(ExitCode::from(2));
    }
    Ok(ExitCode::SUCCESS)
}

fn default_db_path() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        let mut p = PathBuf::from(home);
        p.push(".reverts");
        p.push(".reverts.db");
        return p;
    }
    PathBuf::from(".reverts.db")
}
```

- [ ] **Step 2: Build it**

Run: `cargo build -p reverts-analyze --locked --bin reverts-rollup-probe`
Expected: success.

- [ ] **Step 3: Smoke-run against the real DB**

Run: `./target/debug/reverts-rollup-probe --db ~/.reverts/.reverts.db --target-ratio 0.0`
Expected: prints a funnel report. Capture the printed `projected external import ratio`.

- [ ] **Step 4: Save the JSON for later comparison**

Run: `./target/debug/reverts-rollup-probe --db ~/.reverts/.reverts.db --target-ratio 0.0 --json target/rollup-probe-initial.json`

- [ ] **Step 5: Commit**

```bash
git add crates/reverts-analyze/src/bin/reverts-rollup-probe.rs
git commit -m "🧭 feat(analyze): rollup-probe CLI binary"
```

### Task A7: Real-DB acceptance test

**Files:**
- Create: `crates/reverts-analyze/tests/rollup_real_db.rs`

- [ ] **Step 1: Add acceptance test**

Create `crates/reverts-analyze/tests/rollup_real_db.rs`:

```rust
use std::path::PathBuf;

use reverts_analyze::rollup::db::load_snapshot;
use reverts_analyze::rollup::oracle::{build_oracle, OracleConfig};
use reverts_analyze::rollup::projection::project;
use reverts_analyze::rollup::report::summarize;
use rusqlite::Connection;

#[test]
fn projected_ratio_meets_target_on_real_db() {
    let path = match std::env::var_os("REVERTS_DB") {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("REVERTS_DB not set; skipping");
            return;
        }
    };
    if !path.exists() {
        eprintln!("{} missing; skipping", path.display());
        return;
    }
    let conn = Connection::open(&path).expect("open");
    let snap = load_snapshot(&conn).expect("load");
    let oracle = build_oracle(&snap, OracleConfig::default());
    let projections = project(&snap, &oracle);
    let report = summarize(&snap, &projections);

    eprintln!(
        "projected={:.4} package_modules={} accepted={} rolled_up={} still_rejected={}",
        report.global.projected_ratio,
        report.global.package_modules,
        report.global.already_accepted,
        report.global.rolled_up,
        report.global.still_rejected,
    );
    assert!(
        report.global.projected_ratio >= 0.90,
        "rollup projection {} below 0.90; tighten oracle or expand strategy",
        report.global.projected_ratio
    );
}
```

- [ ] **Step 2: Run against real DB**

Run: `REVERTS_DB=$HOME/.reverts/.reverts.db cargo test -p reverts-analyze --locked --test rollup_real_db -- --nocapture`
Expected: PASS. If it fails, the recorded eprintln tells you the actual ratio — that is the input for the next iteration of the oracle. **Do not relax the assertion in this test.** Either change the oracle / projection logic to raise the ratio, or accept that the rollup heuristic needs broadening (skip to Task A8).

- [ ] **Step 3: Commit**

```bash
git add crates/reverts-analyze/tests/rollup_real_db.rs
git commit -m "🧪 test(analyze): assert rollup projection hits 90% on real db"
```

### Task A8: Iterate the oracle if A7 misses

This task is **only** run if A7's first execution falls below 90%. Skip otherwise.

- [ ] **Step 1: Inspect which packages contribute the largest "still_rejected" buckets**

Use the JSON written in A6 step 4: `jq '.per_package | sort_by(.still_rejected) | reverse | .[:20]' target/rollup-probe-initial.json`. Note the top 20 packages with the most still-rejected modules.

- [ ] **Step 2: Determine the limiting factor**

For each top still-rejected package, query the DB:

```bash
sqlite3 ~/.reverts/.reverts.db "
SELECT package_version,
  SUM(CASE WHEN status='accepted' AND emission_mode='external_import' THEN 1 ELSE 0 END) accepted,
  SUM(CASE WHEN status='rejected' THEN 1 ELSE 0 END) rejected,
  (SELECT COUNT(*) FROM package_externalization_hints h
   WHERE h.package_name=package_attributions.package_name
     AND h.package_version=package_attributions.package_version
     AND h.export_specifier=package_attributions.package_name) AS top_hints
FROM package_attributions WHERE package_name='<NAME>' GROUP BY package_version;"
```

A package missing the row where `export_specifier = package_name` (the top-level hint) cannot be rolled up by the current oracle. Decide whether to:
(a) Lower `direct_match_floor` (currently 0.30) if the package has many accepted-but-not-30%
(b) Treat any `export_specifier` whose subpath equals the package's main entry (`index.js`, `dist/index.js`) as a top-level hint candidate (extend `HintIndex::build_hint_index` to allow that fallback)
(c) Document the package as out-of-scope for rollup (no externalization data exists for it)

- [ ] **Step 3: Apply the chosen change**

Edit `crates/reverts-analyze/src/rollup/oracle.rs`. Capture the rationale in a one-line comment on the changed branch.

- [ ] **Step 4: Re-run A7**

Run: `REVERTS_DB=$HOME/.reverts/.reverts.db cargo test -p reverts-analyze --locked --test rollup_real_db -- --nocapture`
Expected: ratio increases. Repeat A8 until ≥90% or until you have identified that the design needs revision (in which case stop and return to brainstorming with the new data).

- [ ] **Step 5: Commit each iteration**

```bash
git add crates/reverts-analyze/src/rollup/oracle.rs
git commit -m "🧭 perf(analyze): widen oracle <one-line reason>"
```

### Task A9: Pipeline checks

- [ ] **Step 1: Workspace lint + tests**

Run in parallel where possible:
```bash
cargo fmt --check
cargo clippy --workspace --locked -- -D warnings
cargo test --workspace --locked
```
Expected: all clean.

- [ ] **Step 2: Note Phase A outcome**

Append a one-paragraph entry to `docs/superpowers/specs/2026-05-23-package-externalization-rollup-design.md` under a new "## Phase A outcome (2026-05-23)" section with the final projected ratio, the oracle config that produced it, and the top still-rejected packages.

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/2026-05-23-package-externalization-rollup-design.md
git commit -m "📝 docs(specs): record rollup phase A outcome"
```

---

## Phase B — Production Wiring (gated on Phase A ≥ 90% projection)

**Do not start Phase B until Phase A reports a projected ratio ≥ 0.90.** If the projection fell short, return to brainstorming with the Phase A data instead.

When the gate passes, **invoke the writing-plans skill again** with Phase A's outcome as new context, to produce a detailed Phase B plan covering:

1. Schema migration adding `internal_to_externalized_package` to the `package_attributions.emission_mode` CHECK.
2. Acceptance branch in `crates/reverts-cli/src/lib.rs` at the rejection site near `crates/reverts-cli/src/lib.rs:6738` that consults the oracle and produces accepted rollup attributions instead of rejecting.
3. Emitter changes (`crates/reverts-emitter/src/lib.rs`): suppress source for dissolved modules and rewrite consumer references to use the package's public surface.
4. Audit invariant: no emitted file references a dissolved module id.
5. Real-pipeline integration test that runs the modified CLI against a fixture project and asserts the same ratio improvement observed in Phase A.

Phase B's plan is not pre-written here because its task shapes depend on which oracle/projection rules survived Phase A's iteration. Writing them now would be guesswork that the engineer would have to discard.

---

## Self-review notes

- Spec coverage: §1 problem → A2/A3/A4 read & project; §2 strategy → A3 oracle + A4 projection; §3 components — Phase B; §6 tests — A2/A3/A4/A5/A7; §7 success criterion → A7 (and Phase B integration test, deferred).
- Placeholder scan: none — every step has explicit code or commands.
- Type consistency: `OracleConfig`, `OracleVerdict`, `Projection`, `ProjectionKind`, `Snapshot` names are stable across tasks.
- Single bin name `reverts-rollup-probe` referenced in A1, A6 step 2, A6 step 3, A6 step 4.
