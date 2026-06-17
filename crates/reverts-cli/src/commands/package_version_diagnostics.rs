//! Read-only package-version diagnostics.
//!
//! This command inspects package attributions that were rejected because the
//! selected package version did not match the module source. It treats the DB
//! version as a hint, scores every available cached/source-root candidate
//! version against the rejected module source, and prints whether a safer exact
//! version can be inferred. It never writes SQLite.

use std::collections::{BTreeMap, BTreeSet};

use reverts_input::{InputRows, ModuleInput, PackageAttributionStatus};
use reverts_ir::ModuleId;
use reverts_package_matcher::{
    BestVersionMatch, ModulePackageMatch, PackageSource, VersionMatchScore,
    VersionedPackageMatcher, VersionedPackageMatcherConfig, package_source_normalized_hash,
};
use rusqlite::{Connection, OpenFlags};

use crate::args::PackageVersionDiagnosticsArgs;
use crate::errors::{CliRunError, MatchPackagesError};
use crate::{load_package_sources, pct};

const VERSION_MISMATCH_REJECTION_REASON: &str =
    "selected package version did not match this module source";
const STRUCTURAL_RECOMMENDATION_MIN_MARGIN: u32 = 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageVersionDiagnosticsOutcome {
    pub project_id: u32,
    pub rejected_modules: usize,
    pub packages: Vec<PackageVersionDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageVersionDiagnostic {
    pub package_name: String,
    pub rejected_modules: Vec<RejectedVersionModule>,
    pub cached_versions: Vec<String>,
    pub exact_hash_matches: Vec<ExactHashModuleMatch>,
    pub candidates: Vec<VersionCandidateDiagnostic>,
    pub recommendation: VersionDiagnosticRecommendation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedVersionModule {
    pub module_id: ModuleId,
    pub semantic_path: String,
    pub db_version_hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactHashModuleMatch {
    pub module_id: ModuleId,
    pub semantic_path: String,
    pub matches: Vec<ExactHashSourceMatch>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactHashSourceMatch {
    pub package_version: String,
    pub source_path: String,
    pub external_importable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionCandidateDiagnostic {
    pub package_version: String,
    pub decision: &'static str,
    pub score: u32,
    pub matched_modules: usize,
    pub total_modules: usize,
    pub source_hash_matches: usize,
    pub function_signature_matches: usize,
    pub string_anchor_matches: usize,
    pub module_matches: usize,
    pub external_importable_matches: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionDiagnosticRecommendation {
    NoCandidates,
    KeepRejected {
        reason: String,
    },
    SourceHash {
        package_version: String,
        source_hash_matches: usize,
    },
    StructuralMultiModule {
        package_version: String,
        margin: u32,
        matched_modules: usize,
    },
}

pub(crate) fn run(args: PackageVersionDiagnosticsArgs) -> Result<(), CliRunError> {
    let outcome =
        package_version_diagnostics_from_sqlite(&args).map_err(CliRunError::MatchPackages)?;
    print_package_version_diagnostics(&outcome, args.top as usize);
    Ok(())
}

pub fn package_version_diagnostics_from_sqlite(
    args: &PackageVersionDiagnosticsArgs,
) -> Result<PackageVersionDiagnosticsOutcome, MatchPackagesError> {
    let mut connection = Connection::open_with_flags(&args.input, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|source| MatchPackagesError::OpenDatabase {
            path: args.input.clone(),
            source,
        })?;
    let rows =
        reverts_input::sqlite::load_project_rows_from_connection(&connection, args.project_id)
            .map_err(MatchPackagesError::LoadInput)?;
    package_version_diagnostics_from_connection(&mut connection, &rows, args)
}

pub fn package_version_diagnostics_from_connection(
    connection: &mut Connection,
    rows: &InputRows,
    args: &PackageVersionDiagnosticsArgs,
) -> Result<PackageVersionDiagnosticsOutcome, MatchPackagesError> {
    let package_filter = args.package_names.iter().cloned().collect::<BTreeSet<_>>();
    let rejected_by_package = rejected_version_mismatch_modules(rows, &package_filter);
    let rejected_modules = rejected_by_package.values().map(Vec::len).sum::<usize>();
    let package_names = rejected_by_package.keys().cloned().collect::<BTreeSet<_>>();
    let package_sources = load_package_sources(
        connection,
        rows,
        &package_names,
        &args.package_source_roots,
        args.materialize_package_sources,
        false,
    )?;
    let sources_by_package = package_sources_by_package(&package_sources);
    let diagnostics = rejected_by_package
        .into_iter()
        .map(|(package_name, modules)| {
            let sources = sources_by_package
                .get(package_name.as_str())
                .cloned()
                .unwrap_or_default();
            diagnose_package(rows, package_name, modules, sources)
        })
        .collect();

    Ok(PackageVersionDiagnosticsOutcome {
        project_id: args.project_id,
        rejected_modules,
        packages: diagnostics,
    })
}

fn rejected_version_mismatch_modules(
    rows: &InputRows,
    package_filter: &BTreeSet<String>,
) -> BTreeMap<String, Vec<RejectedVersionModule>> {
    let modules_by_id = rows
        .modules
        .iter()
        .map(|module| (module.id, module))
        .collect::<BTreeMap<_, _>>();
    let mut by_package = BTreeMap::<String, Vec<RejectedVersionModule>>::new();
    for attribution in &rows.package_attributions {
        if attribution.status != PackageAttributionStatus::Rejected
            || attribution.rejection_reason.as_deref() != Some(VERSION_MISMATCH_REJECTION_REASON)
        {
            continue;
        }
        if !package_filter.is_empty() && !package_filter.contains(&attribution.package_name) {
            continue;
        }
        let Some(module) = modules_by_id.get(&attribution.module_id) else {
            continue;
        };
        by_package
            .entry(attribution.package_name.clone())
            .or_default()
            .push(RejectedVersionModule {
                module_id: module.id,
                semantic_path: module.semantic_path.clone(),
                db_version_hint: attribution
                    .package_version
                    .as_ref()
                    .filter(|version| !version.trim().is_empty())
                    .cloned(),
            });
    }
    for modules in by_package.values_mut() {
        modules.sort_by(|left, right| left.module_id.cmp(&right.module_id));
    }
    by_package
}

fn package_sources_by_package<'a>(
    package_sources: &'a [PackageSource],
) -> BTreeMap<&'a str, Vec<&'a PackageSource>> {
    let mut by_package = BTreeMap::<&'a str, Vec<&'a PackageSource>>::new();
    for source in package_sources {
        by_package
            .entry(source.package_name.as_str())
            .or_default()
            .push(source);
    }
    by_package
}

fn diagnose_package(
    rows: &InputRows,
    package_name: String,
    modules: Vec<RejectedVersionModule>,
    sources: Vec<&PackageSource>,
) -> PackageVersionDiagnostic {
    let cached_versions = sources
        .iter()
        .map(|source| source.package_version.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let exact_hash_matches = exact_hash_matches(rows, &modules, sources.as_slice());
    let mut candidates = cached_versions
        .iter()
        .filter_map(|package_version| {
            candidate_diagnostic(
                rows,
                &package_name,
                &modules,
                package_version,
                sources.as_slice(),
            )
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| right.source_hash_matches.cmp(&left.source_hash_matches))
            .then_with(|| right.matched_modules.cmp(&left.matched_modules))
            .then_with(|| left.package_version.cmp(&right.package_version))
    });
    let db_hints = modules
        .iter()
        .filter_map(|module| module.db_version_hint.clone())
        .collect::<BTreeSet<_>>();
    let recommendation = recommendation_for_candidates(&candidates, &db_hints);
    PackageVersionDiagnostic {
        package_name,
        rejected_modules: modules,
        cached_versions,
        exact_hash_matches,
        candidates,
        recommendation,
    }
}

fn exact_hash_matches(
    rows: &InputRows,
    modules: &[RejectedVersionModule],
    sources: &[&PackageSource],
) -> Vec<ExactHashModuleMatch> {
    let mut sources_by_hash = BTreeMap::<String, Vec<&PackageSource>>::new();
    for source in sources {
        if let Some(hash) = package_source_normalized_hash(&source.source_path, &source.source) {
            sources_by_hash.entry(hash).or_default().push(*source);
        }
    }
    modules
        .iter()
        .filter_map(|module| {
            let slice = rows.module_source_slice(module.module_id)?;
            let hash = package_source_normalized_hash(slice.source_file_path, slice.source)?;
            let matches = sources_by_hash
                .get(hash.as_str())?
                .iter()
                .map(|source| ExactHashSourceMatch {
                    package_version: source.package_version.clone(),
                    source_path: source.source_path.clone(),
                    external_importable: source.external_importable,
                })
                .collect::<Vec<_>>();
            (!matches.is_empty()).then_some(ExactHashModuleMatch {
                module_id: module.module_id,
                semantic_path: module.semantic_path.clone(),
                matches,
            })
        })
        .collect()
}

fn candidate_diagnostic(
    rows: &InputRows,
    package_name: &str,
    modules: &[RejectedVersionModule],
    package_version: &str,
    sources: &[&PackageSource],
) -> Option<VersionCandidateDiagnostic> {
    let candidate_rows = candidate_rows(rows, modules, package_version);
    let package_sources = sources
        .iter()
        .map(|source| (*source).clone())
        .collect::<Vec<_>>();
    let package_filter = BTreeSet::from([package_name.to_string()]);
    let report = VersionedPackageMatcher::new(VersionedPackageMatcherConfig::default())
        .match_rows_for_packages(&candidate_rows, &package_sources, &package_filter);
    report
        .version_matches
        .into_iter()
        .flat_map(candidate_diagnostics_from_decision)
        .next()
}

fn candidate_rows(
    rows: &InputRows,
    modules: &[RejectedVersionModule],
    package_version: &str,
) -> InputRows {
    let module_ids = modules
        .iter()
        .map(|module| module.module_id)
        .collect::<BTreeSet<_>>();
    let mut candidate = rows.clone();
    candidate.modules = candidate
        .modules
        .into_iter()
        .filter_map(|mut module| {
            module_ids.contains(&module.id).then(|| {
                module.package_version = Some(package_version.to_string());
                module
            })
        })
        .collect::<Vec<ModuleInput>>();
    candidate.assets.clear();
    candidate.symbols.clear();
    candidate.dependencies.clear();
    candidate.package_attributions.clear();
    candidate.package_surfaces.clear();
    candidate
}

fn candidate_diagnostics_from_decision(
    decision: BestVersionMatch,
) -> Vec<VersionCandidateDiagnostic> {
    match decision {
        BestVersionMatch::Selected {
            score,
            module_matches,
        } => vec![candidate_diagnostic_from_score(
            "selected",
            score,
            module_matches.as_slice(),
        )],
        BestVersionMatch::InsufficientEvidence { score } => {
            vec![candidate_diagnostic_from_score("insufficient", score, &[])]
        }
        BestVersionMatch::Ambiguous { scores, .. } => scores
            .into_iter()
            .map(|score| candidate_diagnostic_from_score("ambiguous", score, &[]))
            .collect(),
        BestVersionMatch::NoMatch { scores, .. } => scores
            .into_iter()
            .map(|score| candidate_diagnostic_from_score("no_match", score, &[]))
            .collect(),
    }
}

fn candidate_diagnostic_from_score(
    decision: &'static str,
    score: VersionMatchScore,
    module_matches: &[ModulePackageMatch],
) -> VersionCandidateDiagnostic {
    VersionCandidateDiagnostic {
        package_version: score.package_version,
        decision,
        score: score.score,
        matched_modules: score.matched_modules,
        total_modules: score.total_modules,
        source_hash_matches: score.source_hash_matches,
        function_signature_matches: score.function_signature_matches,
        string_anchor_matches: score.string_anchor_matches,
        module_matches: module_matches.len(),
        external_importable_matches: module_matches
            .iter()
            .filter(|module_match| module_match.external_importable)
            .count(),
    }
}

fn recommendation_for_candidates(
    candidates: &[VersionCandidateDiagnostic],
    db_hints: &BTreeSet<String>,
) -> VersionDiagnosticRecommendation {
    let Some(best) = candidates.first() else {
        return VersionDiagnosticRecommendation::NoCandidates;
    };
    if best.score == 0 || best.matched_modules == 0 {
        return VersionDiagnosticRecommendation::KeepRejected {
            reason: "no candidate version produced usable source evidence".to_string(),
        };
    }
    let runner_up = candidates.get(1);
    let margin = runner_up
        .map(|runner_up| best.score.saturating_sub(runner_up.score))
        .unwrap_or(best.score);
    if best.source_hash_matches > 0 {
        return VersionDiagnosticRecommendation::SourceHash {
            package_version: best.package_version.clone(),
            source_hash_matches: best.source_hash_matches,
        };
    }
    if runner_up.is_some()
        && !db_hints.contains(&best.package_version)
        && best.total_modules >= 2
        && best.matched_modules == best.total_modules
        && margin >= STRUCTURAL_RECOMMENDATION_MIN_MARGIN
    {
        return VersionDiagnosticRecommendation::StructuralMultiModule {
            package_version: best.package_version.clone(),
            margin,
            matched_modules: best.matched_modules,
        };
    }
    VersionDiagnosticRecommendation::KeepRejected {
        reason: keep_rejected_reason(best, runner_up, db_hints, margin),
    }
}

fn keep_rejected_reason(
    best: &VersionCandidateDiagnostic,
    runner_up: Option<&VersionCandidateDiagnostic>,
    db_hints: &BTreeSet<String>,
    margin: u32,
) -> String {
    if runner_up.is_none() && db_hints.contains(&best.package_version) {
        return format!(
            "only the DB-hinted version {} is available and it has no exact source-hash proof",
            best.package_version
        );
    }
    if db_hints.contains(&best.package_version) && best.source_hash_matches == 0 {
        return format!(
            "best candidate {} is still the DB hint and has no exact source-hash proof",
            best.package_version
        );
    }
    format!(
        "best candidate {} has no exact source hash and margin {margin} is below the structural recommendation threshold {STRUCTURAL_RECOMMENDATION_MIN_MARGIN}",
        best.package_version
    )
}

fn print_package_version_diagnostics(
    outcome: &PackageVersionDiagnosticsOutcome,
    top_candidates: usize,
) {
    println!(
        "package version diagnostics for project {}: {} rejected version-mismatch module(s), {} package(s)",
        outcome.project_id,
        outcome.rejected_modules,
        outcome.packages.len()
    );
    for diagnostic in &outcome.packages {
        println!();
        println!(
            "{}: {} rejected module(s), {} candidate version(s)",
            diagnostic.package_name,
            diagnostic.rejected_modules.len(),
            diagnostic.cached_versions.len()
        );
        let hints = diagnostic
            .rejected_modules
            .iter()
            .filter_map(|module| module.db_version_hint.as_deref())
            .collect::<BTreeSet<_>>();
        if !hints.is_empty() {
            println!(
                "  db version hint(s): {}",
                hints.into_iter().collect::<Vec<_>>().join(", ")
            );
        }
        print_recommendation(&diagnostic.recommendation);
        println!(
            "  exact normalized-source hash matches: {}/{} module(s)",
            diagnostic.exact_hash_matches.len(),
            diagnostic.rejected_modules.len()
        );
        for module_match in diagnostic.exact_hash_matches.iter().take(5) {
            let labels = module_match
                .matches
                .iter()
                .map(|source| {
                    format!(
                        "{}:{}:{}",
                        source.package_version,
                        source.source_path,
                        if source.external_importable {
                            "importable"
                        } else {
                            "source-only"
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(" | ");
            println!(
                "    module {} {} -> {}",
                module_match.module_id.0, module_match.semantic_path, labels
            );
        }
        println!("  candidate scores:");
        for candidate in diagnostic.candidates.iter().take(top_candidates) {
            println!(
                "    {}: {} score={} matched={}/{} ({:.2}%) exact_hash={} funcs={} strings={} matches={} importable_matches={}",
                candidate.package_version,
                candidate.decision,
                candidate.score,
                candidate.matched_modules,
                candidate.total_modules,
                pct(candidate.matched_modules, candidate.total_modules),
                candidate.source_hash_matches,
                candidate.function_signature_matches,
                candidate.string_anchor_matches,
                candidate.module_matches,
                candidate.external_importable_matches,
            );
        }
    }
}

fn print_recommendation(recommendation: &VersionDiagnosticRecommendation) {
    match recommendation {
        VersionDiagnosticRecommendation::NoCandidates => {
            println!("  recommendation: keep rejected (no candidate package sources)");
        }
        VersionDiagnosticRecommendation::KeepRejected { reason } => {
            println!("  recommendation: keep rejected ({reason})");
        }
        VersionDiagnosticRecommendation::SourceHash {
            package_version,
            source_hash_matches,
        } => {
            println!(
                "  recommendation: source-verified version {package_version} ({source_hash_matches} exact normalized-source hash match(es))"
            );
        }
        VersionDiagnosticRecommendation::StructuralMultiModule {
            package_version,
            margin,
            matched_modules,
        } => {
            println!(
                "  recommendation: structurally-verified version {package_version} ({matched_modules} module(s), margin {margin})"
            );
        }
    }
}
