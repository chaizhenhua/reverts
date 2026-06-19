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
use reverts_ir::{BindingShape, ModuleId, ModuleKind};
use reverts_js::is_minified_identifier;
use reverts_model::EnrichedProgram;
use reverts_pipeline::prepare_and_enrich;

use crate::args::{NamingProgressArgs, NamingProgressTier};
use crate::errors::{CliRunError, NamingProgressError};
use crate::input_externalization::load_project_bundle_with_package_externalization;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamingKind {
    FunctionLike,
    ValueLike,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    PublicSurface,
    Declarations,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SymbolFact {
    named: bool,
    exported: bool,
    kind: NamingKind,
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

#[must_use]
pub fn naming_kind(shape: BindingShape) -> NamingKind {
    match shape {
        BindingShape::Callable | BindingShape::Constructor | BindingShape::ClassLike => {
            NamingKind::FunctionLike
        }
        BindingShape::Unknown
        | BindingShape::Value
        | BindingShape::PlainObject
        | BindingShape::NamespaceObject
        | BindingShape::EnumObject => NamingKind::ValueLike,
    }
}

fn symbol_tier(exported: bool, kind: NamingKind) -> Tier {
    if exported {
        Tier::PublicSurface
    } else if kind == NamingKind::FunctionLike {
        Tier::Declarations
    } else {
        Tier::Full
    }
}

fn tier_breakdown(facts: &[SymbolFact]) -> TierBreakdown {
    let mut public_surface = TierCoverage::default();
    let mut declarations = TierCoverage::default();
    let mut full = TierCoverage::default();
    for fact in facts {
        let tier = symbol_tier(fact.exported, fact.kind);
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

#[must_use]
pub fn compute_naming_progress(
    project_id: u32,
    program: &EnrichedProgram,
    excluded: &BTreeSet<ModuleId>,
) -> NamingProgressReport {
    let model = program.model();
    let first_party = first_party_module_ids(model.input(), excluded);
    let graph = model.graph();
    let paths: BTreeMap<ModuleId, &str> = model
        .modules()
        .iter()
        .map(|module| (module.id, module.semantic_path.as_str()))
        .collect();

    // Overlay: which (module, original_name) already carry an Agent-written
    // semantic name. The *universe* comes from the graph (the bindings actually
    // present in the emitted code); this table only marks what has been named.
    let mut named_overlay: BTreeSet<(ModuleId, &str)> = BTreeSet::new();
    for symbol in model.symbols() {
        if symbol.semantic_name.is_some() {
            named_overlay.insert((symbol.module_id, symbol.name.as_str()));
        }
    }

    let mut all_facts: Vec<SymbolFact> = Vec::new();
    let mut modules: Vec<ModuleNamingProgress> = Vec::new();
    for module_id in &first_party {
        let exported: BTreeSet<String> = graph
            .import_export()
            .exports_for(*module_id)
            .into_iter()
            .map(|binding| binding.as_str().to_string())
            .collect();
        let mut facts: Vec<SymbolFact> = Vec::new();
        for binding in graph.definitions_for(*module_id) {
            let name = binding.as_str();
            // "Named" = an Agent semantic name exists, or the original name is
            // already a meaningful identifier (e.g. preserved vendored source).
            let named =
                named_overlay.contains(&(*module_id, name)) || !is_minified_identifier(name);
            facts.push(SymbolFact {
                named,
                exported: exported.contains(name),
                kind: naming_kind(program.binding_shape(*module_id, name)),
            });
        }
        all_facts.extend_from_slice(&facts);
        modules.push(ModuleNamingProgress {
            module_id: *module_id,
            semantic_path: paths.get(module_id).copied().unwrap_or("").to_string(),
            breakdown: tier_breakdown(&facts),
        });
    }
    NamingProgressReport {
        project_id,
        modules,
        totals: tier_breakdown(&all_facts),
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
    Ok(compute_naming_progress(
        args.project_id,
        &prepared.program,
        &excluded,
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

fn headline_coverage(breakdown: &TierBreakdown, target: NamingProgressTier) -> TierCoverage {
    match target {
        NamingProgressTier::PublicSurface => breakdown.public_surface,
        NamingProgressTier::Declarations => breakdown.declarations,
        NamingProgressTier::Full => breakdown.full,
    }
}

pub(crate) fn run(args: NamingProgressArgs) -> Result<(), CliRunError> {
    let report = naming_progress_from_sqlite(&args).map_err(CliRunError::NamingProgress)?;
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
        NamingKind, NamingProgressReport, SymbolFact, Tier, compute_naming_progress, naming_kind,
        symbol_tier, tier_breakdown,
    };
    use reverts_ir::BindingShape;

    fn fact(named: bool, exported: bool, kind: NamingKind) -> SymbolFact {
        SymbolFact {
            named,
            exported,
            kind,
        }
    }

    #[test]
    fn naming_kind_maps_callable_shapes_to_function_like() {
        for shape in [
            BindingShape::Callable,
            BindingShape::Constructor,
            BindingShape::ClassLike,
        ] {
            assert_eq!(naming_kind(shape), NamingKind::FunctionLike);
        }
        for shape in [
            BindingShape::Unknown,
            BindingShape::Value,
            BindingShape::PlainObject,
            BindingShape::NamespaceObject,
            BindingShape::EnumObject,
        ] {
            assert_eq!(naming_kind(shape), NamingKind::ValueLike);
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

    #[test]
    fn compute_is_graph_driven_and_excludes_external_modules() {
        use reverts_analyze::enrich_program;
        use reverts_input::{
            InputBundle, InputRows, ModuleInput, PackageAttributionInput, PackageAttributionStatus,
            PackageEmissionMode, ProjectInput, SourceFileInput, SymbolInput,
        };
        use reverts_ir::ModuleId;
        use reverts_model::ProgramModel;

        // First-party module with: exported function `parse`, internal function
        // `help`, minified value `aB` (no overlay), minified value `cV` (Agent
        // semantic name overlay). `dep` is externalized -> excluded.
        let source = "export function parse(a){ return help(a) + aB + cV; }\n\
                      function help(x){ return x + 1; }\n\
                      var aB = 1;\n\
                      var cV = 2;\n";
        let app =
            ModuleInput::application(ModuleId(1), "entry", "src/index.ts").with_source_file(1);
        let pkg = ModuleInput::package(
            ModuleId(2),
            "dep",
            "node_modules/dep",
            "dep",
            Some("1.0.0".into()),
        );

        let mut rows = InputRows::new(ProjectInput::new(7, "fixture".to_string()));
        rows.source_files = vec![SourceFileInput {
            id: 1,
            path: "src/index.ts".to_string(),
            source: Some(source.to_string()),
        }];
        rows.modules = vec![app, pkg];
        rows.symbols = vec![SymbolInput::new(ModuleId(1), "cV").with_semantic_name("counter")];
        rows.package_attributions = vec![PackageAttributionInput {
            module_id: ModuleId(2),
            package_name: "dep".into(),
            package_version: Some("1.0.0".into()),
            subpath: None,
            resolved_file: None,
            export_specifier: Some("dep".into()),
            emission_mode: PackageEmissionMode::ExternalImport,
            status: PackageAttributionStatus::Accepted,
            rejection_reason: None,
            function_span: None,
            confidence: None,
        }];

        let bundle = InputBundle::from_rows(rows).expect("valid bundle");
        let program = enrich_program(ProgramModel::from_input(bundle)).program;
        let report: NamingProgressReport =
            compute_naming_progress(7, &program, &std::collections::BTreeSet::new());

        assert_eq!(report.project_id, 7);
        // Only the first-party module is measured; `dep` is excluded.
        assert_eq!(report.modules.len(), 1);
        assert_eq!(report.modules[0].module_id, ModuleId(1));
        // Universe = 4 graph definitions (parse, help, aB, cV).
        assert_eq!(report.totals.full.universe, 4);
        // Named = parse + help (meaningful) + cV (overlay); aB stays minified.
        assert_eq!(report.totals.full.named, 3);
        // Public surface = the exported `parse`.
        assert_eq!(report.totals.public_surface.universe, 1);
        assert_eq!(report.totals.public_surface.named, 1);
        // L1 and L2 fully named, L3 has the unnamed `aB`.
        assert_eq!(report.totals.reached_level, Some(Tier::Declarations));
    }
}
