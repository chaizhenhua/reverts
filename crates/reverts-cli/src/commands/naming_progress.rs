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

use reverts_input::{InputBundle, PackageAttributionStatus, PackageEmissionMode, SymbolScope};
use reverts_ir::{BindingShape, ModuleId, ModuleKind};
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
    let reached_level = if full.named == full.universe {
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
/// `ApplicationSource`. Excludes `ExternalImport`/`VendoredAsset`/`RuntimeGlue`
/// and `Builtin`.
fn first_party_module_ids(input: &InputBundle) -> BTreeSet<ModuleId> {
    let mut accepted_emission: BTreeMap<ModuleId, PackageEmissionMode> = BTreeMap::new();
    for attribution in &input.package_attributions {
        if attribution.status == PackageAttributionStatus::Accepted {
            accepted_emission.insert(attribution.module_id, attribution.emission_mode);
        }
    }
    input
        .modules
        .iter()
        .filter(|module| match accepted_emission.get(&module.id) {
            Some(mode) => *mode == PackageEmissionMode::ApplicationSource,
            None => module.kind == ModuleKind::Application,
        })
        .map(|module| module.id)
        .collect()
}

#[must_use]
pub fn compute_naming_progress(project_id: u32, program: &EnrichedProgram) -> NamingProgressReport {
    let model = program.model();
    let first_party = first_party_module_ids(model.input());
    let paths: BTreeMap<ModuleId, &str> = model
        .modules()
        .iter()
        .map(|module| (module.id, module.semantic_path.as_str()))
        .collect();

    let mut facts_by_module: BTreeMap<ModuleId, Vec<SymbolFact>> =
        first_party.iter().map(|id| (*id, Vec::new())).collect();
    for symbol in model.symbols() {
        if symbol.scope != SymbolScope::Module {
            continue;
        }
        let Some(facts) = facts_by_module.get_mut(&symbol.module_id) else {
            continue;
        };
        facts.push(SymbolFact {
            named: symbol.semantic_name.is_some(),
            exported: symbol.export_name.is_some(),
            kind: naming_kind(program.binding_shape(symbol.module_id, &symbol.name)),
        });
    }

    let mut all_facts: Vec<SymbolFact> = Vec::new();
    let mut modules: Vec<ModuleNamingProgress> = Vec::new();
    for (module_id, facts) in &facts_by_module {
        all_facts.extend_from_slice(facts);
        modules.push(ModuleNamingProgress {
            module_id: *module_id,
            semantic_path: paths.get(module_id).copied().unwrap_or("").to_string(),
            breakdown: tier_breakdown(facts),
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
    let bundle = load_project_bundle_with_package_externalization(&args.input, args.project_id)
        .map_err(NamingProgressError::LoadInput)?;
    let prepared = prepare_and_enrich(bundle).map_err(NamingProgressError::Pipeline)?;
    Ok(compute_naming_progress(args.project_id, &prepared.program))
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
    fn empty_facts_report_full_reached() {
        let breakdown = tier_breakdown(&[]);
        assert_eq!(breakdown.full.universe, 0);
        assert_eq!(breakdown.reached_level, Some(Tier::Full));
    }

    #[test]
    fn compute_excludes_external_modules_and_classifies_tiers() {
        use reverts_analyze::enrich_program;
        use reverts_input::{
            InputBundle, InputRows, ModuleInput, PackageAttributionInput, PackageAttributionStatus,
            PackageEmissionMode, ProjectInput, SymbolInput,
        };
        use reverts_ir::ModuleId;
        use reverts_model::ProgramModel;

        let app = ModuleInput::application(ModuleId(1), "entry", "src/index.ts");
        let pkg = ModuleInput::package(
            ModuleId(2),
            "dep",
            "node_modules/dep",
            "dep",
            Some("1.0.0".into()),
        );

        let mut rows = InputRows::new(ProjectInput::new(7, "fixture".to_string()));
        rows.modules = vec![app, pkg];
        rows.symbols = vec![
            SymbolInput::new(ModuleId(1), "parse")
                .with_export_name("parse")
                .with_semantic_name("parse"),
            SymbolInput::new(ModuleId(1), "help"),
            SymbolInput::new(ModuleId(2), "z"),
        ];
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
        let report: NamingProgressReport = compute_naming_progress(7, &program);

        assert_eq!(report.project_id, 7);
        assert_eq!(report.modules.len(), 1);
        assert_eq!(report.modules[0].module_id, ModuleId(1));
        assert_eq!(report.totals.full.universe, 2);
        assert_eq!(report.totals.full.named, 1);
        assert_eq!(report.totals.public_surface.universe, 1);
        assert_eq!(report.totals.public_surface.named, 1);
    }
}
