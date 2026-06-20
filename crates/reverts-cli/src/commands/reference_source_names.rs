//! `reference-source-names` command: name a decompiled project's modules,
//! exports, and local bindings by matching its emitted TypeScript against a
//! historical first-party source tree. Tier-gated: only provable matches are
//! auto-accepted; everything else is left for an agent.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use clap::{Args, ValueEnum};
use reverts_graph::{FunctionExtractor, extract_import_specifiers, function_names};
use reverts_input::PackageAttributionStatus;
use reverts_ir::{AxisHashes, FunctionFingerprint, ModuleId};
use reverts_package_matcher::{
    GraphNeighborhoodEvidence, SourceFingerprint, build_structural_bag, fingerprint_source,
    graph_neighborhood_support, score_structural_bags,
};
use reverts_pipeline::{generate_project_from_prepared, prepare_and_enrich};
use rusqlite::{Connection, params};

use crate::args::{parse_args_with_name, parse_project_id};
use crate::commands::naming_gates::{
    NamingGateMode, validate_module_path_acceptance, validate_name_acceptance,
};
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

fn plan_modules(
    subjects: &[SubjectModule],
    index: &ReferenceSourceIndex,
) -> Result<Vec<ModulePlan>, CliRunError> {
    let structural_support = source_structural_support(subjects, index);
    let graph_support = source_graph_support(subjects, index, &structural_support);
    let reference_best_subjects =
        best_subject_by_reference(subjects, index, &structural_support, &graph_support);
    let mut plans = Vec::new();
    // Parallel to `plans`: the string anchors each subject shares with its
    // matched reference file. Used to allow many-to-one assignment when esbuild
    // split one source file into modules covering DISJOINT parts.
    let mut shared_anchors: Vec<BTreeSet<String>> = Vec::new();
    for subject in subjects {
        let subject_structural_support = structural_support.get(&subject.module_id);
        let subject_graph_support = graph_support.get(&subject.module_id);
        let Some(matched) = best_module_match_with_reciprocal(
            subject.module_id,
            &subject.fingerprint,
            index,
            &reference_best_subjects,
            subject_structural_support,
            subject_graph_support,
        ) else {
            continue;
        };
        let reference_module = index
            .modules
            .iter()
            .find(|m| m.file_path == matched.file_path);
        let reference_exports = reference_module
            .map(|m| m.export_names.clone())
            .unwrap_or_default();
        let anchors = reference_module
            .map(|m| {
                subject
                    .fingerprint
                    .string_anchors
                    .intersection(&m.fingerprint.string_anchors)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        plans.push(ModulePlan {
            module_id: subject.module_id,
            subject_path: subject.file_path.clone(),
            reference_version: index.version.clone(),
            module_semantic_name: strip_source_extension(&matched.file_path),
            matched,
            subject_bindings: subject.bindings.clone(),
            reference_exports,
        });
        shared_anchors.push(anchors);
    }
    calibrate_global_reference_uniqueness(&mut plans, &shared_anchors);
    plans.sort_by(|a, b| a.module_id.cmp(&b.module_id));
    Ok(plans)
}

/// Near-injective assignment with a many-to-one escape hatch for esbuild
/// splits. Normally a reference file anchors one Medium match: when several
/// subject modules converge on one file, keep the strongest and demote the
/// rest. BUT esbuild often splits one source file into multiple emitted
/// modules covering DISJOINT parts; such modules share *different*, non-empty
/// anchor sets with the file. So after keeping the strongest, also keep any
/// competitor whose shared-anchor set is disjoint from every already-kept
/// member AND that is independently strong (reciprocal-best or high content
/// overlap). The "six modules all claiming utils/debug.ts" false-positive case
/// is excluded: those share the SAME few common anchors (not disjoint) and have
/// weak content, so they still demote. `shared_anchors[i]` parallels `plans[i]`.
fn calibrate_global_reference_uniqueness(
    plans: &mut [ModulePlan],
    shared_anchors: &[BTreeSet<String>],
) {
    let mut medium_by_reference = BTreeMap::<String, Vec<usize>>::new();
    for (index, plan) in plans.iter().enumerate() {
        if plan.matched.tier == MatchTier::Medium {
            medium_by_reference
                .entry(plan.matched.file_path.clone())
                .or_default()
                .push(index);
        }
    }
    for indices in medium_by_reference.values() {
        if indices.len() <= 1 {
            continue;
        }
        // Strongest first: reciprocal-best, then content, then graph, then margin.
        let mut ordered = indices.clone();
        ordered.sort_by(|&a, &b| {
            let left = &plans[a].matched;
            let right = &plans[b].matched;
            right
                .reciprocal_best
                .cmp(&left.reciprocal_best)
                .then(right.normalized_anchor.total_cmp(&left.normalized_anchor))
                .then(right.graph_support.cmp(&left.graph_support))
                .then(right.margin.total_cmp(&left.margin))
        });
        let mut kept_anchors: Vec<&BTreeSet<String>> = Vec::new();
        for &index in &ordered {
            let anchors = &shared_anchors[index];
            let matched = &plans[index].matched;
            let independently_strong =
                matched.reciprocal_best || matched.normalized_anchor >= MEDIUM_NORMALIZED_ANCHOR;
            let covers_distinct_part =
                !anchors.is_empty() && kept_anchors.iter().all(|kept| kept.is_disjoint(anchors));
            if kept_anchors.is_empty() {
                kept_anchors.push(anchors); // strongest is always kept
            } else if covers_distinct_part && independently_strong {
                kept_anchors.push(anchors); // esbuild split: a different part of the file
            } else {
                plans[index].matched.tier = MatchTier::Low;
            }
        }
    }
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
    let index = build_reference_source_index(&args.reference_source_root, &args.reference_version)
        .map_err(CliRunError::ReferenceSourceNames)?;
    let subjects = subject_modules(&args)?;
    let plans = plan_modules(&subjects, &index)?;
    println!(
        "module_id\tsubject_path\tref_version\tref_file\ttier\tsemantic_name\tasset\texport\tfn\tstruct\tgraph\tgraph_known\tanchor\twanchor\tnanchor\tmargin\treciprocal"
    );
    for plan in &plans {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.1}\t{}\t{}\t{}\t{:.1}\t{:.3}\t{:.3}\t{}",
            plan.module_id,
            plan.subject_path,
            plan.reference_version,
            plan.matched.file_path,
            tier_str(plan.matched.tier),
            plan.module_semantic_name,
            plan.matched.asset_overlap,
            plan.matched.export_overlap,
            plan.matched.function_overlap,
            plan.matched.structural_score,
            plan.matched.graph_support,
            plan.matched.graph_known_edges,
            plan.matched.anchor_overlap,
            plan.matched.weighted_anchor,
            plan.matched.normalized_anchor,
            plan.matched.margin,
            if plan.matched.reciprocal_best {
                "yes"
            } else {
                "no"
            },
        );
    }
    // Function-level pass: GLOBAL match over the whole corpus. Auto-accepts are
    // corroborated by the module match (function's reference file == the file
    // its module matched); everything else is a global proposal.
    let module_matched_file: BTreeMap<u32, String> = plans
        .iter()
        .filter(|plan| tier_passes(plan.matched.tier, args.min_tier))
        .map(|plan| (plan.module_id, plan.matched.file_path.clone()))
        .collect();
    let binding_rows = match_functions_globally(&subjects, &index, &module_matched_file);
    println!("---bindings---");
    println!(
        "module_id\tsubject_path\tref_file\toriginal\tsemantic\tkind\tast\tparams\tstmts\tscore"
    );
    for row in &binding_rows {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{:016x}\t{}\t{}\t{:.1}",
            row.module_id,
            row.subject_path,
            row.reference_file,
            row.original_name,
            row.semantic_name,
            if row.accepted { "accepted" } else { "proposal" },
            row.ast_hash,
            row.param_count,
            row.statement_count,
            row.score,
        );
    }

    if args.apply {
        let connection = Connection::open(&args.input)
            .map_err(|error| CliRunError::ReferenceSourceNames(error.to_string()))?;
        ensure_semantic_name_source_column(&connection)
            .map_err(|e| CliRunError::ReferenceSourceNames(e.to_string()))?;
        ensure_symbol_name_proposals_table(&connection)
            .map_err(|e| CliRunError::ReferenceSourceNames(e.to_string()))?;
        crate::commands::binding_names::ensure_binding_names_table_if_writable(&connection, true)?;
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
        let (binding_accepted, binding_proposed) = write_binding_names(
            &connection,
            args.project_id,
            &binding_rows,
            &args.origin_prefix,
            &args.reference_version,
        )?;
        println!(
            "applied: {module_count} module name(s), {export_count} export name(s), {binding_accepted} binding rename(s), {binding_proposed} binding proposal(s)"
        );
    } else {
        println!(
            "dry-run: {} module match(es); pass --apply to write",
            plans.len()
        );
    }
    Ok(())
}

/// One subject emitted module: its DB module id, emitted path, fingerprint,
/// and the (original_name -> emitted_name) bindings that land in it.
struct SubjectModule {
    module_id: u32,
    file_path: String,
    source: String,
    fingerprint: SourceFingerprint,
    bindings: Vec<(String, String)>, // (original_name, emitted_name)
}

fn subject_modules(args: &ReferenceSourceNamesArgs) -> Result<Vec<SubjectModule>, CliRunError> {
    let bundle = load_project_bundle_with_package_externalization(&args.input, args.project_id)
        .map_err(|error| CliRunError::ReferenceSourceNames(format!("load input: {error}")))?;
    let package_owned_modules = bundle
        .package_attributions
        .iter()
        .filter(|attribution| {
            matches!(
                attribution.status,
                PackageAttributionStatus::Accepted | PackageAttributionStatus::Rejected
            ) && attribution.package_version.is_some()
        })
        .map(|attribution| attribution.module_id.0)
        .collect::<BTreeSet<_>>();
    let prepared = prepare_and_enrich(bundle)
        .map_err(|error| CliRunError::ReferenceSourceNames(format!("prepare: {error}")))?;
    let run = generate_project_from_prepared(prepared)
        .map_err(|error| CliRunError::ReferenceSourceNames(format!("generate: {error}")))?;

    // Group symbol_index bindings by emitted file path. The owning module map
    // comes from the pipeline's module_output_paths, not from symbol_index:
    // symbol-less modules (data/string modules, side-effect modules, tiny
    // wrappers) still need source/package matching coverage.
    let mut bindings_for_path: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for entry in &run.symbol_index {
        if package_owned_modules.contains(&entry.module_id.0) {
            continue;
        }
        bindings_for_path
            .entry(entry.file_path.clone())
            .or_default()
            .push((entry.original_name.clone(), entry.emitted_name.clone()));
    }
    let module_for_path = run
        .module_output_paths
        .iter()
        .filter(|(module_id, _)| !package_owned_modules.contains(&module_id.0))
        .map(|(module_id, path)| (path.clone(), module_id.0))
        .collect::<BTreeMap<_, _>>();

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
            source: file.source.clone(),
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
    pub source: String,
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
    /// Inverse-document-frequency weight per string anchor: `ln(N / df)` where
    /// `df` is the number of reference modules containing the anchor. Rare,
    /// distinctive anchors get a high weight; common "hub" anchors (present in
    /// many files) get ~0, so they no longer forge matches via raw overlap.
    pub anchor_idf: std::collections::BTreeMap<String, f64>,
}

use std::path::Path;

const SOURCE_EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"];
const SKIP_DIRS: &[&str] = &["node_modules", "test", "tests", "__tests__", "coverage"];

pub(crate) fn build_reference_source_index(
    root: &Path,
    version: &str,
) -> Result<ReferenceSourceIndex, String> {
    let mut files = Vec::new();
    collect_source_files(root, &mut files)?;
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
            continue; // unparseable reference file - skip, do not guess
        };
        let (export_names, asset_literals) = classify_anchors(&fingerprint);
        modules.push(ReferenceSourceModule {
            file_path: relative,
            source,
            fingerprint,
            export_names,
            asset_literals,
        });
    }
    let anchor_idf = compute_anchor_idf(&modules);
    Ok(ReferenceSourceIndex {
        version: version.to_string(),
        modules,
        anchor_idf,
    })
}

/// Build the per-anchor IDF weight over the reference modules. `ln(N / df)`
/// (with `df >= 1`) so an anchor in 1 of N modules weighs `ln(N)` and an anchor
/// in every module weighs ~0.
fn compute_anchor_idf(
    modules: &[ReferenceSourceModule],
) -> std::collections::BTreeMap<String, f64> {
    let n = modules.len().max(1) as f64;
    let mut df: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    for module in modules {
        for anchor in &module.fingerprint.string_anchors {
            *df.entry(anchor.clone()).or_insert(0) += 1;
        }
    }
    df.into_iter()
        .map(|(anchor, count)| (anchor, (n / f64::from(count)).ln()))
        .collect()
}

fn source_structural_support(
    subjects: &[SubjectModule],
    index: &ReferenceSourceIndex,
) -> BTreeMap<u32, BTreeMap<String, f64>> {
    let reference_bags = index
        .modules
        .iter()
        .filter_map(|module| {
            let fingerprints = FunctionExtractor::fingerprint(ModuleId(0), module.source.as_str());
            let bag = build_structural_bag(&fingerprints)?;
            let self_score = score_structural_bags(&bag, &bag).unwrap_or(0.0);
            Some((module.file_path.as_str(), (bag, self_score)))
        })
        .collect::<BTreeMap<_, _>>();
    let reference_by_path = index
        .modules
        .iter()
        .map(|module| (module.file_path.as_str(), module))
        .collect::<BTreeMap<_, _>>();
    let mut support = BTreeMap::<u32, BTreeMap<String, f64>>::new();
    for subject in subjects {
        let fingerprints =
            FunctionExtractor::fingerprint(ModuleId(subject.module_id), subject.source.as_str());
        let Some(subject_bag) = build_structural_bag(&fingerprints) else {
            continue;
        };
        let subject_self = score_structural_bags(&subject_bag, &subject_bag).unwrap_or(0.0);
        // Use the cheap anchor/export/hash scorer as a candidate generator,
        // then reuse the package matcher's structural-bag scorer only on the
        // short list. Full subject x reference cascade is too expensive and
        // noisy; structural scoring is evidence refinement, not discovery.
        for module in ranked_module_matches(&subject.fingerprint, index, None, None)
            .into_iter()
            .take(SOURCE_STRUCTURAL_CANDIDATE_LIMIT)
            .filter_map(|candidate| reference_by_path.get(candidate.matched.file_path.as_str()))
        {
            let Some((reference_bag, reference_self)) =
                reference_bags.get(module.file_path.as_str())
            else {
                continue;
            };
            let Some(score) = score_structural_bags(&subject_bag, reference_bag) else {
                continue;
            };
            // Cosine-normalize structural overlap by both bags' self-scores, so a
            // large reference file no longer structurally "matches" every module
            // (raw score is magnitude-biased — the same hub failure mode that raw
            // anchor overlap had before `normalized_anchor`).
            let normalized = if subject_self > f64::EPSILON && *reference_self > f64::EPSILON {
                (score / (subject_self * reference_self).sqrt()).clamp(0.0, 1.0)
            } else {
                0.0
            };
            support
                .entry(subject.module_id)
                .or_default()
                .insert(module.file_path.clone(), normalized);
        }
    }
    support
}

/// Best ref-file assignment for every subject given the current structural and
/// graph support. Returns `subject_id -> ref_path`; this is the propagation
/// state that grows each round. (Reciprocal-best only affects tier, not the
/// selected path, so an empty map is fine here.)
fn graph_seed_assignment(
    subjects: &[SubjectModule],
    index: &ReferenceSourceIndex,
    structural_support: &BTreeMap<u32, BTreeMap<String, f64>>,
    graph_support: &BTreeMap<u32, BTreeMap<String, GraphEvidence>>,
) -> BTreeMap<u32, String> {
    subjects
        .iter()
        .filter_map(|subject| {
            let matched = best_module_match_with_reciprocal(
                subject.module_id,
                &subject.fingerprint,
                index,
                &BTreeMap::new(),
                structural_support.get(&subject.module_id),
                graph_support.get(&subject.module_id),
            )?;
            Some((subject.module_id, matched.file_path))
        })
        .collect::<BTreeMap<_, _>>()
}

fn source_graph_support(
    subjects: &[SubjectModule],
    index: &ReferenceSourceIndex,
    structural_support: &BTreeMap<u32, BTreeMap<String, f64>>,
) -> BTreeMap<u32, BTreeMap<String, GraphEvidence>> {
    // The dependency graphs are invariant across propagation rounds, so build
    // them once. Only `graph_neighborhood_support` (which reads the evolving
    // assignment) is recomputed each round.
    let subject_paths = subjects
        .iter()
        .map(|subject| subject.file_path.as_str())
        .collect::<BTreeSet<_>>();
    let subject_id_by_path = subjects
        .iter()
        .map(|subject| (subject.file_path.as_str(), subject.module_id))
        .collect::<BTreeMap<_, _>>();
    let reference_paths = index
        .modules
        .iter()
        .map(|module| module.file_path.as_str())
        .collect::<BTreeSet<_>>();

    let subject_deps = subjects
        .iter()
        .map(|subject| {
            let deps = extract_import_specifiers(subject.source.as_str())
                .into_iter()
                .filter_map(|specifier| {
                    resolve_relative_source_path(
                        subject.file_path.as_str(),
                        specifier.as_str(),
                        &subject_paths,
                    )
                })
                .filter_map(|path| subject_id_by_path.get(path.as_str()).copied())
                .collect::<BTreeSet<_>>();
            (subject.module_id, deps)
        })
        .collect::<BTreeMap<_, _>>();
    let reference_deps = index
        .modules
        .iter()
        .map(|module| {
            let deps = extract_import_specifiers(module.source.as_str())
                .into_iter()
                .filter_map(|specifier| {
                    resolve_relative_source_path(
                        module.file_path.as_str(),
                        specifier.as_str(),
                        &reference_paths,
                    )
                })
                .collect::<BTreeSet<_>>();
            (module.file_path.clone(), deps)
        })
        .collect::<BTreeMap<_, _>>();
    let subject_incoming = reverse_id_graph(&subject_deps);
    let reference_incoming = reverse_path_graph(&reference_deps);

    // Iterative label propagation. Seed from content + structural only (no
    // graph), then feed each round's graph-aware assignment back as the seed so
    // confidently-placed modules anchor their neighbors. Stops at a fixpoint.
    let mut assignment =
        graph_seed_assignment(subjects, index, structural_support, &BTreeMap::new());
    let mut support = BTreeMap::new();
    for _ in 0..MAX_PROPAGATION_ROUNDS {
        if assignment.is_empty() {
            break;
        }
        support = graph_neighborhood_support(
            &subject_deps,
            &subject_incoming,
            &reference_deps,
            &reference_incoming,
            &assignment,
        );
        let next = graph_seed_assignment(subjects, index, structural_support, &support);
        if next == assignment {
            break;
        }
        assignment = next;
    }
    support
}

fn reverse_id_graph(graph: &BTreeMap<u32, BTreeSet<u32>>) -> BTreeMap<u32, BTreeSet<u32>> {
    let mut reverse = BTreeMap::<u32, BTreeSet<u32>>::new();
    for (source, targets) in graph {
        for target in targets {
            reverse.entry(*target).or_default().insert(*source);
        }
    }
    reverse
}

fn reverse_path_graph(
    graph: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut reverse = BTreeMap::<String, BTreeSet<String>>::new();
    for (source, targets) in graph {
        for target in targets {
            reverse
                .entry(target.clone())
                .or_default()
                .insert(source.clone());
        }
    }
    reverse
}

fn resolve_relative_source_path(
    importer: &str,
    specifier: &str,
    known_paths: &BTreeSet<&str>,
) -> Option<String> {
    let specifier = specifier.split(['?', '#']).next().unwrap_or(specifier);
    let candidate_root = if specifier.starts_with('.') {
        let base_dir = importer.rsplit_once('/').map_or("", |(dir, _)| dir);
        let joined = if base_dir.is_empty() {
            specifier.to_string()
        } else {
            format!("{base_dir}/{specifier}")
        };
        normalize_relative_path(joined.as_str())?
    } else if let Some(stripped) = specifier.strip_prefix("src/") {
        // Claude Code's published source map tree uses both relative imports
        // and root-relative `src/...` imports. Treat those aliases as paths
        // relative to the reference source root; package/builtin imports still
        // fall through because they will not resolve to a known source path.
        stripped.to_string()
    } else {
        specifier.to_string()
    };
    source_path_candidates(candidate_root.as_str())
        .into_iter()
        .find(|candidate| known_paths.contains(candidate.as_str()))
}

fn normalize_relative_path(path: &str) -> Option<String> {
    let mut parts = Vec::<&str>::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

fn source_path_candidates(path: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    candidates.push(path.to_string());
    if let Some(stripped) = path
        .strip_suffix(".js")
        .or_else(|| path.strip_suffix(".mjs"))
        .or_else(|| path.strip_suffix(".cjs"))
    {
        for ext in ["ts", "tsx", "js", "jsx", "mjs", "cjs"] {
            candidates.push(format!("{stripped}.{ext}"));
        }
    } else if Path::new(path).extension().is_none() {
        for ext in SOURCE_EXTENSIONS {
            candidates.push(format!("{path}.{ext}"));
        }
        for ext in SOURCE_EXTENSIONS {
            candidates.push(format!("{path}/index.{ext}"));
        }
    }
    candidates
}

fn collect_source_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> Result<(), String> {
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
            collect_source_files(&path, out)?;
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
            // Require a non-empty stem before `.node`: a bare `".node"` extension
            // string (common in file-type lists) is NOT a native-asset reference
            // and must not produce a spurious High-tier asset match.
            if basename
                .strip_suffix(".node")
                .is_some_and(|stem| !stem.is_empty())
            {
                assets.insert(basename.to_string());
            }
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

/// IDF-weighted anchor mass (sum of `idf`) required to treat string-anchor
/// overlap as Medium-tier evidence. Calibrated on the 2.1.89 to claude-code/src
/// dataset: real first-party matches (e.g. the PowerShellTool cluster) carry
/// many rare shared anchors and clear this comfortably, while hub files that
/// coincidentally share common anchors with many modules stay below it.
const MEDIUM_WEIGHTED_ANCHOR: f64 = 30.0;
const MEDIUM_NORMALIZED_ANCHOR: f64 = 0.18;
const MEDIUM_SCORE_MARGIN: f64 = 0.20;
/// Strong normalized content overlap promotes to Medium on its own, regardless
/// of absolute IDF mass. This recovers SMALL first-party modules (few anchors
/// but a high fraction shared — e.g. a tiny module sharing 3 distinctive tokens
/// at nanchor=0.5) that the `weighted_anchor >= 30` mass gate misses. Normalized
/// overlap is hub-resistant, so this does not re-admit large-file false matches.
const MEDIUM_STRONG_NANCHOR: f64 = 0.30;
const SOURCE_STRUCTURAL_CANDIDATE_LIMIT: usize = 12;
// Structural score is now cosine-normalized to [0,1] (see source_structural_support).
// Used as ranking evidence and as a Medium criterion only WITH anchor
// corroboration — standalone structural promotion was tried and reverted because
// even normalized cosine is high for structurally-generic hub files (main.tsx,
// mcp/client, bootstrap/state), which it admitted as false positives.
const MEDIUM_STRUCTURAL_SCORE: f64 = 0.35;
const MEDIUM_STRUCTURAL_WEIGHTED_ANCHOR: f64 = 10.0;
const MEDIUM_STRUCTURAL_NORMALIZED_ANCHOR: f64 = 0.08;
const MEDIUM_GRAPH_SUPPORT: usize = 1;
/// Max rounds of graph label-propagation. Each round feeds the previous round's
/// graph-aware assignment back as the seed, so confidently-matched modules
/// anchor their neighbors and the match set grows across the dependency graph.
/// Converges early when the assignment stops changing; 4 is an upper bound.
const MAX_PROPAGATION_ROUNDS: usize = 8;
/// Minimum normalized-anchor (content) corroboration required for the
/// otherwise-content-free Medium criteria — `export>=2 || function>=2` and the
/// graph-all-edges-matched promotion. Without it, coincidental function/export
/// hashes or a trivial 2-edge "all dependencies matched" forge a Medium with
/// zero content overlap (measured on 2.1.89: ~30 of 130 mediums, e.g. six
/// unrelated modules all "matching" `utils/debug.ts`, three "matching"
/// `cli/print.ts`, all at nanchor=0). Content is the true/false discriminator.
const MEDIUM_CONTENT_NORMALIZED_FLOOR: f64 = 0.05;

type GraphEvidence = GraphNeighborhoodEvidence;

#[derive(Debug, Clone)]
pub(crate) struct ModuleMatch {
    pub file_path: String,
    pub tier: MatchTier,
    pub asset_overlap: usize,
    pub export_overlap: usize,
    pub function_overlap: usize,
    /// Aggregate structural-bag score produced by the shared package matcher
    /// scorer. This reuses package matcher matching mechanics for first-party
    /// source matching instead of maintaining a separate, weaker source-only
    /// matcher.
    pub structural_score: f64,
    /// Count of matched outgoing/incoming neighbor edges between the subject
    /// graph and the reference graph. This is the source-side analogue of
    /// package matcher dependency-neighborhood promotion.
    pub graph_support: usize,
    /// Number of subject neighbor edges that had preliminary source matches and
    /// therefore could participate in graph-neighborhood evidence.
    pub graph_known_edges: usize,
    pub anchor_overlap: usize,
    /// Sum of per-anchor IDF over shared string anchors - the size/hub-robust
    /// similarity signal that drives ranking and the anchor tier promotion.
    pub weighted_anchor: f64,
    /// Cosine-like normalized IDF overlap. This is the hub penalty: a large
    /// reference file with many anchors must share a meaningful fraction of its
    /// weighted surface, not just many raw tokens.
    pub normalized_anchor: f64,
    /// Relative distance from the runner-up candidate: `(top - runner_up) / top`.
    /// A high margin raises confidence; a low margin keeps non-provable matches
    /// in the Low bucket for agent review.
    pub margin: f64,
    /// Whether this module is also the best subject for the selected reference
    /// file. Reciprocal-best is strong evidence for first-party source matches
    /// and suppresses hub files that attract many unrelated subjects.
    pub reciprocal_best: bool,
}

#[derive(Debug, Clone)]
struct RankedModuleMatch {
    relevance: f64,
    matched: ModuleMatch,
}

#[derive(Debug, Clone, Copy)]
struct MatchEvidence {
    hash_match: bool,
    asset_overlap: usize,
    export_overlap: usize,
    function_overlap: usize,
    structural_score: f64,
    graph: GraphEvidence,
    weighted_anchor: f64,
    normalized_anchor: f64,
}

fn overlap_len(left: &BTreeSet<String>, right: &BTreeSet<String>) -> usize {
    left.intersection(right).count()
}

/// Sum of IDF weights over the anchors shared by `subject` and `reference`.
fn weighted_anchor_overlap(
    subject: &BTreeSet<String>,
    reference: &BTreeSet<String>,
    anchor_idf: &std::collections::BTreeMap<String, f64>,
) -> f64 {
    subject
        .intersection(reference)
        .map(|anchor| anchor_idf.get(anchor).copied().unwrap_or(0.0))
        .sum()
}

fn weighted_anchor_mass(
    anchors: &BTreeSet<String>,
    anchor_idf: &std::collections::BTreeMap<String, f64>,
) -> f64 {
    anchors
        .iter()
        .map(|anchor| anchor_idf.get(anchor).copied().unwrap_or(0.0))
        .sum()
}

fn normalized_anchor_overlap(
    subject: &BTreeSet<String>,
    reference: &BTreeSet<String>,
    anchor_idf: &std::collections::BTreeMap<String, f64>,
    weighted_anchor: f64,
) -> f64 {
    let subject_mass = weighted_anchor_mass(subject, anchor_idf);
    let reference_mass = weighted_anchor_mass(reference, anchor_idf);
    if subject_mass <= f64::EPSILON || reference_mass <= f64::EPSILON {
        0.0
    } else {
        weighted_anchor / (subject_mass * reference_mass).sqrt()
    }
}

fn candidate_relevance(evidence: MatchEvidence) -> f64 {
    (if evidence.hash_match { 1.0e9 } else { 0.0 })
        + (evidence.asset_overlap as f64) * 1.0e6
        + evidence.normalized_anchor * 150.0
        + evidence.weighted_anchor
        + (evidence.export_overlap as f64) * 8.0
        + (evidence.function_overlap as f64) * 4.0
        + evidence.structural_score * 120.0
        + (evidence.graph.matched_edges as f64) * 35.0
        + evidence.graph.coverage() * 75.0
}

fn raw_match_tier(evidence: MatchEvidence) -> MatchTier {
    if evidence.hash_match || evidence.asset_overlap >= 1 {
        MatchTier::High
    } else if ((evidence.export_overlap >= 2 || evidence.function_overlap >= 2)
        && evidence.normalized_anchor >= MEDIUM_CONTENT_NORMALIZED_FLOOR)
        || (evidence.structural_score >= MEDIUM_STRUCTURAL_SCORE
            && evidence.weighted_anchor >= MEDIUM_STRUCTURAL_WEIGHTED_ANCHOR
            && evidence.normalized_anchor >= MEDIUM_STRUCTURAL_NORMALIZED_ANCHOR)
        || (evidence.graph.matched_edges >= MEDIUM_GRAPH_SUPPORT
            && evidence.weighted_anchor >= MEDIUM_STRUCTURAL_WEIGHTED_ANCHOR
            && evidence.normalized_anchor >= MEDIUM_STRUCTURAL_NORMALIZED_ANCHOR)
        // Strong graph placement: 3+ matched dependency edges AND a majority
        // (>=60%) of known edges matched. Content-poor first-party modules get
        // placed by their position in the (shared) import graph; requiring a
        // majority rather than ALL edges lets placement cascade across iteration
        // rounds. The injective 1:1 pass downstream resolves any contention.
        || (evidence.graph.matched_edges >= 3
            && evidence.graph.matched_edges * 5 >= evidence.graph.known_edges * 3)
        || (evidence.graph.known_edges >= 2
            && evidence.graph.matched_edges == evidence.graph.known_edges
            && evidence.normalized_anchor >= MEDIUM_CONTENT_NORMALIZED_FLOOR)
        || (evidence.weighted_anchor >= MEDIUM_WEIGHTED_ANCHOR
            && evidence.normalized_anchor >= MEDIUM_NORMALIZED_ANCHOR)
        || evidence.normalized_anchor >= MEDIUM_STRONG_NANCHOR
    {
        MatchTier::Medium
    } else {
        MatchTier::Low
    }
}

fn calibrate_tier(
    tier: MatchTier,
    margin: f64,
    reciprocal_best: bool,
    normalized_anchor: f64,
) -> MatchTier {
    match tier {
        MatchTier::High | MatchTier::Low => tier,
        // Strong normalized content is self-sufficient: keep it Medium even
        // without reciprocal-best or a wide margin. The margin/reciprocal gate
        // exists to suppress weak/ambiguous matches, not strong-content ones.
        MatchTier::Medium
            if reciprocal_best
                || margin >= MEDIUM_SCORE_MARGIN
                || normalized_anchor >= MEDIUM_STRONG_NANCHOR =>
        {
            tier
        }
        MatchTier::Medium => MatchTier::Low,
    }
}

fn ranked_module_matches(
    subject: &SourceFingerprint,
    index: &ReferenceSourceIndex,
    structural_support: Option<&BTreeMap<String, f64>>,
    graph_support: Option<&BTreeMap<String, GraphEvidence>>,
) -> Vec<RankedModuleMatch> {
    let (subject_exports, subject_assets) = classify_anchors(subject);
    let mut ranked = Vec::new();
    for module in &index.modules {
        let asset_overlap = overlap_len(&subject_assets, &module.asset_literals);
        let export_overlap = overlap_len(&subject_exports, &module.export_names);
        let function_overlap = overlap_len(
            &subject.function_signature_hashes,
            &module.fingerprint.function_signature_hashes,
        );
        let anchor_overlap =
            overlap_len(&subject.string_anchors, &module.fingerprint.string_anchors);
        let structural_score = structural_support
            .and_then(|support| support.get(module.file_path.as_str()).copied())
            .unwrap_or(0.0);
        let graph = graph_support
            .and_then(|support| support.get(module.file_path.as_str()).copied())
            .unwrap_or_default();
        let weighted_anchor = weighted_anchor_overlap(
            &subject.string_anchors,
            &module.fingerprint.string_anchors,
            &index.anchor_idf,
        );
        let normalized_anchor = normalized_anchor_overlap(
            &subject.string_anchors,
            &module.fingerprint.string_anchors,
            &index.anchor_idf,
            weighted_anchor,
        );
        let hash_match = !subject
            .normalized_source_hashes
            .is_disjoint(&module.fingerprint.normalized_source_hashes);
        // Reject candidates with no real evidence. `weighted_anchor` (not raw
        // overlap) is the floor signal so a handful of common shared anchors no
        // longer qualifies; Low tier is never auto-accepted, so this only
        // affects the dry-run report.
        if !hash_match
            && asset_overlap == 0
            && export_overlap == 0
            && function_overlap == 0
            && structural_score < 1.0
            && graph.matched_edges == 0
            && weighted_anchor < 1.0
        {
            continue;
        }
        let evidence = MatchEvidence {
            hash_match,
            asset_overlap,
            export_overlap,
            function_overlap,
            structural_score,
            graph,
            weighted_anchor,
            normalized_anchor,
        };
        let tier = raw_match_tier(evidence);
        let relevance = candidate_relevance(evidence);
        ranked.push(RankedModuleMatch {
            relevance,
            matched: ModuleMatch {
                file_path: module.file_path.clone(),
                tier,
                asset_overlap,
                export_overlap,
                function_overlap,
                structural_score,
                graph_support: graph.matched_edges,
                graph_known_edges: graph.known_edges,
                anchor_overlap,
                weighted_anchor,
                normalized_anchor,
                margin: 0.0,
                reciprocal_best: false,
            },
        });
    }
    ranked.sort_by(|left, right| {
        right
            .relevance
            .total_cmp(&left.relevance)
            .then_with(|| left.matched.file_path.cmp(&right.matched.file_path))
    });
    ranked
}

#[cfg(test)]
pub(crate) fn best_module_match(
    subject: &SourceFingerprint,
    index: &ReferenceSourceIndex,
) -> Option<ModuleMatch> {
    let ranked = ranked_module_matches(subject, index, None, None);
    let mut best = ranked.first()?.matched.clone();
    best.margin = match (ranked.first(), ranked.get(1)) {
        (Some(top), Some(runner_up)) if top.relevance > f64::EPSILON => {
            (top.relevance - runner_up.relevance).max(0.0) / top.relevance
        }
        (Some(_), None) => 1.0,
        _ => 0.0,
    };
    best.tier = calibrate_tier(
        best.tier,
        best.margin,
        best.reciprocal_best,
        best.normalized_anchor,
    );
    Some(best)
}

fn best_module_match_with_reciprocal(
    subject_id: u32,
    subject: &SourceFingerprint,
    index: &ReferenceSourceIndex,
    reference_best_subjects: &BTreeMap<String, u32>,
    structural_support: Option<&BTreeMap<String, f64>>,
    graph_support: Option<&BTreeMap<String, GraphEvidence>>,
) -> Option<ModuleMatch> {
    let ranked = ranked_module_matches(subject, index, structural_support, graph_support);
    let mut matched = ranked.first()?.matched.clone();
    matched.margin = match (ranked.first(), ranked.get(1)) {
        (Some(top), Some(runner_up)) if top.relevance > f64::EPSILON => {
            (top.relevance - runner_up.relevance).max(0.0) / top.relevance
        }
        (Some(_), None) => 1.0,
        _ => 0.0,
    };
    matched.reciprocal_best = reference_best_subjects
        .get(matched.file_path.as_str())
        .is_some_and(|best_subject_id| *best_subject_id == subject_id);
    matched.tier = calibrate_tier(
        matched.tier,
        matched.margin,
        matched.reciprocal_best,
        matched.normalized_anchor,
    );
    Some(matched)
}

fn best_subject_by_reference(
    subjects: &[SubjectModule],
    index: &ReferenceSourceIndex,
    structural_support_by_subject: &BTreeMap<u32, BTreeMap<String, f64>>,
    graph_support_by_subject: &BTreeMap<u32, BTreeMap<String, GraphEvidence>>,
) -> BTreeMap<String, u32> {
    let mut best = BTreeMap::<String, (f64, u32)>::new();
    for subject in subjects {
        for candidate in ranked_module_matches(
            &subject.fingerprint,
            index,
            structural_support_by_subject.get(&subject.module_id),
            graph_support_by_subject.get(&subject.module_id),
        ) {
            best.entry(candidate.matched.file_path)
                .and_modify(|current| {
                    if candidate.relevance > current.0
                        || (candidate.relevance == current.0 && subject.module_id < current.1)
                    {
                        *current = (candidate.relevance, subject.module_id);
                    }
                })
                .or_insert((candidate.relevance, subject.module_id));
        }
    }
    best.into_iter()
        .map(|(file_path, (_score, module_id))| (file_path, module_id))
        .collect()
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
    ensure_symbol_name_proposals_table(connection)
        .map_err(|error| CliRunError::ReferenceSourceNames(error.to_string()))?;
    let subject_originals: BTreeSet<&str> = subject_bindings
        .iter()
        .map(|(orig, _)| orig.as_str())
        .collect();
    let mut written = 0;
    for export in reference_exports {
        if !subject_originals.contains(export.as_str()) {
            continue; // not an unambiguous 1:1 - leave for agent
        }
        let evidence = "{\"tier\":\"export-exact\"}";
        validate_name_acceptance(
            export.as_str(),
            export.as_str(),
            origin,
            Some(evidence),
            NamingGateMode::Symbol,
        )
        .map_err(|error| CliRunError::ReferenceSourceNames(error.message()))?;
        connection
            .execute(
                r"
                INSERT INTO symbol_name_proposals (
                    project_id, module_id, original_name, semantic_name, origin, accepted,
                    evidence, gate_status, gate_reason
                ) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, 'passed', 'deterministic-gates-passed')
                ON CONFLICT(project_id, module_id, original_name, origin, semantic_name)
                DO UPDATE SET accepted = excluded.accepted,
                    evidence = COALESCE(excluded.evidence, symbol_name_proposals.evidence),
                    gate_status = excluded.gate_status,
                    gate_reason = excluded.gate_reason
                ",
                params![
                    i64::from(project_id),
                    i64::from(module_id),
                    export.as_str(),
                    export.as_str(),
                    origin,
                    evidence,
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

/// Number of ranked reference-function candidates emitted as proposals for each
/// unaccepted subject function. Proposals are never written as `accepted`.
const BINDING_PROPOSAL_TOP_K: usize = 3;

/// One function-binding naming decision derived from a matched module pair:
/// rename the minified subject binding `original_name` to the reference
/// function's `semantic_name`. `accepted` rows are provable (unique α-rename
/// AST-hash match + param/statement corroboration); the rest are proposals.
#[derive(Debug, Clone, PartialEq)]
struct BindingNameRow {
    module_id: u32,
    subject_path: String,
    reference_file: String,
    original_name: String,
    semantic_name: String,
    accepted: bool,
    ast_hash: u64,
    param_count: u32,
    statement_count: u32,
    score: f64,
}

impl BindingNameRow {
    fn evidence(&self) -> String {
        if self.accepted {
            format!(
                "{{\"tier\":\"fn-hash-unique\",\"ast\":\"{:016x}\",\"params\":{},\"stmts\":{}}}",
                self.ast_hash, self.param_count, self.statement_count
            )
        } else {
            format!(
                "{{\"tier\":\"fn-hash-proposal\",\"ast\":\"{:016x}\",\"params\":{},\"stmts\":{},\"score\":{:.1}}}",
                self.ast_hash, self.param_count, self.statement_count, self.score
            )
        }
    }
}

/// Whether a reference function name is specific enough to propagate. The
/// reference tree is itself a reconstruction, so some functions carry
/// placeholder names (`_temp1`, `t8`, `e3`) or fully generic ones (`get`,
/// `init`). Propagating those to a subject binding adds noise, not meaning —
/// the same rationale as `is_specific_export_member` for exports.
fn is_specific_reference_name(name: &str) -> bool {
    let name = name.trim();
    if name.len() < 3 {
        return false;
    }
    // `_temp`, `_temp1`, `__temp2`, …
    let unprefixed = name.trim_start_matches('_');
    if let Some(rest) = unprefixed.strip_prefix("temp")
        && rest.chars().all(|c| c.is_ascii_digit())
    {
        return false;
    }
    // `_0`, `__12` — underscore-prefixed numeric temporaries.
    if !unprefixed.is_empty() && unprefixed.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    // single letter + digits: `t8`, `e10`, `x5` (decompiler temporaries).
    let mut chars = name.chars();
    if let Some(first) = chars.next()
        && first.is_ascii_alphabetic()
        && chars.clone().count() >= 1
        && chars.all(|c| c.is_ascii_digit())
    {
        return false;
    }
    !matches!(
        name,
        "get"
            | "set"
            | "has"
            | "map"
            | "run"
            | "main"
            | "init"
            | "name"
            | "type"
            | "value"
            | "index"
            | "data"
            | "args"
            | "props"
            | "item"
            | "node"
            | "callback"
            | "handler"
            | "fn"
    )
}

/// The set of α-rename-invariant AST hashes a function carries (primary +
/// every alternate normalization pass). Two functions sharing any hash are
/// structurally related across builds.
fn function_ast_hashes(fingerprint: &FunctionFingerprint) -> BTreeSet<u64> {
    let mut hashes = BTreeSet::new();
    hashes.insert(fingerprint.primary.ast);
    for alternate in &fingerprint.alternates {
        hashes.insert(alternate.axes.ast);
    }
    hashes
}

/// Composite signature folding ALL of a function's structural axes (control
/// flow, return/effect/throw patterns, callee set, binding/access shapes, …),
/// not just the `ast` axis. Two functions with an identical composite are
/// byte-for-byte structurally the same across every axis — strong enough to
/// auto-accept WITHOUT a module match, because the structural collisions that
/// plague single-axis `ast` matching (`resolve` vs `isFollowUpDigit`) differ on
/// the other axes and so get distinct composites.
fn composite_signature(axes: &AxisHashes) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    axes.ast.hash(&mut hasher);
    axes.cfg.hash(&mut hasher);
    axes.return_pattern.hash(&mut hasher);
    axes.effect_pattern.hash(&mut hasher);
    axes.structural_anchor.hash(&mut hasher);
    axes.binding_pattern.hash(&mut hasher);
    axes.literal_anchor.hash(&mut hasher);
    axes.access_pattern.hash(&mut hasher);
    axes.literal_shape.hash(&mut hasher);
    axes.access_shape.hash(&mut hasher);
    axes.callee_set.hash(&mut hasher);
    axes.throw_set.hash(&mut hasher);
    hasher.finish()
}

/// All composite signatures a function carries (primary + every normalization
/// alternate). Lets a function match across builds that a normalization pass
/// canonicalizes differently.
fn function_composites(fingerprint: &FunctionFingerprint) -> BTreeSet<u64> {
    let mut sigs = BTreeSet::new();
    sigs.insert(composite_signature(&fingerprint.primary));
    for alternate in &fingerprint.alternates {
        sigs.insert(composite_signature(&alternate.axes));
    }
    sigs
}

/// Proposal score for pairing subject function `subject` with reference
/// function `reference`. Returns `None` when they share no AST hash — pure
/// param/statement similarity is too weak to propose a name from.
fn binding_proposal_score(
    subject: &FunctionFingerprint,
    reference: &FunctionFingerprint,
) -> Option<f64> {
    let overlap = function_ast_hashes(subject)
        .intersection(&function_ast_hashes(reference))
        .count();
    if overlap == 0 {
        return None;
    }
    let mut score = 100.0 * overlap as f64;
    if subject.param_count == reference.param_count {
        score += 5.0;
    }
    let stmt_delta = f64::from(subject.statement_count.abs_diff(reference.statement_count));
    score += (10.0 - stmt_delta).max(0.0) * 0.5;
    Some(score)
}

/// One reference function with a recoverable, specific name.
struct ReferenceFunction {
    file: String,
    name: String,
    fingerprint: FunctionFingerprint,
}

/// One subject (emitted) function with a recoverable name.
struct SubjectFunction {
    module_id: u32,
    subject_path: String,
    name: String,
    fingerprint: FunctionFingerprint,
}

/// Every named, specifically-named function across the whole reference tree.
fn collect_reference_functions(index: &ReferenceSourceIndex) -> Vec<ReferenceFunction> {
    let mut out = Vec::new();
    for module in &index.modules {
        let names: BTreeMap<reverts_ir::ByteRange, String> = function_names(module.source.as_str())
            .into_iter()
            .filter(|(_, name)| is_specific_reference_name(name))
            .collect();
        for fingerprint in FunctionExtractor::fingerprint(ModuleId(0), module.source.as_str()) {
            if let Some(name) = names.get(&fingerprint.id.span) {
                out.push(ReferenceFunction {
                    file: module.file_path.clone(),
                    name: name.clone(),
                    fingerprint,
                });
            }
        }
    }
    out
}

/// Every named function across all subject (emitted) modules.
fn collect_subject_functions(subjects: &[SubjectModule]) -> Vec<SubjectFunction> {
    let mut out = Vec::new();
    for subject in subjects {
        let names = function_names(subject.source.as_str());
        for fingerprint in
            FunctionExtractor::fingerprint(ModuleId(subject.module_id), subject.source.as_str())
        {
            if let Some(name) = names.get(&fingerprint.id.span) {
                out.push(SubjectFunction {
                    module_id: subject.module_id,
                    subject_path: subject.file_path.clone(),
                    name: name.clone(),
                    fingerprint,
                });
            }
        }
    }
    out
}

/// Match subject functions to reference functions across the whole corpus.
///
/// Two independent signals must agree to **auto-accept** a rename: (1) the
/// subject's module matched reference file `F` by content (`module_matched_file`),
/// AND (2) the function's body AST hash maps 1:1 within that pair (unique among
/// `F`'s functions and the module's functions), with equal param/stmt counts and
/// a specific reference name. Global hash uniqueness alone is *not* enough — the
/// α-rename-invariant hash makes structurally-identical-but-different functions
/// collide, so a globally-unique body can match the wrong function in an
/// unrelated file. Everything with shared AST-hash signal that isn't accepted
/// becomes a ranked global **proposal** (top-K), maximizing coverage for review.
fn match_function_lists(
    subject_fns: &[SubjectFunction],
    reference_fns: &[ReferenceFunction],
    module_matched_file: &BTreeMap<u32, String>,
) -> Vec<BindingNameRow> {
    let mut ref_by_file: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    let mut ref_by_any: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (index, r) in reference_fns.iter().enumerate() {
        ref_by_file.entry(r.file.as_str()).or_default().push(index);
        for hash in function_ast_hashes(&r.fingerprint) {
            ref_by_any.entry(hash).or_default().push(index);
        }
    }
    let mut subject_by_module: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for (index, s) in subject_fns.iter().enumerate() {
        subject_by_module
            .entry(s.module_id)
            .or_default()
            .push(index);
    }

    // Composite-signature indices for the corroboration-free accept path.
    let mut ref_by_composite: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (index, r) in reference_fns.iter().enumerate() {
        for sig in function_composites(&r.fingerprint) {
            ref_by_composite.entry(sig).or_default().push(index);
        }
    }
    let mut subject_composite_freq: BTreeMap<u64, usize> = BTreeMap::new();
    for s in subject_fns {
        for sig in function_composites(&s.fingerprint) {
            *subject_composite_freq.entry(sig).or_default() += 1;
        }
    }

    let mut rows = Vec::new();
    // (module_id, original_name) accepted so later passes skip them.
    let mut accepted: BTreeSet<(u32, String)> = BTreeSet::new();

    // ACCEPT pass 0: globally-unique COMPOSITE signature — identical across every
    // structural axis and one-of-a-kind in both corpora. Strong enough to accept
    // with no module match, so it names functions in unmatched modules too.
    for (subject_index, subject) in subject_fns.iter().enumerate() {
        let _ = subject_index;
        let Some(sig) = function_composites(&subject.fingerprint)
            .into_iter()
            .find(|sig| {
                subject_composite_freq.get(sig).copied().unwrap_or(0) == 1
                    && ref_by_composite.get(sig).is_some_and(|v| v.len() == 1)
            })
        else {
            continue;
        };
        let reference = &reference_fns[ref_by_composite[&sig][0]];
        if subject.fingerprint.param_count != reference.fingerprint.param_count
            || subject.fingerprint.statement_count != reference.fingerprint.statement_count
        {
            continue;
        }
        accepted.insert((subject.module_id, subject.name.clone()));
        if subject.name == reference.name {
            continue;
        }
        rows.push(BindingNameRow {
            module_id: subject.module_id,
            subject_path: subject.subject_path.clone(),
            reference_file: reference.file.clone(),
            original_name: subject.name.clone(),
            semantic_name: reference.name.clone(),
            accepted: true,
            ast_hash: subject.fingerprint.primary.ast,
            param_count: subject.fingerprint.param_count,
            statement_count: subject.fingerprint.statement_count,
            score: 2.0,
        });
    }

    // ACCEPT pass 1: module-corroborated, unique within the matched file pair.
    for (module_id, subject_indices) in &subject_by_module {
        let Some(file) = module_matched_file.get(module_id) else {
            continue; // module didn't match a reference file - no corroboration
        };
        let Some(reference_indices) = ref_by_file.get(file.as_str()) else {
            continue;
        };
        let mut subject_by_hash: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
        for &si in subject_indices {
            subject_by_hash
                .entry(subject_fns[si].fingerprint.primary.ast)
                .or_default()
                .push(si);
        }
        let mut reference_by_hash: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
        for &ri in reference_indices {
            reference_by_hash
                .entry(reference_fns[ri].fingerprint.primary.ast)
                .or_default()
                .push(ri);
        }
        for (hash, sis) in &subject_by_hash {
            if sis.len() != 1 {
                continue; // ambiguous within the module
            }
            let Some(ris) = reference_by_hash.get(hash) else {
                continue;
            };
            if ris.len() != 1 {
                continue; // ambiguous within the reference file
            }
            let subject = &subject_fns[sis[0]];
            let reference = &reference_fns[ris[0]];
            if accepted.contains(&(subject.module_id, subject.name.clone())) {
                continue; // already accepted by the composite pass
            }
            if subject.fingerprint.param_count != reference.fingerprint.param_count
                || subject.fingerprint.statement_count != reference.fingerprint.statement_count
            {
                continue;
            }
            accepted.insert((subject.module_id, subject.name.clone()));
            if subject.name == reference.name {
                continue;
            }
            rows.push(BindingNameRow {
                module_id: subject.module_id,
                subject_path: subject.subject_path.clone(),
                reference_file: reference.file.clone(),
                original_name: subject.name.clone(),
                semantic_name: reference.name.clone(),
                accepted: true,
                ast_hash: *hash,
                param_count: subject.fingerprint.param_count,
                statement_count: subject.fingerprint.statement_count,
                score: 1.0,
            });
        }
    }

    // PROPOSAL pass: global, for every subject function not auto-accepted.
    for subject in subject_fns {
        if accepted.contains(&(subject.module_id, subject.name.clone())) {
            continue;
        }
        let mut candidates = BTreeSet::<usize>::new();
        for h in function_ast_hashes(&subject.fingerprint) {
            if let Some(indices) = ref_by_any.get(&h) {
                candidates.extend(indices.iter().copied());
            }
        }
        let mut scored: Vec<(f64, usize)> = candidates
            .iter()
            .filter_map(|&ri| {
                binding_proposal_score(&subject.fingerprint, &reference_fns[ri].fingerprint)
                    .map(|score| (score, ri))
            })
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        for (score, ri) in scored.into_iter().take(BINDING_PROPOSAL_TOP_K) {
            let reference = &reference_fns[ri];
            if subject.name == reference.name {
                continue;
            }
            rows.push(BindingNameRow {
                module_id: subject.module_id,
                subject_path: subject.subject_path.clone(),
                reference_file: reference.file.clone(),
                original_name: subject.name.clone(),
                semantic_name: reference.name.clone(),
                accepted: false,
                ast_hash: subject.fingerprint.primary.ast,
                param_count: subject.fingerprint.param_count,
                statement_count: subject.fingerprint.statement_count,
                score,
            });
        }
    }
    rows
}

/// Global function matching over the whole corpus (see [`match_function_lists`]).
/// `module_matched_file` maps subject module id -> the reference file it matched,
/// used to corroborate auto-accepts.
fn match_functions_globally(
    subjects: &[SubjectModule],
    index: &ReferenceSourceIndex,
    module_matched_file: &BTreeMap<u32, String>,
) -> Vec<BindingNameRow> {
    let reference_fns = collect_reference_functions(index);
    let subject_fns = collect_subject_functions(subjects);
    report_normalize_effect(&subject_fns, &reference_fns);
    match_function_lists(&subject_fns, &reference_fns, module_matched_file)
}

/// Measure whether the `reverts_js::normalize` passes (which produce the
/// per-function `alternates`) actually buy any matches: how many subject
/// functions gain extra hashes from normalization, and how many would match a
/// reference function ONLY via an alternate hash (primary differs). Printed to
/// stderr so it doesn't pollute the TSV.
fn report_normalize_effect(subject_fns: &[SubjectFunction], reference_fns: &[ReferenceFunction]) {
    let mut ref_primary: BTreeSet<u64> = BTreeSet::new();
    let mut ref_any: BTreeSet<u64> = BTreeSet::new();
    for r in reference_fns {
        ref_primary.insert(r.fingerprint.primary.ast);
        ref_any.extend(function_ast_hashes(&r.fingerprint));
    }
    let with_alternates = subject_fns
        .iter()
        .filter(|s| !s.fingerprint.alternates.is_empty())
        .count();
    let mut matchable_primary = 0usize; // primary.ast hits some reference primary
    let mut matchable_only_via_alternate = 0usize; // no primary hit, but an alternate hits
    for s in subject_fns {
        let primary_hit = ref_any.contains(&s.fingerprint.primary.ast);
        let any_hit = function_ast_hashes(&s.fingerprint)
            .iter()
            .any(|h| ref_any.contains(h));
        if primary_hit {
            matchable_primary += 1;
        } else if any_hit {
            matchable_only_via_alternate += 1;
        }
    }
    let _ = ref_primary;
    eprintln!(
        "[normalize] subject fns={} with_alternates={} matchable_via_primary={} matchable_ONLY_via_alternate={}",
        subject_fns.len(),
        with_alternates,
        matchable_primary,
        matchable_only_via_alternate
    );
}

/// Write binding-level naming rows into `semantic_binding_names`
/// (`accepted=1` provable renames, `=0` proposals). Each row carries its own
/// subject `file_path` and reference-file provenance (function matching is
/// global, so a row's reference file is unrelated to its module's match).
/// Returns `(accepted_written, proposals_written)`.
fn write_binding_names(
    connection: &Connection,
    project_id: u32,
    rows: &[BindingNameRow],
    origin_prefix: &str,
    reference_version: &str,
) -> Result<(usize, usize), CliRunError> {
    crate::commands::binding_names::ensure_binding_names_table_if_writable(connection, true)?;
    let (mut accepted, mut proposed) = (0usize, 0usize);
    for row in rows {
        let binding_key = row.original_name.clone(); // no binding_index -> key on original
        let origin = format!("{origin_prefix}:{reference_version}:{}", row.reference_file);
        let evidence = row.evidence();
        validate_name_acceptance(
            row.original_name.as_str(),
            row.semantic_name.as_str(),
            origin.as_str(),
            Some(evidence.as_str()),
            NamingGateMode::LocalBinding,
        )
        .map_err(|error| CliRunError::ReferenceSourceNames(error.message()))?;
        connection
            .execute(
                r"
                INSERT INTO semantic_binding_names (
                    project_id, file_path, original_name, binding_index, binding_key, semantic_name,
                    origin, evidence, accepted, created_at, updated_at, gate_status, gate_reason
                ) VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7, ?8, datetime('now'), datetime('now'), 'passed', 'deterministic-gates-passed')
                ON CONFLICT(project_id, file_path, original_name, binding_key) DO UPDATE SET
                    semantic_name = excluded.semantic_name, origin = excluded.origin,
                    evidence = excluded.evidence, accepted = excluded.accepted,
                    gate_status = excluded.gate_status, gate_reason = excluded.gate_reason,
                    updated_at = datetime('now')
                ",
                params![
                    i64::from(project_id),
                    row.subject_path,
                    row.original_name,
                    binding_key,
                    row.semantic_name,
                    origin,
                    evidence,
                    i64::from(row.accepted),
                ],
            )
            .map_err(|e| CliRunError::ReferenceSourceNames(e.to_string()))?;
        if row.accepted {
            accepted += 1;
        } else {
            proposed += 1;
        }
    }
    Ok((accepted, proposed))
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
        // writers in later tasks record it - modules has no origin column.
        let _origin = format!(
            "{origin_prefix}:{reference_version}:{}",
            plan.matched.file_path
        );
        validate_module_path_acceptance(plan.module_semantic_name.as_str(), _origin.as_str())
            .map_err(|error| CliRunError::ReferenceSourceNames(error.message()))?;
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
    fn bare_node_extension_is_not_a_native_asset() {
        // A file-type list containing the bare `'.node'` extension must NOT be
        // classified as a native-asset literal (it would collide across unrelated
        // modules and forge a High-tier match).
        let fingerprint = fingerprint_source(
            "constants/files.ts",
            "export const sourceExtensions = ['.ts', '.node', '.wasm'];",
        )
        .expect("fingerprint");
        let (_exports, assets) = classify_anchors(&fingerprint);
        assert!(
            !assets.contains(".node"),
            "bare .node extension must not be an asset: {assets:?}"
        );
        assert!(
            assets.is_empty(),
            "no native-asset literals expected: {assets:?}"
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

    #[test]
    fn source_path_resolution_handles_relative_and_src_alias_imports() {
        let known = BTreeSet::from([
            "components/HelpV2/HelpV2.tsx",
            "services/analytics/index.ts",
        ]);
        assert_eq!(
            resolve_relative_source_path(
                "commands/help.tsx",
                "../components/HelpV2/HelpV2.js",
                &known,
            ),
            Some("components/HelpV2/HelpV2.tsx".to_string())
        );
        assert_eq!(
            resolve_relative_source_path(
                "entrypoints/app.ts",
                "src/services/analytics/index.js",
                &known,
            ),
            Some("services/analytics/index.ts".to_string())
        );
        assert_eq!(
            resolve_relative_source_path("entrypoints/app.ts", "react", &known),
            None
        );
    }

    fn fp(anchors: &[&str]) -> SourceFingerprint {
        SourceFingerprint {
            normalized_source_hash: String::new(),
            normalized_source_hashes: BTreeSet::new(),
            function_signature_hashes: BTreeSet::new(),
            string_anchors: anchors.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn refmod(path: &str, anchors: &[&str]) -> ReferenceSourceModule {
        ReferenceSourceModule {
            file_path: path.to_string(),
            source: String::new(),
            fingerprint: fp(anchors),
            export_names: BTreeSet::new(),
            asset_literals: BTreeSet::new(),
        }
    }

    #[test]
    fn compute_anchor_idf_downweights_common_anchors() {
        let modules = vec![
            refmod("a.ts", &["common", "u1x"]),
            refmod("b.ts", &["common", "u2x"]),
            refmod("c.ts", &["common", "u3x"]),
            refmod("d.ts", &["common", "u4x"]),
        ];
        let idf = compute_anchor_idf(&modules);
        // "common" is in 4/4 modules -> ln(4/4) = 0; a unique anchor -> ln(4) > 1.
        assert!(idf["common"] < 1.0e-9, "common idf: {}", idf["common"]);
        assert!(idf["u1x"] > 1.0, "unique idf: {}", idf["u1x"]);
    }

    #[test]
    fn hub_overlap_stays_low_distinctive_overlap_is_medium() {
        // 6 "hub" modules all share `hubtoken`; one `distinctive.ts` has 20 rare
        // anchors (df = 1 each). N = 7 -> rare idf = ln(7), about 1.95.
        let rare: Vec<String> = (0..20).map(|i| format!("rare-anchor-{i}")).collect();
        let rare_refs: Vec<&str> = rare.iter().map(String::as_str).collect();
        let mut modules: Vec<ReferenceSourceModule> = (0..6)
            .map(|i| {
                let p = format!("hub{i}.ts");
                ReferenceSourceModule {
                    file_path: p,
                    source: String::new(),
                    fingerprint: fp(&["hubtoken", "filler-token"]),
                    export_names: BTreeSet::new(),
                    asset_literals: BTreeSet::new(),
                }
            })
            .collect();
        modules.push(refmod("distinctive.ts", &rare_refs));
        let anchor_idf = compute_anchor_idf(&modules);
        let index = ReferenceSourceIndex {
            version: "t".to_string(),
            modules,
            anchor_idf,
        };

        // Sharing only the hub token = near-zero weighted overlap -> rejected/Low,
        // never promoted to Medium.
        let subject_hub = fp(&["hubtoken"]);
        let hub_match = best_module_match(&subject_hub, &index);
        assert!(
            hub_match.map_or(true, |m| m.tier == MatchTier::Low),
            "hub-only overlap must not be promoted above Low"
        );

        // Sharing the 20 rare anchors = high weighted overlap -> Medium.
        let subject_dist = fp(&rare_refs);
        let dist_match = best_module_match(&subject_dist, &index).expect("distinctive match");
        assert_eq!(dist_match.file_path, "distinctive.ts");
        assert_eq!(
            dist_match.tier,
            MatchTier::Medium,
            "wanchor={}",
            dist_match.weighted_anchor
        );
    }

    #[test]
    fn normalized_anchor_penalizes_large_reference_hubs() {
        let shared: Vec<String> = (0..20).map(|i| format!("shared-{i}")).collect();
        let mut large_ref = shared.clone();
        large_ref.extend((0..980).map(|i| format!("large-only-{i}")));
        let large_refs: Vec<&str> = large_ref.iter().map(String::as_str).collect();

        let mut modules = vec![refmod("large-hub.ts", &large_refs)];
        for i in 0..99 {
            let path = format!("filler-{i}.ts");
            let anchor = format!("filler-anchor-{i}");
            modules.push(ReferenceSourceModule {
                file_path: path,
                source: String::new(),
                fingerprint: fp(&[anchor.as_str()]),
                export_names: BTreeSet::new(),
                asset_literals: BTreeSet::new(),
            });
        }
        let index = ReferenceSourceIndex {
            version: "t".to_string(),
            anchor_idf: compute_anchor_idf(&modules),
            modules,
        };
        let subject_refs: Vec<&str> = shared.iter().map(String::as_str).collect();
        let matched = best_module_match(&fp(&subject_refs), &index).expect("match");

        assert_eq!(matched.file_path, "large-hub.ts");
        assert!(
            matched.weighted_anchor >= MEDIUM_WEIGHTED_ANCHOR,
            "raw weighted overlap should be high enough to need normalization: {}",
            matched.weighted_anchor
        );
        assert!(
            matched.normalized_anchor < MEDIUM_NORMALIZED_ANCHOR,
            "large reference denominator should expose hub-like coverage: {}",
            matched.normalized_anchor
        );
        assert_eq!(matched.tier, MatchTier::Low);
    }

    #[test]
    fn fn_export_overlap_needs_anchor_corroboration_for_medium() {
        let base = MatchEvidence {
            hash_match: false,
            asset_overlap: 0,
            export_overlap: 0,
            function_overlap: 2,
            structural_score: 0.0,
            graph: GraphEvidence::default(),
            weighted_anchor: 5.0,
            normalized_anchor: 0.0,
        };
        // function_overlap >= 2 but no anchor corroboration -> Low (these were the
        // ~30 fn/export coincidence false positives, e.g. unrelated modules all
        // "matching" cli/print.ts at nanchor=0).
        assert_eq!(raw_match_tier(base), MatchTier::Low);
        // The same overlap WITH normalized-anchor corroboration -> Medium.
        let corroborated = MatchEvidence {
            normalized_anchor: 0.1,
            ..base
        };
        assert_eq!(raw_match_tier(corroborated), MatchTier::Medium);
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
                structural_score: 0.0,
                graph_support: 0,
                graph_known_edges: 0,
                anchor_overlap: 0,
                weighted_anchor: 0.0,
                normalized_anchor: 0.0,
                margin: 1.0,
                reciprocal_best: true,
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
    fn global_reference_uniqueness_keeps_one_medium_per_reference() {
        // Two modules claim `components/hub`; the reciprocal-best one wins.
        let mut reciprocal = make_plan(1, "components/hub", MatchTier::Medium);
        reciprocal.matched.reciprocal_best = true;
        let mut hub_duplicate = make_plan(2, "components/hub", MatchTier::Medium);
        hub_duplicate.matched.reciprocal_best = false;

        // Two modules claim `components/graph`; both graph-supported, so the
        // higher-content (normalized_anchor) one wins. The old behavior kept
        // BOTH — the bug that let one reference file anchor many modules (e.g.
        // four modules all "matching" ElicitationDialog.tsx after propagation).
        let mut graph_strong = make_plan(3, "components/graph", MatchTier::Medium);
        graph_strong.matched.graph_support = 1;
        graph_strong.matched.normalized_anchor = 0.30;
        let mut graph_weak = make_plan(4, "components/graph", MatchTier::Medium);
        graph_weak.matched.graph_support = 1;
        graph_weak.matched.normalized_anchor = 0.10;

        let unique = make_plan(5, "components/unique", MatchTier::Medium);

        let mut plans = vec![reciprocal, hub_duplicate, graph_strong, graph_weak, unique];
        // Empty shared-anchors -> no esbuild-split exception -> classic injective.
        let anchors = vec![BTreeSet::new(); plans.len()];
        calibrate_global_reference_uniqueness(&mut plans, &anchors);

        assert_eq!(plans[0].matched.tier, MatchTier::Medium, "reciprocal kept");
        assert_eq!(
            plans[1].matched.tier,
            MatchTier::Low,
            "hub duplicate demoted"
        );
        assert_eq!(
            plans[2].matched.tier,
            MatchTier::Medium,
            "stronger graph kept"
        );
        assert_eq!(
            plans[3].matched.tier,
            MatchTier::Low,
            "weaker graph duplicate demoted (injective 1:1)"
        );
        assert_eq!(plans[4].matched.tier, MatchTier::Medium, "unique untouched");
    }

    #[test]
    fn many_to_one_keeps_disjoint_strong_split_modules() {
        // Two modules both match split.ts; their shared-anchor sets are DISJOINT
        // (esbuild split it into two parts) and both are independently strong ->
        // both kept. A third module shares the SAME anchors as the first and is
        // weak -> demoted.
        let mut a = make_plan(1, "split", MatchTier::Medium);
        a.matched.reciprocal_best = true;
        a.matched.normalized_anchor = 0.4;
        let mut b = make_plan(2, "split", MatchTier::Medium);
        b.matched.reciprocal_best = false;
        b.matched.normalized_anchor = 0.3; // independently strong (>=0.18)
        let mut c = make_plan(3, "split", MatchTier::Medium);
        c.matched.normalized_anchor = 0.05; // weak, overlapping anchors -> demote
        let mut plans = vec![a, b, c];
        let anchors = vec![
            BTreeSet::from(["alpha".to_string(), "beta".to_string()]),
            BTreeSet::from(["gamma".to_string(), "delta".to_string()]), // disjoint from a
            BTreeSet::from(["alpha".to_string()]),                      // overlaps a
        ];
        calibrate_global_reference_uniqueness(&mut plans, &anchors);
        // a is strongest (reciprocal) -> kept; b disjoint+strong -> kept; c overlaps -> demoted.
        assert_eq!(plans[0].matched.tier, MatchTier::Medium, "strongest kept");
        assert_eq!(
            plans[1].matched.tier,
            MatchTier::Medium,
            "disjoint split kept"
        );
        assert_eq!(
            plans[2].matched.tier,
            MatchTier::Low,
            "overlapping weak demoted"
        );
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
            BindingNameRow {
                module_id: 5,
                subject_path: "modules/m.ts".into(),
                reference_file: "f.ts".into(),
                original_name: "x".into(),
                semantic_name: "decodeFrame".into(),
                accepted: true,
                ast_hash: 0xabcd,
                param_count: 1,
                statement_count: 2,
                score: 1.0,
            },
            BindingNameRow {
                module_id: 5,
                subject_path: "modules/m.ts".into(),
                reference_file: "f.ts".into(),
                original_name: "y".into(),
                semantic_name: "guessName".into(),
                accepted: false,
                ast_hash: 0x1234,
                param_count: 0,
                statement_count: 1,
                score: 105.0,
            },
        ];
        let (accepted_n, proposed_n) =
            write_binding_names(&connection, 1, &rows, "source", "2.1.76").expect("write");
        assert_eq!((accepted_n, proposed_n), (1, 1));
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

    // Test 1: e2e module match for both wrappers (hermetic)

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

    // Test 2: e2e module-name WRITE (in-memory DB)

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

    // Test 3: export mapping e2e (in-memory DB)

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

    // Test 4: precision gate - no false High-tier accept

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
        // exports/functions) must yield NO match at all - not merely a non-High one.
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

    fn subject_fn(module_id: u32, path: &str, source: &str) -> Vec<SubjectFunction> {
        let names = function_names(source);
        FunctionExtractor::fingerprint(ModuleId(module_id), source)
            .into_iter()
            .filter_map(|f| {
                names.get(&f.id.span).map(|name| SubjectFunction {
                    module_id,
                    subject_path: path.to_string(),
                    name: name.clone(),
                    fingerprint: f,
                })
            })
            .collect()
    }

    fn reference_fn(file: &str, source: &str) -> Vec<ReferenceFunction> {
        let names: BTreeMap<reverts_ir::ByteRange, String> = function_names(source)
            .into_iter()
            .filter(|(_, name)| is_specific_reference_name(name))
            .collect();
        FunctionExtractor::fingerprint(ModuleId(0), source)
            .into_iter()
            .filter_map(|f| {
                names.get(&f.id.span).map(|name| ReferenceFunction {
                    file: file.to_string(),
                    name: name.clone(),
                    fingerprint: f,
                })
            })
            .collect()
    }

    fn corroborate(module_id: u32, file: &str) -> BTreeMap<u32, String> {
        BTreeMap::from([(module_id, file.to_string())])
    }

    #[test]
    fn function_match_accepts_when_module_corroborates() {
        // Module 1 matched util/inc.ts (signal 1) AND the body hash maps 1:1
        // within that file (signal 2) -> provable accept.
        let subjects = subject_fn(1, "modules/m.ts", "function aB(x) { return x + 1; }");
        let references = reference_fn("util/inc.ts", "function increment(x) { return x + 1; }");
        let rows = match_function_lists(&subjects, &references, &corroborate(1, "util/inc.ts"));
        assert_eq!(rows.iter().filter(|r| r.accepted).count(), 1, "{rows:?}");
        let accept = rows.iter().find(|r| r.accepted).unwrap();
        assert_eq!(accept.original_name, "aB");
        assert_eq!(accept.semantic_name, "increment");
        assert_eq!(accept.reference_file, "util/inc.ts");
    }

    #[test]
    fn function_match_accepts_unique_composite_without_corroboration() {
        // Identical across every structural axis AND one-of-a-kind in both
        // corpora -> the multi-axis composite pass accepts even with NO module
        // match, naming functions in otherwise-unmatched modules.
        let subjects = subject_fn(
            99,
            "modules/unmatched.ts",
            "function aB(x) { return x + 1; }",
        );
        let references = reference_fn("util/inc.ts", "function increment(x) { return x + 1; }");
        let rows = match_function_lists(&subjects, &references, &BTreeMap::new());
        assert!(
            rows.iter()
                .any(|r| r.accepted && r.semantic_name == "increment"),
            "unique composite should accept without corroboration: {rows:?}"
        );
    }

    #[test]
    fn function_match_composite_collision_needs_corroboration() {
        // Two structurally-identical reference functions in DIFFERENT files share
        // a composite -> not globally unique -> the composite pass cannot accept;
        // the module match then disambiguates which file is right.
        let subjects = subject_fn(1, "modules/m.ts", "function aB(x) { return x + 1; }");
        let mut references = reference_fn("util/a.ts", "function alpha(x) { return x + 1; }");
        references.extend(reference_fn(
            "util/b.ts",
            "function beta(x) { return x + 1; }",
        ));
        let ambiguous = match_function_lists(&subjects, &references, &BTreeMap::new());
        assert!(
            !ambiguous.iter().any(|r| r.accepted),
            "ambiguous composite must not auto-accept: {ambiguous:?}"
        );
        let rows = match_function_lists(&subjects, &references, &corroborate(1, "util/a.ts"));
        let accept = rows.iter().find(|r| r.accepted);
        assert!(accept.is_some(), "corroboration should accept: {rows:?}");
        assert_eq!(accept.unwrap().semantic_name, "alpha");
    }

    #[test]
    fn function_match_demotes_when_reference_hash_not_unique_in_file() {
        let subjects = subject_fn(1, "modules/m.ts", "function aB(x) { return x + 1; }");
        let references = reference_fn(
            "util/inc.ts",
            "function increment(x) { return x + 1; } function bump(y) { return y + 1; }",
        );
        let rows = match_function_lists(&subjects, &references, &corroborate(1, "util/inc.ts"));
        assert!(!rows.iter().any(|r| r.accepted), "no accepts: {rows:?}");
        assert!(
            rows.iter().filter(|r| !r.accepted).count() >= 1,
            "expected proposals: {rows:?}"
        );
    }

    #[test]
    fn function_match_demotes_when_subject_hash_not_unique_in_module() {
        // Two functions in the SAME subject module share the hash -> ambiguous
        // within the module -> no accept.
        let subjects = subject_fn(
            1,
            "modules/m.ts",
            "function aB(x) { return x + 1; } function cD(y) { return y + 1; }",
        );
        let references = reference_fn("util/inc.ts", "function increment(z) { return z + 1; }");
        let rows = match_function_lists(&subjects, &references, &corroborate(1, "util/inc.ts"));
        assert!(!rows.iter().any(|r| r.accepted), "{rows:?}");
    }

    #[test]
    fn function_match_blocks_accept_on_param_mismatch() {
        let subjects = subject_fn(1, "modules/m.ts", "function aB(x) { return x + 1; }");
        let references = reference_fn("util/inc.ts", "function increment(x, y) { return x + 1; }");
        let rows = match_function_lists(&subjects, &references, &corroborate(1, "util/inc.ts"));
        assert!(!rows.iter().any(|r| r.accepted), "{rows:?}");
        assert!(
            rows.iter()
                .any(|r| !r.accepted && r.semantic_name == "increment")
        );
    }

    #[test]
    fn function_match_drops_placeholder_reference_names() {
        let subjects = subject_fn(1, "modules/m.ts", "function aB(x) { return x + 1; }");
        let references = reference_fn("util/inc.ts", "function _temp1(x) { return x + 1; }");
        let rows = match_function_lists(&subjects, &references, &corroborate(1, "util/inc.ts"));
        assert!(
            rows.is_empty(),
            "placeholder ref name must be filtered: {rows:?}"
        );
    }

    #[test]
    fn function_match_skips_when_name_already_matches() {
        let subjects = subject_fn(1, "modules/m.ts", "function increment(x) { return x + 1; }");
        let references = reference_fn("util/inc.ts", "function increment(x) { return x + 1; }");
        let rows = match_function_lists(&subjects, &references, &corroborate(1, "util/inc.ts"));
        assert!(
            rows.is_empty(),
            "already-correct name needs no row: {rows:?}"
        );
    }

    #[test]
    fn specific_reference_name_filters_placeholders_and_generics() {
        for junk in [
            "_temp", "_temp1", "__temp2", "t8", "e10", "x5", "_0", "__12", "get", "init", "fn",
            "id",
        ] {
            assert!(!is_specific_reference_name(junk), "should reject {junk}");
        }
        for good in [
            "escapeJsLineTerminators",
            "charWidth",
            "toAgentId",
            "normalize",
            "isMCPToolResult",
        ] {
            assert!(is_specific_reference_name(good), "should keep {good}");
        }
    }
}
