//! `reference-source-names` command: name a decompiled project's modules,
//! exports, and local bindings by matching its emitted TypeScript against a
//! historical first-party source tree. Tier-gated: only provable matches are
//! auto-accepted; everything else is left for an agent.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use clap::{Args, ValueEnum};
use reverts_package_matcher::{SourceFingerprint, fingerprint_source};
use reverts_pipeline::{generate_project_from_prepared, prepare_and_enrich};
use rusqlite::{Connection, params};

use crate::args::{parse_args_with_name, parse_project_id};
use crate::commands::symbol_names::{
    ensure_semantic_name_source_column, ensure_symbol_name_proposals_table,
};
use crate::errors::{CliError, CliRunError};
use crate::input_externalization::load_project_bundle_with_package_externalization;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum MinTier {
    High,
    Medium,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct ReferenceSourceNamesArgs {
    #[arg(long)]
    pub input: PathBuf,
    #[arg(long, value_parser = parse_project_id)]
    pub project_id: u32,
    #[arg(long)]
    pub reference_source_root: PathBuf,
    #[arg(long)]
    pub reference_version: String,
    #[arg(long, default_value_t = false)]
    pub apply: bool,
    #[arg(long, value_enum, default_value_t = MinTier::High)]
    pub min_tier: MinTier,
    #[arg(long, default_value = "source")]
    pub origin_prefix: String,
}

impl ReferenceSourceNamesArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let mut args = args.into_iter().collect::<Vec<_>>();
        if args
            .first()
            .is_some_and(|a| a == crate::help::REFERENCE_SOURCE_NAMES_COMMAND)
        {
            args.remove(0);
        }
        parse_args_with_name(crate::help::REFERENCE_SOURCE_NAMES_COMMAND, args)
    }
}

struct ModulePlan {
    module_id: u32,
    subject_path: String,
    reference_version: String,
    matched: ModuleMatch,
    module_semantic_name: String,
    subject_bindings: Vec<(String, String)>,
    reference_exports: std::collections::BTreeSet<String>,
}

fn plan_modules(args: &ReferenceSourceNamesArgs) -> Result<Vec<ModulePlan>, CliRunError> {
    let index = build_reference_source_index(&args.reference_source_root, &args.reference_version)
        .map_err(CliRunError::ReferenceSourceNames)?;
    let subjects = subject_modules(args)?;
    let mut plans = Vec::new();
    for subject in subjects {
        let Some(matched) = best_module_match(&subject.fingerprint, &index) else {
            continue;
        };
        let reference_exports = index
            .modules
            .iter()
            .find(|m| m.file_path == matched.file_path)
            .map(|m| m.export_names.clone())
            .unwrap_or_default();
        plans.push(ModulePlan {
            module_id: subject.module_id,
            subject_path: subject.file_path,
            reference_version: index.version.clone(),
            module_semantic_name: strip_source_extension(&matched.file_path),
            matched,
            subject_bindings: subject.bindings,
            reference_exports,
        });
    }
    plans.sort_by(|a, b| a.module_id.cmp(&b.module_id));
    Ok(plans)
}

fn strip_source_extension(path: &str) -> String {
    for ext in SOURCE_EXTENSIONS {
        if let Some(stripped) = path.strip_suffix(&format!(".{ext}")) {
            return stripped.to_string();
        }
    }
    path.to_string()
}

fn tier_str(tier: MatchTier) -> &'static str {
    match tier {
        MatchTier::High => "high",
        MatchTier::Medium => "medium",
        MatchTier::Low => "low",
    }
}

pub(crate) fn run(args: ReferenceSourceNamesArgs) -> Result<(), CliRunError> {
    let plans = plan_modules(&args)?;
    println!(
        "module_id\tsubject_path\tref_version\tref_file\ttier\tsemantic_name\tasset\texport\tfn\tanchor"
    );
    for plan in &plans {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            plan.module_id,
            plan.subject_path,
            plan.reference_version,
            plan.matched.file_path,
            tier_str(plan.matched.tier),
            plan.module_semantic_name,
            plan.matched.asset_overlap,
            plan.matched.export_overlap,
            plan.matched.function_overlap,
            plan.matched.anchor_overlap,
        );
    }
    if args.apply {
        let connection = Connection::open(&args.input)
            .map_err(|error| CliRunError::ReferenceSourceNames(error.to_string()))?;
        ensure_semantic_name_source_column(&connection)
            .map_err(|e| CliRunError::ReferenceSourceNames(e.to_string()))?;
        ensure_symbol_name_proposals_table(&connection)
            .map_err(|e| CliRunError::ReferenceSourceNames(e.to_string()))?;
        let module_count = write_module_names(
            &connection,
            &plans,
            args.min_tier,
            &args.origin_prefix,
            &args.reference_version,
        )?;
        let mut export_count = 0;
        for plan in &plans {
            if !tier_passes(plan.matched.tier, args.min_tier) {
                continue;
            }
            let origin = format!(
                "{}:{}:{}",
                args.origin_prefix, args.reference_version, plan.matched.file_path
            );
            export_count += write_export_names(
                &connection,
                args.project_id,
                plan.module_id,
                &plan.reference_exports,
                &plan.subject_bindings,
                &origin,
            )?;
        }
        println!("applied: {module_count} module name(s), {export_count} export name(s) written");
    } else {
        println!(
            "dry-run: {} module match(es); pass --apply to write",
            plans.len()
        );
    }
    Ok(())
}

/// One subject emitted module: its DB module id, emitted path, fingerprint,
/// and the (original_name → emitted_name) bindings that land in it.
struct SubjectModule {
    module_id: u32,
    file_path: String,
    fingerprint: SourceFingerprint,
    bindings: Vec<(String, String)>, // (original_name, emitted_name)
}

fn subject_modules(args: &ReferenceSourceNamesArgs) -> Result<Vec<SubjectModule>, CliRunError> {
    let bundle = load_project_bundle_with_package_externalization(&args.input, args.project_id)
        .map_err(|error| CliRunError::ReferenceSourceNames(format!("load input: {error}")))?;
    let prepared = prepare_and_enrich(bundle)
        .map_err(|error| CliRunError::ReferenceSourceNames(format!("prepare: {error}")))?;
    let run = generate_project_from_prepared(prepared)
        .map_err(|error| CliRunError::ReferenceSourceNames(format!("generate: {error}")))?;

    // Group symbol_index bindings by emitted file path, capturing module id.
    let mut module_for_path: BTreeMap<String, u32> = BTreeMap::new();
    let mut bindings_for_path: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for entry in &run.symbol_index {
        module_for_path
            .entry(entry.file_path.clone())
            .or_insert(entry.module_id.0);
        bindings_for_path
            .entry(entry.file_path.clone())
            .or_default()
            .push((entry.original_name.clone(), entry.emitted_name.clone()));
    }

    let mut modules = Vec::new();
    for file in &run.project.files {
        let Some(&module_id) = module_for_path.get(file.path.as_str()) else {
            continue; // scaffold/runtime file with no owning module
        };
        let Ok(fingerprint) = fingerprint_source(file.path.as_str(), file.source.as_str()) else {
            continue;
        };
        modules.push(SubjectModule {
            module_id,
            file_path: file.path.clone(),
            fingerprint,
            bindings: bindings_for_path
                .remove(file.path.as_str())
                .unwrap_or_default(),
        });
    }
    Ok(modules)
}

/// One source file from the reference tree, fingerprinted for matching.
#[derive(Debug, Clone)]
pub(crate) struct ReferenceSourceModule {
    /// Path relative to the source root, e.g. `features/audio-capture.ts`.
    pub file_path: String,
    pub fingerprint: SourceFingerprint,
    /// Exported member names (from `export:` anchors).
    pub export_names: BTreeSet<String>,
    /// Native-asset literals referenced (string anchors ending in `.node`).
    pub asset_literals: BTreeSet<String>,
}

/// In-memory index over a reference source tree. Not persisted.
#[derive(Debug, Clone)]
pub(crate) struct ReferenceSourceIndex {
    pub version: String,
    pub modules: Vec<ReferenceSourceModule>,
}

use std::path::Path;

const SOURCE_EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"];
const SKIP_DIRS: &[&str] = &["node_modules", "test", "tests", "__tests__", "coverage"];

pub(crate) fn build_reference_source_index(
    root: &Path,
    version: &str,
) -> Result<ReferenceSourceIndex, String> {
    let mut files = Vec::new();
    collect_source_files(root, root, &mut files)?;
    files.sort();
    let mut modules = Vec::new();
    for absolute in files {
        let relative = absolute
            .strip_prefix(root)
            .map_err(|error| error.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        let source = std::fs::read_to_string(&absolute)
            .map_err(|error| format!("read {}: {error}", absolute.display()))?;
        let Ok(fingerprint) = fingerprint_source(relative.as_str(), source.as_str()) else {
            continue; // unparseable reference file — skip, do not guess
        };
        let (export_names, asset_literals) = classify_anchors(&fingerprint);
        modules.push(ReferenceSourceModule {
            file_path: relative,
            fingerprint,
            export_names,
            asset_literals,
        });
    }
    Ok(ReferenceSourceIndex {
        version: version.to_string(),
        modules,
    })
}

fn collect_source_files(
    root: &Path,
    dir: &Path,
    out: &mut Vec<std::path::PathBuf>,
) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|error| format!("read_dir {}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        let name = entry.file_name().to_string_lossy().to_string();
        if file_type.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            collect_source_files(root, &path, out)?;
        } else if file_type.is_file() {
            if name.ends_with(".d.ts") {
                continue;
            }
            let is_source = path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext));
            if is_source {
                out.push(path);
            }
        }
    }
    Ok(())
}

fn classify_anchors(fingerprint: &SourceFingerprint) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut exports = BTreeSet::new();
    let mut assets = BTreeSet::new();
    for anchor in &fingerprint.string_anchors {
        if let Some(name) = anchor.strip_prefix("export:") {
            exports.insert(name.to_string());
        } else if anchor.ends_with(".node") {
            // Match native assets by basename: the emitter rewrites the require
            // path (e.g. `/$bunfs/root/audio-capture.node` in the source tree
            // becomes `../assets/audio-capture.node` in emitted output), so the
            // full literal differs while the distinctive `.node` filename is
            // stable across versions.
            let basename = anchor.rsplit('/').next().unwrap_or(anchor.as_str());
            assets.insert(basename.to_string());
        }
    }
    (exports, assets)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MatchTier {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone)]
pub(crate) struct ModuleMatch {
    pub file_path: String,
    pub tier: MatchTier,
    pub asset_overlap: usize,
    pub export_overlap: usize,
    pub function_overlap: usize,
    pub anchor_overlap: usize,
    pub score: usize,
}

fn overlap_len(left: &BTreeSet<String>, right: &BTreeSet<String>) -> usize {
    left.intersection(right).count()
}

pub(crate) fn best_module_match(
    subject: &SourceFingerprint,
    index: &ReferenceSourceIndex,
) -> Option<ModuleMatch> {
    let (subject_exports, subject_assets) = classify_anchors(subject);
    let mut best: Option<ModuleMatch> = None;
    for module in &index.modules {
        let asset_overlap = overlap_len(&subject_assets, &module.asset_literals);
        let export_overlap = overlap_len(&subject_exports, &module.export_names);
        let function_overlap = overlap_len(
            &subject.function_signature_hashes,
            &module.fingerprint.function_signature_hashes,
        );
        let anchor_overlap =
            overlap_len(&subject.string_anchors, &module.fingerprint.string_anchors);
        let hash_match = !subject
            .normalized_source_hashes
            .is_disjoint(&module.fingerprint.normalized_source_hashes);
        let score =
            asset_overlap * 1000 + export_overlap * 50 + function_overlap * 5 + anchor_overlap;
        // Require at least 2 points of evidence when there is no hash match;
        // Low tier is never auto-accepted downstream, so this floor only
        // affects the dry-run report, not safety.
        if score < 2 && !hash_match {
            continue;
        }
        let tier = if hash_match || asset_overlap >= 1 {
            MatchTier::High
        } else if export_overlap >= 2 || function_overlap >= 2 {
            MatchTier::Medium
        } else {
            MatchTier::Low
        };
        let candidate = ModuleMatch {
            file_path: module.file_path.clone(),
            tier,
            asset_overlap,
            export_overlap,
            function_overlap,
            anchor_overlap,
            score,
        };
        let better = match &best {
            None => true,
            Some(current) => {
                candidate.score > current.score
                    || (candidate.score == current.score && candidate.file_path < current.file_path)
            }
        };
        if better {
            best = Some(candidate);
        }
    }
    best
}

fn tier_passes(tier: MatchTier, min: MinTier) -> bool {
    match min {
        MinTier::High => matches!(tier, MatchTier::High),
        MinTier::Medium => matches!(tier, MatchTier::High | MatchTier::Medium),
    }
}

fn write_export_names(
    connection: &Connection,
    project_id: u32,
    module_id: u32,
    reference_exports: &BTreeSet<String>,
    subject_bindings: &[(String, String)],
    origin: &str,
) -> Result<usize, CliRunError> {
    let subject_originals: BTreeSet<&str> = subject_bindings
        .iter()
        .map(|(orig, _)| orig.as_str())
        .collect();
    let mut written = 0;
    for export in reference_exports {
        if !subject_originals.contains(export.as_str()) {
            continue; // not an unambiguous 1:1 — leave for agent
        }
        connection
            .execute(
                r"
                INSERT INTO symbol_name_proposals (
                    project_id, module_id, original_name, semantic_name, origin, accepted, evidence
                ) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6)
                ON CONFLICT(project_id, module_id, original_name, origin, semantic_name)
                DO UPDATE SET accepted = excluded.accepted,
                    evidence = COALESCE(excluded.evidence, symbol_name_proposals.evidence)
                ",
                params![
                    i64::from(project_id),
                    i64::from(module_id),
                    export.as_str(),
                    export.as_str(),
                    origin,
                    "{\"tier\":\"export-exact\"}",
                ],
            )
            .map_err(|e| CliRunError::ReferenceSourceNames(e.to_string()))?;
        written += connection
            .execute(
                r"
                UPDATE symbols SET semantic_name = ?3, semantic_name_source = ?4
                 WHERE module_id = ?1 AND original_name = ?2 AND scope_level = 'module'
                ",
                params![
                    i64::from(module_id),
                    export.as_str(),
                    export.as_str(),
                    origin
                ],
            )
            .map_err(|e| CliRunError::ReferenceSourceNames(e.to_string()))?;
    }
    Ok(written)
}

/// Reserved for the deferred binding-level naming follow-up (Task 11b): carries
/// one row to be written into `semantic_binding_names`.
/// Not yet wired into `run()`.
#[allow(dead_code)]
struct BindingNameWrite {
    original_name: String,
    semantic_name: String,
    accepted: bool,
}

/// Reserved for the deferred binding-level naming follow-up (Task 11b): writes
/// `semantic_binding_names` rows (accepted=1 for anchored, 0 for agent proposals).
/// Not yet wired into `run()`.
#[allow(dead_code)]
fn write_binding_names(
    connection: &Connection,
    project_id: u32,
    file_path: &str,
    rows: &[BindingNameWrite],
    origin: &str,
) -> Result<usize, CliRunError> {
    let mut written = 0;
    for row in rows {
        let binding_key = row.original_name.clone(); // no binding_index → key on original
        written += connection
            .execute(
                r"
                INSERT INTO semantic_binding_names (
                    project_id, file_path, original_name, binding_index, binding_key, semantic_name,
                    origin, evidence, accepted, created_at, updated_at
                ) VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7, ?8, datetime('now'), datetime('now'))
                ON CONFLICT(project_id, file_path, original_name, binding_key) DO UPDATE SET
                    semantic_name = excluded.semantic_name, origin = excluded.origin,
                    evidence = excluded.evidence, accepted = excluded.accepted,
                    updated_at = datetime('now')
                ",
                params![
                    i64::from(project_id),
                    file_path,
                    row.original_name,
                    binding_key,
                    row.semantic_name,
                    origin,
                    if row.accepted {
                        "{\"tier\":\"fn-ast-collision\"}"
                    } else {
                        "{\"tier\":\"agent\"}"
                    },
                    i64::from(row.accepted),
                ],
            )
            .map_err(|e| CliRunError::ReferenceSourceNames(e.to_string()))?;
    }
    Ok(written)
}

fn write_module_names(
    connection: &Connection,
    plans: &[ModulePlan],
    min_tier: MinTier,
    origin_prefix: &str,
    reference_version: &str,
) -> Result<usize, CliRunError> {
    let mut written = 0;
    for plan in plans {
        if !tier_passes(plan.matched.tier, min_tier) {
            continue;
        }
        // _origin documents the provenance schema (prefix:version:file); symbol/binding
        // writers in later tasks record it — modules has no origin column.
        let _origin = format!(
            "{origin_prefix}:{reference_version}:{}",
            plan.matched.file_path
        );
        written += connection
            .execute(
                "UPDATE modules SET semantic_name = ?1 WHERE id = ?2",
                params![plan.module_semantic_name, i64::from(plan.module_id)],
            )
            .map_err(|error| CliRunError::ReferenceSourceNames(error.to_string()))?;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_index_fingerprints_source_files_and_skips_dts_and_node_modules() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path();
        std::fs::create_dir_all(root.join("features")).expect("mkdir");
        std::fs::create_dir_all(root.join("node_modules/x")).expect("mkdir");
        std::fs::write(
            root.join("features/audio-capture.ts"),
            "export var _HL = require('/$bunfs/root/audio-capture.node');",
        )
        .expect("write ts");
        std::fs::write(root.join("features/types.d.ts"), "export type T = number;")
            .expect("write dts");
        std::fs::write(root.join("node_modules/x/index.js"), "module.exports = 1;")
            .expect("write nm");

        let index = build_reference_source_index(root, "2.1.76").expect("index");
        assert_eq!(index.version, "2.1.76");
        let paths: Vec<&str> = index.modules.iter().map(|m| m.file_path.as_str()).collect();
        assert_eq!(paths, vec!["features/audio-capture.ts"]);
        assert!(
            index.modules[0]
                .asset_literals
                .contains("audio-capture.node")
        );
    }

    #[test]
    fn classify_anchors_splits_exports_and_native_assets() {
        // `_HL` normalizes to "hl" (2 chars) and is filtered by
        // `is_specific_export_member` (requires >= 3 normalized chars), so it
        // is never stored as an `export:` anchor.  Use `captureAudio` instead,
        // which normalises to "captureaudio" (12 chars) and passes all filters.
        let fingerprint = fingerprint_source(
            "features/audio-capture.ts",
            "export var captureAudio = require('/$bunfs/root/audio-capture.node');",
        )
        .expect("fingerprint");
        let (exports, assets) = classify_anchors(&fingerprint);
        assert!(exports.contains("captureAudio"), "exports: {exports:?}");
        assert!(
            assets.contains("audio-capture.node"),
            "assets matched by basename: {assets:?}"
        );
    }

    #[test]
    fn native_asset_literal_match_is_high_tier() {
        let index = {
            let temp = tempfile::tempdir().expect("temp");
            std::fs::create_dir_all(temp.path().join("features")).expect("mkdir");
            std::fs::write(
                temp.path().join("features/audio-capture.ts"),
                "export var _HL = require('/$bunfs/root/audio-capture.node');",
            )
            .expect("write");
            build_reference_source_index(temp.path(), "2.1.76").expect("index")
        };
        // Subject emitted module references the same native asset.
        let subject = fingerprint_source(
            "modules/m1.ts",
            "export const a = require('/$bunfs/root/audio-capture.node');",
        )
        .expect("subject fp");
        let matched = best_module_match(&subject, &index).expect("match");
        assert_eq!(matched.file_path, "features/audio-capture.ts");
        assert_eq!(matched.tier, MatchTier::High);
    }

    #[test]
    fn strip_source_extension_drops_known_suffixes() {
        assert_eq!(
            strip_source_extension("features/audio-capture.ts"),
            "features/audio-capture"
        );
        assert_eq!(strip_source_extension("a/b.mjs"), "a/b");
        assert_eq!(strip_source_extension("noext"), "noext");
    }

    fn make_plan(module_id: u32, name: &str, tier: MatchTier) -> ModulePlan {
        ModulePlan {
            module_id,
            subject_path: format!("modules/m{module_id}.ts"),
            reference_version: "2.1.76".to_string(),
            module_semantic_name: name.to_string(),
            matched: ModuleMatch {
                file_path: format!("{name}.ts"),
                tier,
                asset_overlap: if tier == MatchTier::High { 1 } else { 0 },
                export_overlap: 0,
                function_overlap: 0,
                anchor_overlap: 0,
                score: 1000,
            },
            subject_bindings: Vec::new(),
            reference_exports: std::collections::BTreeSet::new(),
        }
    }
    fn high_plan(id: u32, name: &str) -> ModulePlan {
        make_plan(id, name, MatchTier::High)
    }
    fn low_plan(id: u32, name: &str) -> ModulePlan {
        make_plan(id, name, MatchTier::Low)
    }

    #[test]
    fn write_module_names_updates_high_tier_only() {
        let connection = rusqlite::Connection::open_in_memory().expect("db");
        connection
            .execute_batch(
                r"
                CREATE TABLE modules (
                    id INTEGER PRIMARY KEY, file_id INTEGER, original_name TEXT NOT NULL,
                    semantic_name TEXT, module_category TEXT, package_name TEXT,
                    package_version TEXT, byte_start INTEGER, byte_end INTEGER
                );
                INSERT INTO modules (id, original_name) VALUES (10, 'm10'), (11, 'm11');
                ",
            )
            .expect("schema");
        let plans = vec![
            high_plan(10, "features/audio-capture"),
            low_plan(11, "misc/maybe"),
        ];
        let written = write_module_names(&connection, &plans, MinTier::High, "source", "2.1.76")
            .expect("write");
        assert_eq!(written, 1);
        let name10: Option<String> = connection
            .query_row("SELECT semantic_name FROM modules WHERE id = 10", [], |r| {
                r.get(0)
            })
            .expect("q10");
        let name11: Option<String> = connection
            .query_row("SELECT semantic_name FROM modules WHERE id = 11", [], |r| {
                r.get(0)
            })
            .expect("q11");
        assert_eq!(name10.as_deref(), Some("features/audio-capture"));
        assert_eq!(name11, None, "low tier must not be written");
    }

    #[test]
    fn export_name_proposals_only_on_exact_original_match() {
        let connection = rusqlite::Connection::open_in_memory().expect("db");
        connection
            .execute_batch(
                r"
                CREATE TABLE symbol_name_proposals (
                    project_id INTEGER NOT NULL, module_id INTEGER NOT NULL,
                    original_name TEXT NOT NULL, semantic_name TEXT NOT NULL,
                    origin TEXT NOT NULL, accepted INTEGER NOT NULL DEFAULT 0, evidence TEXT,
                    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    PRIMARY KEY (project_id, module_id, original_name, origin, semantic_name)
                );
                CREATE TABLE symbols (
                    module_id INTEGER, semantic_name TEXT, semantic_name_source TEXT,
                    export_name TEXT, original_name TEXT, scope_level TEXT
                );
                INSERT INTO symbols (module_id, original_name, scope_level)
                VALUES (10, '_HL', 'module'), (10, 'other', 'module');
                ",
            )
            .expect("schema");
        let ref_exports: BTreeSet<String> = ["_HL".to_string(), "missing".to_string()].into();
        let subject_bindings = vec![("_HL".to_string(), "a".to_string())];
        let written = write_export_names(
            &connection,
            1,
            10,
            &ref_exports,
            &subject_bindings,
            "source:2.1.76:features/audio-capture.ts",
        )
        .expect("write");
        assert_eq!(written, 1);
        let sem: Option<String> = connection
            .query_row(
                "SELECT semantic_name FROM symbols WHERE module_id = 10 AND original_name = '_HL'",
                [],
                |r| r.get(0),
            )
            .expect("q");
        assert_eq!(sem.as_deref(), Some("_HL"));
    }

    #[test]
    fn binding_names_written_anchored_accepted_unanchored_proposed() {
        let connection = rusqlite::Connection::open_in_memory().expect("db");
        connection
            .execute_batch(
                r"
                CREATE TABLE semantic_binding_names (
                    project_id INTEGER NOT NULL, file_path TEXT NOT NULL,
                    original_name TEXT NOT NULL, binding_index INTEGER, binding_key TEXT NOT NULL,
                    semantic_name TEXT NOT NULL, origin TEXT NOT NULL, evidence TEXT,
                    accepted INTEGER NOT NULL DEFAULT 1, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                    PRIMARY KEY (project_id, file_path, original_name, binding_key)
                );
                ",
            )
            .expect("schema");
        let rows = vec![
            BindingNameWrite {
                original_name: "x".into(),
                semantic_name: "decodeFrame".into(),
                accepted: true,
            },
            BindingNameWrite {
                original_name: "y".into(),
                semantic_name: "guessName".into(),
                accepted: false,
            },
        ];
        write_binding_names(&connection, 1, "modules/m.ts", &rows, "source:2.1.76:f.ts")
            .expect("write");
        let accepted: i64 = connection
            .query_row(
                "SELECT accepted FROM semantic_binding_names WHERE original_name='x'",
                [],
                |r| r.get(0),
            )
            .expect("qx");
        let proposed: i64 = connection
            .query_row(
                "SELECT accepted FROM semantic_binding_names WHERE original_name='y'",
                [],
                |r| r.get(0),
            )
            .expect("qy");
        assert_eq!(accepted, 1);
        assert_eq!(proposed, 0);
    }

    #[test]
    fn unrelated_modules_do_not_match() {
        let index = {
            let temp = tempfile::tempdir().expect("temp");
            std::fs::write(
                temp.path().join("a.ts"),
                "export function alpha(x){ return x + 1; }",
            )
            .expect("write");
            build_reference_source_index(temp.path(), "v").expect("index")
        };
        let subject = fingerprint_source(
            "modules/m.ts",
            "export const totallyDifferent = 42; console.log('zzz-unique-string');",
        )
        .expect("fp");
        assert!(best_module_match(&subject, &index).is_none());
    }

    // ── Test 1: e2e module match for both wrappers (hermetic) ──────────────────

    #[test]
    fn e2e_module_match_both_wrappers() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path();
        std::fs::create_dir_all(root.join("features")).expect("mkdir features");
        std::fs::create_dir_all(root.join("init")).expect("mkdir init");
        std::fs::write(
            root.join("features/audio-capture.ts"),
            "export var captureAudio = require('/$bunfs/root/audio-capture.node');",
        )
        .expect("write audio-capture.ts");
        std::fs::write(
            root.join("init/image-processor-native.ts"),
            "export var processImage = require('/$bunfs/root/image-processor.node');",
        )
        .expect("write image-processor-native.ts");

        let index = build_reference_source_index(root, "2.1.76").expect("index");

        // Subject 1: references the audio-capture .node literal
        let audio_subject = fingerprint_source(
            "modules/m1.ts",
            "export const a = require('/$bunfs/root/audio-capture.node');",
        )
        .expect("audio subject fp");
        let audio_match = best_module_match(&audio_subject, &index).expect("audio match");
        assert_eq!(
            audio_match.file_path, "features/audio-capture.ts",
            "audio subject must match features/audio-capture.ts"
        );
        assert_eq!(
            audio_match.tier,
            MatchTier::High,
            "audio match must be High tier"
        );
        assert!(
            audio_match.asset_overlap >= 1,
            "audio match must have asset_overlap >= 1"
        );
        assert_eq!(
            strip_source_extension(&audio_match.file_path),
            "features/audio-capture"
        );

        // Subject 2: references the image-processor .node literal
        let image_subject = fingerprint_source(
            "modules/m2.ts",
            "export const b = require('/$bunfs/root/image-processor.node');",
        )
        .expect("image subject fp");
        let image_match = best_module_match(&image_subject, &index).expect("image match");
        assert_eq!(
            image_match.file_path, "init/image-processor-native.ts",
            "image subject must match init/image-processor-native.ts"
        );
        assert_eq!(
            image_match.tier,
            MatchTier::High,
            "image match must be High tier"
        );
        assert!(
            image_match.asset_overlap >= 1,
            "image match must have asset_overlap >= 1"
        );
        assert_eq!(
            strip_source_extension(&image_match.file_path),
            "init/image-processor-native"
        );
    }

    // ── Test 2: e2e module-name WRITE (in-memory DB) ───────────────────────────

    #[test]
    fn e2e_write_module_names_two_high_tier() {
        let connection = rusqlite::Connection::open_in_memory().expect("db");
        connection
            .execute_batch(
                r"
                CREATE TABLE modules (
                    id INTEGER PRIMARY KEY, file_id INTEGER, original_name TEXT NOT NULL,
                    semantic_name TEXT, module_category TEXT, package_name TEXT,
                    package_version TEXT, byte_start INTEGER, byte_end INTEGER
                );
                INSERT INTO modules (id, original_name) VALUES (10, 'm10'), (11, 'm11');
                ",
            )
            .expect("schema");

        let plans = vec![
            make_plan(10, "features/audio-capture", MatchTier::High),
            make_plan(11, "init/image-processor-native", MatchTier::High),
        ];
        let written = write_module_names(&connection, &plans, MinTier::High, "source", "2.1.76")
            .expect("write");
        assert_eq!(written, 2, "both High-tier plans must be written");

        let name10: Option<String> = connection
            .query_row("SELECT semantic_name FROM modules WHERE id = 10", [], |r| {
                r.get(0)
            })
            .expect("q10");
        let name11: Option<String> = connection
            .query_row("SELECT semantic_name FROM modules WHERE id = 11", [], |r| {
                r.get(0)
            })
            .expect("q11");
        assert_eq!(
            name10.as_deref(),
            Some("features/audio-capture"),
            "module 10 semantic_name must be features/audio-capture"
        );
        assert_eq!(
            name11.as_deref(),
            Some("init/image-processor-native"),
            "module 11 semantic_name must be init/image-processor-native"
        );
    }

    // ── Test 3: export mapping e2e (in-memory DB) ──────────────────────────────

    #[test]
    fn e2e_export_mapping_exact_match_only() {
        let connection = rusqlite::Connection::open_in_memory().expect("db");
        connection
            .execute_batch(
                r"
                CREATE TABLE symbol_name_proposals (
                    project_id INTEGER NOT NULL, module_id INTEGER NOT NULL,
                    original_name TEXT NOT NULL, semantic_name TEXT NOT NULL,
                    origin TEXT NOT NULL, accepted INTEGER NOT NULL DEFAULT 0, evidence TEXT,
                    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    PRIMARY KEY (project_id, module_id, original_name, origin, semantic_name)
                );
                CREATE TABLE symbols (
                    module_id INTEGER, semantic_name TEXT, semantic_name_source TEXT,
                    export_name TEXT, original_name TEXT, scope_level TEXT
                );
                INSERT INTO symbols (module_id, original_name, scope_level)
                VALUES (10, 'captureAudio', 'module');
                ",
            )
            .expect("schema");

        let reference_exports: BTreeSet<String> =
            ["captureAudio".to_string(), "somethingMissing".to_string()].into();
        let subject_bindings = vec![("captureAudio".to_string(), "emittedX".to_string())];

        let written = write_export_names(
            &connection,
            1,
            10,
            &reference_exports,
            &subject_bindings,
            "source:2.1.76:features/audio-capture.ts",
        )
        .expect("write");

        assert_eq!(written, 1, "only the exact-match export must be written");

        let sem: Option<String> = connection
            .query_row(
                "SELECT semantic_name FROM symbols \
                 WHERE module_id = 10 AND original_name = 'captureAudio'",
                [],
                |r| r.get(0),
            )
            .expect("query captureAudio");
        assert_eq!(
            sem.as_deref(),
            Some("captureAudio"),
            "captureAudio must have semantic_name = captureAudio"
        );

        // The exact-match export must have an accepted proposal row...
        let captured_proposals: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM symbol_name_proposals \
                 WHERE module_id = 10 AND original_name = 'captureAudio' AND accepted = 1",
                [],
                |r| r.get(0),
            )
            .expect("count captureAudio proposals");
        assert_eq!(
            captured_proposals, 1,
            "captureAudio must have one accepted proposal"
        );

        // ...and the non-matching export must NOT be written at all.
        let missing_proposals: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM symbol_name_proposals WHERE original_name = 'somethingMissing'",
                [],
                |r| r.get(0),
            )
            .expect("count somethingMissing proposals");
        assert_eq!(
            missing_proposals, 0,
            "somethingMissing must never be written"
        );
    }

    // ── Test 4: precision gate — no false High-tier accept ─────────────────────

    #[test]
    fn precision_gate_no_false_high_tier() {
        // Build an index from a single unrelated file.
        let index = {
            let temp = tempfile::tempdir().expect("temp");
            std::fs::write(
                temp.path().join("a.ts"),
                "export function alpha(x){ return x + 1; }",
            )
            .expect("write a.ts");
            build_reference_source_index(temp.path(), "v").expect("index")
        };

        // Subject references a .node literal that is NOT in the index.
        let subject = fingerprint_source(
            "modules/m.ts",
            "export const x = require('/$bunfs/root/other.node'); const zzz_unique_xk9q = 1;",
        )
        .expect("subject fp");

        // Zero shared evidence (different .node literal, unique strings, no shared
        // exports/functions) must yield NO match at all — not merely a non-High one.
        // (Test 1 proves the matcher DOES return Some+High on real shared evidence, so
        // this is not vacuously satisfied by a matcher that always returns None.)
        let matched = best_module_match(&subject, &index);
        assert!(
            matched.is_none(),
            "unshared evidence must never produce a match; got: {matched:?}"
        );

        // Low-tier plan must not be written at MinTier::High gate.
        let connection = rusqlite::Connection::open_in_memory().expect("db");
        connection
            .execute_batch(
                r"
                CREATE TABLE modules (
                    id INTEGER PRIMARY KEY, file_id INTEGER, original_name TEXT NOT NULL,
                    semantic_name TEXT, module_category TEXT, package_name TEXT,
                    package_version TEXT, byte_start INTEGER, byte_end INTEGER
                );
                INSERT INTO modules (id, original_name) VALUES (99, 'mUnrelated');
                ",
            )
            .expect("schema");
        let low = make_plan(99, "a", MatchTier::Low);
        let written =
            write_module_names(&connection, &[low], MinTier::High, "source", "v").expect("write");
        assert_eq!(
            written, 0,
            "Low-tier plan must not be written at MinTier::High gate"
        );
    }
}
