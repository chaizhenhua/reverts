//! `naming-progress` command: report semantic-naming completion across three
//! cumulative work-amount tiers for first-party modules.
//!
//! Tiers (cumulative, `PublicSurface ⊆ Declarations ⊆ Full`):
//! - `PublicSurface`: exported symbols.
//! - `Declarations`: + non-exported function/class-like top-level symbols.
//! - `Full`: + remaining module-level value/const symbols.
//!
//! "Named" is the Agent-written DB field `symbols.semantic_name`
//! (`SymbolInput::semantic_name`), never the always-present computed
//! `SemanticNameMap`.

use std::collections::{BTreeMap, BTreeSet};

use reverts_input::{InputBundle, PackageAttributionStatus, PackageEmissionMode};
use reverts_ir::{ModuleId, ModuleKind};
use reverts_js::{is_generated_placeholder_identifier, is_minified_identifier};
use reverts_model::EnrichedProgram;
use reverts_pipeline::{SymbolIndexEntry, generate_project_from_prepared, prepare_and_enrich};

use crate::args::{NamingProgressArgs, NamingProgressTier};
use crate::errors::{CliRunError, NamingProgressError};
use crate::input_externalization::load_project_bundle_with_package_externalization;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NamingKind {
    FunctionLike,
    ValueLike,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    PublicSurface,
    Declarations,
    Full,
}

/// One module-level binding with its computed tier and whether it is already
/// named. Shared by `naming-progress` (counts) and `naming-plan` (work list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SymbolDetail {
    pub original_name: String,
    pub tier: Tier,
    pub named: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TierCoverage {
    /// Cumulative count of symbols whose introduced tier is <= this level.
    pub universe: usize,
    /// Cumulative count of those that are named.
    pub named: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TierBreakdown {
    pub public_surface: TierCoverage,
    pub declarations: TierCoverage,
    pub full: TierCoverage,
    /// Highest fully-named (100%) tier; `None` if public surface is incomplete.
    pub reached_level: Option<Tier>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleNamingProgress {
    pub module_id: ModuleId,
    pub semantic_path: String,
    pub breakdown: TierBreakdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamingProgressReport {
    pub project_id: u32,
    pub modules: Vec<ModuleNamingProgress>,
    pub totals: TierBreakdown,
}

pub(crate) fn symbol_tier(exported: bool, kind: NamingKind) -> Tier {
    if exported {
        Tier::PublicSurface
    } else if kind == NamingKind::FunctionLike {
        Tier::Declarations
    } else {
        Tier::Full
    }
}

fn tier_breakdown(facts: &[SymbolDetail]) -> TierBreakdown {
    let mut public_surface = TierCoverage::default();
    let mut declarations = TierCoverage::default();
    let mut full = TierCoverage::default();
    for fact in facts {
        let tier = fact.tier;
        full.universe += 1;
        if fact.named {
            full.named += 1;
        }
        if matches!(tier, Tier::PublicSurface | Tier::Declarations) {
            declarations.universe += 1;
            if fact.named {
                declarations.named += 1;
            }
        }
        if tier == Tier::PublicSurface {
            public_surface.universe += 1;
            if fact.named {
                public_surface.named += 1;
            }
        }
    }
    let reached_level = if full.universe == 0 {
        // No bindings in scope: nothing extracted, not "fully named".
        None
    } else if full.named == full.universe {
        Some(Tier::Full)
    } else if declarations.named == declarations.universe {
        Some(Tier::Declarations)
    } else if public_surface.named == public_surface.universe {
        Some(Tier::PublicSurface)
    } else {
        None
    };
    TierBreakdown {
        public_surface,
        declarations,
        full,
        reached_level,
    }
}

/// Modules whose source the human reads: `Application` modules with no
/// externalizing attribution, plus package modules attributed
/// `ApplicationSource`. Excludes `ExternalImport`/`VendoredAsset`/`RuntimeGlue`,
/// `Builtin`, and vendored third-party source kept under `node_modules` (a
/// deterministic path signal — such modules are not first-party naming work
/// even when they were not externalized to a bare import).
fn first_party_module_ids(
    input: &InputBundle,
    excluded: &BTreeSet<ModuleId>,
) -> BTreeSet<ModuleId> {
    let mut accepted_emission: BTreeMap<ModuleId, PackageEmissionMode> = BTreeMap::new();
    for attribution in &input.package_attributions {
        if attribution.status == PackageAttributionStatus::Accepted {
            accepted_emission.insert(attribution.module_id, attribution.emission_mode);
        }
    }
    input
        .modules
        .iter()
        .filter(|module| {
            // Honor recorded module classifications (agent or deterministic).
            if excluded.contains(&module.id) {
                return false;
            }
            if is_vendored_path(&module.semantic_path) {
                return false;
            }
            match accepted_emission.get(&module.id) {
                Some(mode) => *mode == PackageEmissionMode::ApplicationSource,
                None => module.kind == ModuleKind::Application,
            }
        })
        .map(|module| module.id)
        .collect()
}

/// Path evidence that a module is vendored third-party source.
fn is_vendored_path(path: &str) -> bool {
    path.contains("node_modules/") || path.starts_with("node_modules")
}

/// First-party module set (after path/classification/emission exclusion) plus
/// exported binding names per module. The actionable binding universe comes from
/// the *emitted* output (`SymbolIndexEntry`); export *names* are preserved across
/// reconstruction, so the graph supplies them reliably to classify tiers.
pub(crate) struct EmittedUniverse {
    pub first_party: BTreeSet<u32>,
    pub exported_by_module: BTreeMap<u32, BTreeSet<String>>,
}

pub(crate) fn emitted_universe(
    program: &EnrichedProgram,
    excluded: &BTreeSet<ModuleId>,
) -> EmittedUniverse {
    let model = program.model();
    let first_party_ids = first_party_module_ids(model.input(), excluded);
    let graph = model.graph();
    let mut first_party = BTreeSet::new();
    let mut exported_by_module: BTreeMap<u32, BTreeSet<String>> = BTreeMap::new();
    for module_id in &first_party_ids {
        first_party.insert(module_id.0);
        let exported = graph
            .import_export()
            .exports_for(*module_id)
            .into_iter()
            .map(|binding| binding.as_str().to_string())
            .collect();
        exported_by_module.insert(module_id.0, exported);
    }
    EmittedUniverse {
        first_party,
        exported_by_module,
    }
}

/// Classifies one emitted symbol-index entry into a tier with named status.
/// `None` when its module is not first-party (externalized / vendored /
/// classified out). Single source of truth for both progress and plan.
pub(crate) fn classify_emitted_entry(
    entry: &SymbolIndexEntry,
    universe: &EmittedUniverse,
) -> Option<SymbolDetail> {
    if !universe.first_party.contains(&entry.module_id.0) {
        return None;
    }
    // Named = renamed by the Agent (emitted != original) or already a meaningful
    // identifier (preserved vendored source).
    let named = !is_generated_placeholder_identifier(&entry.emitted_name)
        && (entry.emitted_name != entry.original_name
            || is_meaningful_preserved_identifier(&entry.original_name));
    let exported = universe
        .exported_by_module
        .get(&entry.module_id.0)
        .is_some_and(|names| names.contains(&entry.original_name));
    let kind = if entry.function_like {
        NamingKind::FunctionLike
    } else {
        NamingKind::ValueLike
    };
    Some(SymbolDetail {
        original_name: entry.original_name.clone(),
        tier: symbol_tier(exported, kind),
        named,
    })
}

fn is_meaningful_preserved_identifier(name: &str) -> bool {
    !is_generated_placeholder_identifier(name)
        && (!is_minified_identifier(name)
            || matches!(name, "cmd" | "cwd" | "env" | "gid" | "pid" | "uid" | "uri"))
}

#[must_use]
pub(crate) fn compute_naming_progress(
    project_id: u32,
    symbol_index: &[SymbolIndexEntry],
    universe: &EmittedUniverse,
) -> NamingProgressReport {
    let mut by_module: BTreeMap<u32, (String, Vec<SymbolDetail>)> = BTreeMap::new();
    let mut all_symbols: Vec<SymbolDetail> = Vec::new();
    for entry in symbol_index {
        let Some(detail) = classify_emitted_entry(entry, universe) else {
            continue;
        };
        all_symbols.push(detail.clone());
        by_module
            .entry(entry.module_id.0)
            .or_insert_with(|| (entry.file_path.clone(), Vec::new()))
            .1
            .push(detail);
    }
    let modules = by_module
        .into_iter()
        .map(|(module_id, (file_path, symbols))| ModuleNamingProgress {
            module_id: ModuleId(module_id),
            semantic_path: file_path,
            breakdown: tier_breakdown(&symbols),
        })
        .collect();
    NamingProgressReport {
        project_id,
        modules,
        totals: tier_breakdown(&all_symbols),
    }
}

pub fn naming_progress_from_sqlite(
    args: &NamingProgressArgs,
) -> Result<NamingProgressReport, NamingProgressError> {
    let excluded = crate::commands::module_classify::excluded_module_ids_from_sqlite(
        args.input.as_path(),
        args.project_id,
    )
    .map_err(NamingProgressError::Classification)?;
    let bundle = load_project_bundle_with_package_externalization(&args.input, args.project_id)
        .map_err(NamingProgressError::LoadInput)?;
    let prepared = prepare_and_enrich(bundle).map_err(NamingProgressError::Pipeline)?;
    // The actionable universe is the emitted output; build the export/first-party
    // view before the emit consumes `prepared`.
    let universe = emitted_universe(&prepared.program, &excluded);
    let run = generate_project_from_prepared(prepared).map_err(NamingProgressError::Pipeline)?;
    Ok(compute_naming_progress(
        args.project_id,
        &run.symbol_index,
        &universe,
    ))
}

fn pct(coverage: TierCoverage) -> f64 {
    if coverage.universe == 0 {
        100.0
    } else {
        (coverage.named as f64 * 100.0) / coverage.universe as f64
    }
}

fn tier_label(tier: Option<Tier>) -> &'static str {
    match tier {
        Some(Tier::Full) => "full",
        Some(Tier::Declarations) => "declarations",
        Some(Tier::PublicSurface) => "public-surface",
        None => "below-public-surface",
    }
}

fn target_label(target: NamingProgressTier) -> &'static str {
    match target {
        NamingProgressTier::PublicSurface => "public-surface",
        NamingProgressTier::Declarations => "declarations",
        NamingProgressTier::Full => "full",
    }
}

fn coverage_json(coverage: TierCoverage) -> serde_json::Value {
    serde_json::json!({
        "named": coverage.named,
        "total": coverage.universe,
        "pending": coverage.universe.saturating_sub(coverage.named),
        "percent": pct(coverage),
    })
}

fn headline_coverage(breakdown: &TierBreakdown, target: NamingProgressTier) -> TierCoverage {
    match target {
        NamingProgressTier::PublicSurface => breakdown.public_surface,
        NamingProgressTier::Declarations => breakdown.declarations,
        NamingProgressTier::Full => breakdown.full,
    }
}

#[must_use]
pub fn naming_progress_json(report: &NamingProgressReport, target: NamingProgressTier) -> String {
    let headline = headline_coverage(&report.totals, target);
    let value = serde_json::json!({
        "schema": "reverts.naming_progress.v1",
        "project_id": report.project_id,
        "target_level": target_label(target),
        "named": headline.named,
        "total": headline.universe,
        "pending": headline.universe.saturating_sub(headline.named),
        "percent": pct(headline),
        "reached": tier_label(report.totals.reached_level),
        "complete": headline.universe == 0 || headline.named == headline.universe,
        "tiers": {
            "public_surface": coverage_json(report.totals.public_surface),
            "declarations": coverage_json(report.totals.declarations),
            "full": coverage_json(report.totals.full),
        },
        "modules": report.modules.iter().map(|module| {
            serde_json::json!({
                "module_id": module.module_id.0,
                "file_path": module.semantic_path,
                "reached": tier_label(module.breakdown.reached_level),
                "tiers": {
                    "public_surface": coverage_json(module.breakdown.public_surface),
                    "declarations": coverage_json(module.breakdown.declarations),
                    "full": coverage_json(module.breakdown.full),
                },
            })
        }).collect::<Vec<_>>(),
    });
    serde_json::to_string_pretty(&value).expect("serializing naming progress JSON is infallible")
}

pub(crate) fn run(args: NamingProgressArgs) -> Result<(), CliRunError> {
    let report = naming_progress_from_sqlite(&args).map_err(CliRunError::NamingProgress)?;
    if args.json {
        println!("{}", naming_progress_json(&report, args.target_level));
        return Ok(());
    }
    let totals = &report.totals;
    let headline = headline_coverage(totals, args.target_level);
    println!(
        "naming progress for project {}: target={} {}/{} ({:.2}%) | public_surface {}/{} ({:.2}%), declarations {}/{} ({:.2}%), full {}/{} ({:.2}%), reached={}, modules={}",
        report.project_id,
        target_label(args.target_level),
        headline.named,
        headline.universe,
        pct(headline),
        totals.public_surface.named,
        totals.public_surface.universe,
        pct(totals.public_surface),
        totals.declarations.named,
        totals.declarations.universe,
        pct(totals.declarations),
        totals.full.named,
        totals.full.universe,
        pct(totals.full),
        tier_label(totals.reached_level),
        report.modules.len(),
    );
    for module in &report.modules {
        println!(
            "  {}: public_surface {}/{}, declarations {}/{}, full {}/{}, reached={}",
            module.semantic_path,
            module.breakdown.public_surface.named,
            module.breakdown.public_surface.universe,
            module.breakdown.declarations.named,
            module.breakdown.declarations.universe,
            module.breakdown.full.named,
            module.breakdown.full.universe,
            tier_label(module.breakdown.reached_level),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        NamingKind, NamingProgressReport, SymbolDetail, SymbolIndexEntry, Tier,
        classify_emitted_entry, compute_naming_progress, symbol_tier, tier_breakdown,
    };

    fn fact(named: bool, exported: bool, kind: NamingKind) -> SymbolDetail {
        SymbolDetail {
            original_name: String::new(),
            tier: symbol_tier(exported, kind),
            named,
        }
    }

    #[test]
    fn symbol_tier_uses_export_then_kind() {
        assert_eq!(
            symbol_tier(true, NamingKind::ValueLike),
            Tier::PublicSurface
        );
        assert_eq!(
            symbol_tier(false, NamingKind::FunctionLike),
            Tier::Declarations
        );
        assert_eq!(symbol_tier(false, NamingKind::ValueLike), Tier::Full);
    }

    #[test]
    fn breakdown_counts_are_cumulative() {
        let facts = [
            fact(true, true, NamingKind::FunctionLike),   // L1
            fact(false, false, NamingKind::FunctionLike), // L2
            fact(false, false, NamingKind::ValueLike),    // L3
        ];
        let breakdown = tier_breakdown(&facts);
        assert_eq!(breakdown.public_surface.universe, 1);
        assert_eq!(breakdown.declarations.universe, 2);
        assert_eq!(breakdown.full.universe, 3);
    }

    #[test]
    fn reached_level_is_highest_fully_named_tier() {
        // L1 named, L2 named, L3 unnamed -> reached Declarations.
        let facts = [
            fact(true, true, NamingKind::ValueLike),
            fact(true, false, NamingKind::FunctionLike),
            fact(false, false, NamingKind::ValueLike),
        ];
        let breakdown = tier_breakdown(&facts);
        assert_eq!(breakdown.reached_level, Some(Tier::Declarations));
    }

    #[test]
    fn reached_level_none_when_public_surface_incomplete() {
        let facts = [fact(false, true, NamingKind::ValueLike)];
        let breakdown = tier_breakdown(&facts);
        assert_eq!(breakdown.reached_level, None);
    }

    #[test]
    fn empty_universe_reports_no_reached_level() {
        let breakdown = tier_breakdown(&[]);
        assert_eq!(breakdown.full.universe, 0);
        assert_eq!(breakdown.reached_level, None);
    }

    #[test]
    fn vendored_node_modules_paths_are_excluded() {
        use super::is_vendored_path;
        assert!(is_vendored_path("modules/36-node_modules/ws/lib/stream.ts"));
        assert!(is_vendored_path("node_modules/ws/index.js"));
        assert!(!is_vendored_path("modules/495-esbuild-sfr.ts"));
        assert!(!is_vendored_path("src/index.ts"));
    }

    fn universe(first_party: &[u32], exported: &[(u32, &str)]) -> super::EmittedUniverse {
        use std::collections::{BTreeMap, BTreeSet};
        let mut exported_by_module: BTreeMap<u32, BTreeSet<String>> = BTreeMap::new();
        for (module_id, name) in exported {
            exported_by_module
                .entry(*module_id)
                .or_default()
                .insert((*name).to_string());
        }
        super::EmittedUniverse {
            first_party: first_party.iter().copied().collect(),
            exported_by_module,
        }
    }

    fn entry(
        module_id: u32,
        original: &str,
        emitted: &str,
        function_like: bool,
    ) -> SymbolIndexEntry {
        SymbolIndexEntry {
            module_id: reverts_ir::ModuleId(module_id),
            original_name: original.to_string(),
            emitted_name: emitted.to_string(),
            file_path: format!("modules/{module_id}.ts"),
            function_like,
        }
    }

    #[test]
    fn classify_marks_minified_unnamed_exported_binding() {
        let universe = universe(&[1], &[(1, "aB")]);
        let detail = classify_emitted_entry(&entry(1, "aB", "aB", false), &universe)
            .expect("first-party binding");
        assert_eq!(detail.tier, Tier::PublicSurface); // exported
        assert!(!detail.named); // minified, not renamed
    }

    #[test]
    fn classify_treats_common_runtime_abbreviations_as_meaningful() {
        let universe = universe(&[1], &[]);

        let detail = classify_emitted_entry(&entry(1, "cwd", "cwd", false), &universe)
            .expect("first-party symbol");

        assert!(detail.named);
    }

    #[test]
    fn classify_skips_non_first_party_module() {
        let universe = universe(&[1], &[]);
        assert!(classify_emitted_entry(&entry(2, "x", "x", false), &universe).is_none());
    }

    #[test]
    fn classify_counts_renamed_or_meaningful_as_named() {
        let universe = universe(&[1], &[]);
        // Renamed by the Agent (emitted != original).
        assert!(
            classify_emitted_entry(&entry(1, "aB", "createClient", false), &universe)
                .expect("first-party")
                .named
        );
        // Already meaningful original.
        assert!(
            classify_emitted_entry(&entry(1, "tokenize", "tokenize", true), &universe)
                .expect("first-party")
                .named
        );
    }

    #[test]
    fn classify_does_not_count_generated_placeholders_as_named() {
        let universe = universe(&[1], &[]);

        assert!(
            !classify_emitted_entry(&entry(1, "aB", "semanticValue25", false), &universe)
                .expect("first-party")
                .named
        );
        assert!(
            !classify_emitted_entry(
                &entry(1, "Rdr", "module247SemanticSymbol001", false),
                &universe
            )
            .expect("first-party")
            .named
        );
    }

    #[test]
    fn compute_aggregates_emitted_index_and_excludes_external() {
        let universe = universe(&[1], &[(1, "aB")]);
        let index = [
            entry(1, "aB", "aB", false),         // exported, minified -> L1 unnamed
            entry(1, "hL", "hL", true),          // internal fn -> L2 unnamed
            entry(1, "cD", "cD", false),         // internal value -> L3 unnamed
            entry(1, "pretty", "pretty", false), // meaningful -> L3 named
            entry(2, "zz", "zz", false),         // module 2 not first-party -> excluded
        ];
        let report: NamingProgressReport = compute_naming_progress(7, &index, &universe);

        assert_eq!(report.project_id, 7);
        assert_eq!(report.modules.len(), 1);
        assert_eq!(report.modules[0].module_id, reverts_ir::ModuleId(1));
        assert_eq!(report.totals.full.universe, 4); // module 2 excluded
        assert_eq!(report.totals.full.named, 1); // only `pretty`
        assert_eq!(report.totals.public_surface.universe, 1); // `aB`
        assert_eq!(report.totals.public_surface.named, 0);
        assert_eq!(report.totals.declarations.universe, 2); // `aB` + `hL`
        assert_eq!(report.totals.reached_level, None); // public surface incomplete
    }
}
