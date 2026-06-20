//! `reference-source-names` command: name a decompiled project's modules,
//! exports, and local bindings by matching its emitted TypeScript against a
//! historical first-party source tree. Tier-gated: only provable matches are
//! auto-accepted; everything else is left for an agent.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Instant;

use clap::{Args, ValueEnum};
use reverts_graph::{
    FunctionExtractor, IdentifierStreams, extract_import_specifiers, function_anchor_tokens,
    function_callee_names, function_names, function_referenced_names, identifier_streams,
};
use reverts_input::sqlite::{load_project_rows_from_connection, load_project_rows_from_sqlite};
use reverts_input::{InputBundle, InputRows, ModuleDependencyTarget, PackageAttributionStatus};
use reverts_ir::{AxisHashes, FunctionFingerprint, ModuleId};
use reverts_js::sanitize_identifier;
use reverts_package_matcher::{
    GraphNeighborhoodEvidence, SourceEvidenceIdf, SourceEvidenceProfile, SourceEvidenceScore,
    SourceFingerprint, StructuralBag, build_source_evidence_profile_with_fingerprint,
    build_structural_bag, fingerprint_source, graph_neighborhood_support, score_source_evidence,
    score_structural_bags, source_evidence_idf,
};
use reverts_pipeline::{
    generate_project_from_prepared, prepare_and_enrich, prepare_input_rows_for_pipeline,
};
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
use crate::persistence::repository::persist_module_dependencies;
use crate::persistence::synthetic_modules::persist_prepared_synthetic_inputs;
use crate::pkg_sources::version_resolution::best_matching_package_version_by_binary_search;

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
    /// Match only module paths/names from bundle-extracted input slices.
    ///
    /// This avoids generating the full TypeScript project before source-tree
    /// matching, which is much faster for large single-file bundles. It reuses
    /// the normal `reverts-bundle` extraction path and intentionally skips
    /// path overrides and export/local-binding name propagation because those
    /// rows must target the final generated output.
    #[arg(long, default_value_t = false)]
    pub module_only: bool,
    /// Write a machine-readable module match summary.
    #[arg(long)]
    pub summary_json: Option<PathBuf>,
    /// Write Low/Medium boundary diagnostics for tuning source matching.
    #[arg(long)]
    pub diagnostics_json: Option<PathBuf>,
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
    top_candidate: RankedModuleMatch,
    runner_up: Option<RankedModuleMatch>,
    shared_string_anchors: BTreeSet<String>,
    module_semantic_name: String,
    subject_bindings: Vec<(String, String)>,
    reference_exports: std::collections::BTreeSet<String>,
}

#[derive(Debug, Clone, Copy)]
struct PlanSupportOptions {
    structural_bag: bool,
    graph_support: bool,
    graph_structure: bool,
}

fn plan_modules(
    subjects: &[SubjectModule],
    index: &ReferenceSourceIndex,
    support_options: PlanSupportOptions,
) -> Result<Vec<ModulePlan>, CliRunError> {
    let trace_start = Instant::now();
    let structural_support = if support_options.structural_bag {
        source_structural_support(subjects, index)
    } else {
        BTreeMap::new()
    };
    trace_reference_source_names(trace_start, "plan.source_structural_support");
    let graph_support = if support_options.graph_support {
        source_graph_support(subjects, index, &structural_support)
    } else {
        BTreeMap::new()
    };
    let graph_structure = if support_options.graph_structure {
        source_graph_structure(subjects, index)
    } else {
        GraphStructureContext::default()
    };
    trace_reference_source_names(trace_start, "plan.source_graph_support");
    let subject_best_matches =
        best_ranked_by_subject(subjects, index, &structural_support, &graph_support);
    let post_match_graph_support =
        if support_options.graph_structure && !support_options.graph_support {
            let assignment = subject_best_matches
                .iter()
                .map(|(module_id, ranked)| (*module_id, ranked.best.matched.file_path.clone()))
                .collect::<BTreeMap<_, _>>();
            source_graph_support_for_assignment(subjects, index, &assignment)
        } else {
            BTreeMap::new()
        };
    trace_reference_source_names(trace_start, "plan.best_ranked_by_subject");
    let reference_best_subjects = best_subject_by_reference_matches(&subject_best_matches);
    let mut plans = Vec::new();
    // Parallel to `plans`: the string anchors each subject shares with its
    // matched reference file. Used to allow many-to-one assignment when esbuild
    // split one source file into modules covering DISJOINT parts.
    let mut shared_anchors: Vec<BTreeSet<String>> = Vec::new();
    for subject in subjects {
        let Some(ranked) = subject_best_matches.get(&subject.module_id) else {
            continue;
        };
        let mut matched = ranked.best.matched.clone();
        attach_post_match_graph_support(subject.module_id, &mut matched, &post_match_graph_support);
        matched.graph_structure = graph_structure_evidence(
            subject.module_id,
            matched.file_path.as_str(),
            &graph_structure,
        );
        matched.reciprocal_best = reference_best_subjects
            .get(matched.file_path.as_str())
            .is_some_and(|best_subject_id| *best_subject_id == subject.module_id);
        matched.tier = calibrate_tier(
            matched.tier,
            matched.margin,
            matched.reciprocal_best,
            matched.source_score,
            matched.weighted_anchor,
            matched.normalized_anchor,
        );
        let reference_module = index
            .modules
            .iter()
            .find(|m| m.file_path == matched.file_path);
        let reference_exports = reference_module
            .map(|m| m.export_names.clone())
            .unwrap_or_default();
        let anchors: BTreeSet<String> = reference_module
            .map(|m| {
                subject
                    .fingerprint
                    .string_anchors
                    .intersection(&m.fingerprint.string_anchors)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let runner_up = ranked.runner_up.clone().map(|mut runner_up| {
            attach_post_match_graph_support(
                subject.module_id,
                &mut runner_up.matched,
                &post_match_graph_support,
            );
            runner_up.matched.graph_structure = graph_structure_evidence(
                subject.module_id,
                runner_up.matched.file_path.as_str(),
                &graph_structure,
            );
            runner_up
        });
        if matched.tier == MatchTier::Low && guarded_graph_placement_promotion(&matched) {
            matched.tier = MatchTier::Medium;
        }
        if matched.tier == MatchTier::Low
            && runner_up
                .as_ref()
                .is_some_and(|runner_up| guarded_ambiguous_promotion(&matched, &runner_up.matched))
        {
            matched.tier = MatchTier::Medium;
        }
        plans.push(ModulePlan {
            module_id: subject.module_id,
            subject_path: subject.file_path.clone(),
            reference_version: index.version.clone(),
            module_semantic_name: strip_source_extension(&matched.file_path),
            top_candidate: RankedModuleMatch {
                relevance: ranked.best.relevance,
                matched: matched.clone(),
            },
            matched,
            runner_up,
            shared_string_anchors: anchors.clone(),
            subject_bindings: subject.bindings.clone(),
            reference_exports,
        });
        shared_anchors.push(anchors);
    }
    trace_reference_source_names(trace_start, "plan.best_module_loop");
    apply_split_module_cluster_promotions(&mut plans, &shared_anchors);
    calibrate_global_reference_uniqueness(&mut plans, &shared_anchors);
    plans.sort_by(|a, b| a.module_id.cmp(&b.module_id));
    Ok(plans)
}

/// Promote Low candidates that look like additional esbuild slices of a
/// reference file that already has at least one Medium seed. This is deliberately
/// cluster-local: a candidate must point at the same reference as a seed, cover a
/// distinct source region (by anchors or structural region evidence), and carry
/// independent content/source/graph corroboration. It never uses cluster
/// evidence as standalone proof.
fn apply_split_module_cluster_promotions(
    plans: &mut [ModulePlan],
    shared_anchors: &[BTreeSet<String>],
) {
    let mut indices_by_reference = BTreeMap::<String, Vec<usize>>::new();
    for (index, plan) in plans.iter().enumerate() {
        indices_by_reference
            .entry(plan.matched.file_path.clone())
            .or_default()
            .push(index);
    }
    for indices in indices_by_reference.values() {
        let mut kept_anchor_sets = indices
            .iter()
            .filter(|&&index| plans[index].matched.tier == MatchTier::Medium)
            .map(|&index| shared_anchors[index].clone())
            .collect::<Vec<_>>();
        if kept_anchor_sets.is_empty() {
            continue;
        }
        let mut low_indices = indices
            .iter()
            .copied()
            .filter(|&index| plans[index].matched.tier == MatchTier::Low)
            .collect::<Vec<_>>();
        low_indices.sort_by(|&left, &right| {
            let left_match = &plans[left].matched;
            let right_match = &plans[right].matched;
            right_match
                .structural_score
                .total_cmp(&left_match.structural_score)
                .then(
                    right_match
                        .normalized_anchor
                        .total_cmp(&left_match.normalized_anchor),
                )
                .then(
                    right_match
                        .weighted_anchor
                        .total_cmp(&left_match.weighted_anchor),
                )
                .then(right_match.graph_support.cmp(&left_match.graph_support))
                .then(plans[left].module_id.cmp(&plans[right].module_id))
        });
        for index in low_indices {
            let anchors = &shared_anchors[index];
            if split_module_cluster_candidate(&plans[index], anchors, &kept_anchor_sets) {
                plans[index].matched.tier = MatchTier::Medium;
                kept_anchor_sets.push(anchors.clone());
            }
        }
    }
}

fn split_module_cluster_candidate(
    plan: &ModulePlan,
    anchors: &BTreeSet<String>,
    kept_anchor_sets: &[BTreeSet<String>],
) -> bool {
    let matched = &plan.matched;
    if matched.margin < SPLIT_CLUSTER_MIN_MARGIN || !has_ambiguous_promotion_content(matched) {
        return false;
    }
    if !has_split_module_cluster_support(matched, anchors) {
        return false;
    }
    let distinct_anchor_region = !anchors.is_empty()
        && kept_anchor_sets
            .iter()
            .all(|kept| anchor_sets_cover_distinct_parts(anchors, kept));
    let structural_region = has_split_module_structural_region(matched);
    if !(distinct_anchor_region || structural_region) {
        return false;
    }
    plan.runner_up.as_ref().is_none_or(|runner_up| {
        has_clear_anchor_delta(matched, &runner_up.matched)
            || has_clear_source_axis_delta(matched, &runner_up.matched)
            || has_clear_granular_delta(matched, &runner_up.matched)
            || has_clear_structural_delta(matched, &runner_up.matched)
            || has_clear_graph_delta(matched, &runner_up.matched)
            || (distinct_anchor_region && matched.source_score.unique_string_anchor_overlap >= 1)
    })
}

fn has_split_module_cluster_support(matched: &ModuleMatch, anchors: &BTreeSet<String>) -> bool {
    has_high_unique_anchor_mass(matched)
        || has_high_cooccurrence_source_mass(matched)
        || guarded_graph_placement_promotion(matched)
        || (matched.weighted_anchor >= AMBIGUOUS_PROMOTION_MIN_WEIGHTED_ANCHOR
            && matched.normalized_anchor >= AMBIGUOUS_PROMOTION_MIN_NANCHOR)
        || (matched.source_score.unique_string_anchor_overlap >= 1
            && (matched.weighted_anchor >= 4.0 || !anchors.is_empty()))
        || (matched.source_score.anchor_cooccurrence_overlap >= 3
            && matched.source_score.anchor_cooccurrence_jaccard >= 0.15)
        || (matched.source_score.jsx_react_shape_overlap >= 3
            && matched.source_score.jsx_react_shape_jaccard >= 0.15)
        || has_split_module_structural_region(matched)
}

fn has_split_module_structural_region(matched: &ModuleMatch) -> bool {
    matched.structural_score >= SPLIT_CLUSTER_STRUCTURAL_SCORE
        && (matched.statement_window_overlap >= 2
            || matched.block_branch_overlap >= 2
            || matched.statement_window_containment >= 0.50
            || matched.block_branch_containment >= 0.25)
        && (matched.normalized_anchor >= AMBIGUOUS_PROMOTION_MIN_NANCHOR
            || matched.weighted_anchor >= AMBIGUOUS_PROMOTION_MIN_WEIGHTED_ANCHOR
            || matched.source_score.unique_string_anchor_overlap >= 1
            || (matched.source_score.function_axis_overlap >= 4
                && matched.source_score.function_axis_containment >= 0.25)
            || matched.graph_support >= 1)
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
            let independently_strong = matched.reciprocal_best
                || matched.normalized_anchor >= MEDIUM_NORMALIZED_ANCHOR
                || has_high_unique_anchor_mass(matched)
                || has_high_cooccurrence_source_mass(matched)
                || has_split_module_cluster_support(matched, anchors)
                || guarded_graph_placement_promotion(matched);
            let covers_distinct_part = !anchors.is_empty()
                && kept_anchors
                    .iter()
                    .all(|kept| anchor_sets_cover_distinct_parts(anchors, kept));
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

fn anchor_sets_cover_distinct_parts(left: &BTreeSet<String>, right: &BTreeSet<String>) -> bool {
    if left.is_empty() || right.is_empty() {
        return false;
    }
    let intersection = left.intersection(right).count();
    if intersection == 0 {
        return true;
    }
    let smaller = left.len().min(right.len());
    intersection * 4 <= smaller
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

fn tier_rank(tier: MatchTier) -> u8 {
    match tier {
        MatchTier::High => 2,
        MatchTier::Medium => 1,
        MatchTier::Low => 0,
    }
}

/// Propagate module matches along the import graph. Prior knowledge that the two
/// builds are the SAME app makes their module import graphs near-isomorphic, so
/// from the confirmed module matches we align import edges: if confirmed module
/// M↔R, and after removing M's already-matched imports (which must all map into
/// R's imports — a consistency check) exactly one subject import and one
/// reference import remain unmatched, that residual edge identifies the pair.
/// Iterates to a fixpoint. Returns the seed expanded with the new matches.
///
/// Safe by construction: even a wrong propagated module pair cannot produce a
/// false FUNCTION name, because the within-module function passes still require
/// per-pair hard evidence — this only widens the set of module pairs they search.
fn propagate_module_matches_by_graph(
    subjects: &[SubjectModule],
    index: &ReferenceSourceIndex,
    seed: &BTreeMap<u32, String>,
) -> BTreeMap<u32, String> {
    const MAX_ROUNDS: usize = 8;
    let edges = build_source_graph_edges(subjects, index);
    let mut matched = seed.clone();
    let mut used_reference: BTreeSet<String> = seed.values().cloned().collect();
    for _ in 0..MAX_ROUNDS {
        let mut votes: BTreeMap<u32, BTreeMap<String, usize>> = BTreeMap::new();
        for (subject_module, reference_file) in &matched {
            let (Some(subject_deps), Some(reference_deps)) = (
                edges.subject_deps.get(subject_module),
                edges.reference_deps.get(reference_file),
            ) else {
                continue;
            };
            // Consistency: every already-matched import of M must map to an
            // import of R, else this pair's alignment is unreliable.
            let consistent = subject_deps
                .iter()
                .filter_map(|dep| matched.get(dep))
                .all(|mapped| reference_deps.contains(mapped));
            if !consistent {
                continue;
            }
            let unmatched_subject: Vec<u32> = subject_deps
                .iter()
                .copied()
                .filter(|dep| !matched.contains_key(dep))
                .collect();
            let unmatched_reference: Vec<&String> = reference_deps
                .iter()
                .filter(|dep| !used_reference.contains(*dep))
                .collect();
            if unmatched_subject.len() == 1 && unmatched_reference.len() == 1 {
                *votes
                    .entry(unmatched_subject[0])
                    .or_default()
                    .entry(unmatched_reference[0].clone())
                    .or_default() += 1;
            }
        }
        // Accept modules whose residual edge points unanimously at one reference.
        let mut round: Vec<(u32, String)> = votes
            .into_iter()
            .filter(|(module, _)| !matched.contains_key(module))
            .filter_map(|(module, candidates)| {
                (candidates.len() == 1)
                    .then(|| (module, candidates.into_keys().next().unwrap_or_default()))
            })
            .collect();
        round.sort();
        let mut applied = 0usize;
        for (subject_module, reference_file) in round {
            if used_reference.insert(reference_file.clone()) {
                matched.insert(subject_module, reference_file);
                applied += 1;
            }
        }
        if applied == 0 {
            break;
        }
    }
    matched
}

pub(crate) fn run(args: ReferenceSourceNamesArgs) -> Result<(), CliRunError> {
    let trace_start = Instant::now();
    let index = build_reference_source_index(&args.reference_source_root, &args.reference_version)
        .map_err(CliRunError::ReferenceSourceNames)?;
    trace_reference_source_names(trace_start, "build_reference_source_index");
    let (subjects, island_source) = if args.module_only {
        (subject_modules_from_extracted_input(&args)?, None)
    } else {
        subject_modules(&args)?
    };
    trace_reference_source_names(trace_start, "subject_modules");
    let mut plans = plan_modules(
        &subjects,
        &index,
        PlanSupportOptions {
            // Full generated-output mode can afford the costlier structural-bag
            // refinement. Module-only bundle matching is optimized for large
            // single-file targets, so keep structural bags and iterative graph
            // propagation off there but still build graph diagnostics/support
            // after the initial top-candidate assignment.
            structural_bag: !args.module_only,
            graph_support: !args.module_only,
            graph_structure: true,
        },
    )?;
    trace_reference_source_names(trace_start, "plan_modules");

    // Collect every named function across both corpora once. In module-only
    // mode this powers module promotions only; in full mode the same rows later
    // feed binding/export propagation. Keeping one shared pass avoids a second,
    // divergent matching implementation.
    let subject_fns = collect_subject_functions(&subjects);
    trace_reference_source_names(trace_start, "collect_subject_functions");
    let reference_fns = collect_reference_functions(&index);
    trace_reference_source_names(trace_start, "collect_reference_functions");
    if !args.module_only {
        report_normalize_effect(&subject_fns, &reference_fns);
    }

    // Function->module reinforcement: promote unmatched modules when multiple
    // high-precision accepted function matches point at the same reference file.
    // This is safe for module-only because it writes only module semantic names;
    // generated-output binding/path mutations remain disabled in that mode.
    // Feed the function passes EVERY module's best-candidate reference file, not
    // just the tier-gated matches. This is safe: the within-module function passes
    // require per-pair hard evidence, so a weak/wrong module pair yields no false
    // function name — it only gives the precise passes more pairs to search. (The
    // graph-propagation seed below stays tier-gated for its edge-alignment.)
    let module_matched_file: BTreeMap<u32, String> = plans
        .iter()
        .map(|plan| (plan.module_id, plan.matched.file_path.clone()))
        .collect();
    let mut binding_rows = match_function_lists(&subject_fns, &reference_fns, &module_matched_file);
    trace_reference_source_names(trace_start, "match_function_lists");
    let promotions = derive_module_promotions(&binding_rows, &module_matched_file);
    if !promotions.is_empty() {
        apply_module_promotions(&mut plans, &promotions, &subjects, &index);
        plans.sort_by(|a, b| a.module_id.cmp(&b.module_id));
        let module_matched_file: BTreeMap<u32, String> = plans
            .iter()
            .filter(|plan| tier_passes(plan.matched.tier, args.min_tier))
            .map(|plan| (plan.module_id, plan.matched.file_path.clone()))
            .collect();
        binding_rows = match_function_lists(&subject_fns, &reference_fns, &module_matched_file);
        trace_reference_source_names(trace_start, "module_promotions");
    }

    // Module dependency-graph propagation (prior: same app -> near-isomorphic
    // import graph). Expand the confirmed module matches along aligned import
    // edges, then re-run function matching so the precise within-module passes
    // cascade into the newly-aligned modules.
    let confirmed_modules: BTreeMap<u32, String> = module_matched_file.clone();
    let graph_modules = propagate_module_matches_by_graph(&subjects, &index, &confirmed_modules);
    if graph_modules.len() > confirmed_modules.len() {
        binding_rows = match_function_lists(&subject_fns, &reference_fns, &graph_modules);
        trace_reference_source_names(trace_start, "module_graph_propagation");
    }
    drop_real_name_remaps(&mut binding_rows);

    // Entrypoint-island functions: matched in ISOLATION with an empty
    // `module_matched_file` and `island_mode=true`, so only the distinctive
    // per-function passes (0 composite / 3 anchor-set / 4 AST×rare-anchor) accept —
    // the graded-structure (5) and call-graph (6) passes are disabled because run
    // globally over the island they reach the least-distinctive functions with no
    // way to validate them. The island aggregates functions from across the app and
    // has no single owner reference file. This recovers names for the thousands of
    // root-scope first-party functions that carry no model ModuleId and so never
    // enter the per-module subject set. Kept separate from the per-module flow so
    // it never perturbs module promotion / graph propagation above.
    if let Some(island_source) = island_source.as_deref() {
        let island_fns = collect_island_functions(island_source);
        let mut island_rows =
            match_function_lists_inner(&island_fns, &reference_fns, &BTreeMap::new(), true);
        // Keep only committed accepts; island proposals carry no value downstream.
        island_rows.retain(|row| row.accepted);
        drop_real_name_remaps(&mut island_rows);
        binding_rows.extend(island_rows);
        trace_reference_source_names(trace_start, "island_function_matching");
    }

    write_match_summary_if_requested(&args, subjects.len(), &plans)?;
    write_match_diagnostics_if_requested(&args, subjects.len(), &plans)?;

    if args.module_only {
        if args.apply {
            let mut connection = Connection::open(&args.input)
                .map_err(|error| CliRunError::ReferenceSourceNames(error.to_string()))?;
            let rows = load_project_rows_from_connection(&connection, args.project_id)
                .map_err(|error| CliRunError::ReferenceSourceNames(error.to_string()))?;
            let prepared_rows = prepare_input_rows_for_pipeline(rows);
            persist_prepared_synthetic_inputs(
                &mut connection,
                args.project_id,
                &prepared_rows.rows,
                &prepared_rows.synthetic_modules,
            )
            .map_err(|error| CliRunError::ReferenceSourceNames(error.to_string()))?;
            persist_module_dependencies(&mut connection, &prepared_rows.rows)
                .map_err(|error| CliRunError::ReferenceSourceNames(error.to_string()))?;
            ensure_semantic_name_source_column(&connection)
                .map_err(|e| CliRunError::ReferenceSourceNames(e.to_string()))?;
            let module_count = write_module_names(
                &connection,
                &plans,
                args.min_tier,
                &args.origin_prefix,
                &args.reference_version,
            )?;
            println!("applied module-only: {module_count} module name(s)");
        } else {
            let accepted = plans
                .iter()
                .filter(|plan| tier_passes(plan.matched.tier, args.min_tier))
                .count();
            println!(
                "dry-run module-only: {accepted}/{} module match(es) pass {:?}; pass --apply to write module names",
                subjects.len(),
                args.min_tier
            );
        }
        return Ok(());
    }

    // Symbol propagation: name the module-level symbols that matched functions
    // reference, by lockstep-aligning each accepted isomorphic function pair.
    let reference_source: BTreeMap<&str, &str> = index
        .modules
        .iter()
        .map(|m| (m.file_path.as_str(), m.source.as_str()))
        .collect();
    let propagated = propagate_symbols(
        &subjects,
        &reference_source,
        &subject_fns,
        &reference_fns,
        &binding_rows,
    );
    binding_rows.extend(propagated);
    trace_reference_source_names(trace_start, "propagate_symbols");

    // Cross-evidence reinforcement: confirmed functions AND symbols become anchors
    // to match more functions by reference topology, iterated to a fixpoint.
    propagate_by_reference_topology(&subject_fns, &reference_fns, &mut binding_rows);
    trace_reference_source_names(trace_start, "reference_topology_reinforcement");

    println!(
        "module_id\tsubject_path\tref_version\tref_file\ttier\tsemantic_name\tasset\texport\tfn\ttop_decl\tsurface\tmember\tstmt_win\tblock_branch\tpq_gram\twl\tgranular\tstruct\tgraph\tgraph_known\tanchor\twanchor\tnanchor\tmargin\treciprocal"
    );
    for plan in &plans {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.1}\t{}\t{}\t{}\t{:.1}\t{:.3}\t{:.3}\t{}",
            plan.module_id,
            plan.subject_path,
            plan.reference_version,
            plan.matched.file_path,
            tier_str(plan.matched.tier),
            plan.module_semantic_name,
            plan.matched.asset_overlap,
            plan.matched.export_overlap,
            plan.matched.function_overlap,
            plan.matched.top_level_declaration_overlap,
            plan.matched.import_export_surface_overlap,
            plan.matched.class_member_overlap,
            plan.matched.statement_window_overlap,
            plan.matched.block_branch_overlap,
            plan.matched.pq_gram_overlap,
            plan.matched.wl_overlap,
            granular_match_overlap(&plan.matched),
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
        let mut connection = Connection::open(&args.input)
            .map_err(|error| CliRunError::ReferenceSourceNames(error.to_string()))?;
        let rows = load_project_rows_from_connection(&connection, args.project_id)
            .map_err(|error| CliRunError::ReferenceSourceNames(error.to_string()))?;
        let prepared_rows = prepare_input_rows_for_pipeline(rows);
        persist_prepared_synthetic_inputs(
            &mut connection,
            args.project_id,
            &prepared_rows.rows,
            &prepared_rows.synthetic_modules,
        )
        .map_err(|error| CliRunError::ReferenceSourceNames(error.to_string()))?;
        persist_module_dependencies(&mut connection, &prepared_rows.rows)
            .map_err(|error| CliRunError::ReferenceSourceNames(error.to_string()))?;
        ensure_semantic_name_source_column(&connection)
            .map_err(|e| CliRunError::ReferenceSourceNames(e.to_string()))?;
        ensure_symbol_name_proposals_table(&connection)
            .map_err(|e| CliRunError::ReferenceSourceNames(e.to_string()))?;
        crate::commands::binding_names::ensure_binding_names_table_if_writable(&connection, true)?;
        ensure_module_path_overrides_table(&connection)?;
        let module_count = write_module_names(
            &connection,
            &plans,
            args.min_tier,
            &args.origin_prefix,
            &args.reference_version,
        )?;
        let path_count = write_module_path_overrides(
            &connection,
            args.project_id,
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
            &accepted_module_paths(&plans, args.min_tier),
            &args.origin_prefix,
            &args.reference_version,
        )?;
        println!(
            "applied: {module_count} module name(s), {path_count} module path override(s), {export_count} export name(s), {binding_accepted} binding rename(s), {binding_proposed} binding proposal(s)"
        );
    } else {
        println!(
            "dry-run: {} module match(es); pass --apply to write",
            plans.len()
        );
    }
    Ok(())
}

fn trace_reference_source_names(start: Instant, label: &str) {
    if std::env::var_os("REVERTS_TRACE_REFERENCE_SOURCE_NAMES").is_some() {
        eprintln!(
            "reference-source-names: {label}: {:.3}s",
            start.elapsed().as_secs_f64()
        );
    }
}

/// One subject emitted module: its DB module id, emitted path, fingerprint,
/// and the (original_name -> emitted_name) bindings that land in it.
#[derive(Debug)]
struct SubjectModule {
    module_id: u32,
    file_path: String,
    source: String,
    fingerprint: SourceFingerprint,
    profile: SourceEvidenceProfile,
    dependencies: BTreeSet<u32>,
    bindings: Vec<(String, String)>, // (original_name, emitted_name)
}

fn subject_modules(
    args: &ReferenceSourceNamesArgs,
) -> Result<(Vec<SubjectModule>, Option<String>), CliRunError> {
    let bundle = load_project_bundle_with_package_externalization(&args.input, args.project_id)
        .map_err(|error| CliRunError::ReferenceSourceNames(format!("load input: {error}")))?;
    // Exclude only EXTERNALIZED package modules (`Accepted` → emitted as a runtime
    // `import` of the real npm package); those have no first-party counterpart in
    // the reference tree. `Rejected` modules are package code that was inlined
    // (vendored) rather than externalized — and crucially, a `Rejected` attribution
    // can be a FALSE POSITIVE of the package matcher (e.g. first-party `insights`
    // mis-attributed to highlight.js). Including them as first-party subjects lets
    // a strong cross-version match RECLAIM such mis-attributed modules; genuine
    // inlined package code (react/undici/…) shares no distinctive anchors with any
    // reference file, so the medium-tier accept gate never names it.
    let externalized_modules = bundle
        .package_attributions
        .iter()
        .filter(|attribution| {
            matches!(attribution.status, PackageAttributionStatus::Accepted)
                && attribution.package_version.is_some()
        })
        .map(|attribution| attribution.module_id.0)
        .collect::<BTreeSet<_>>();
    generate_subject_modules(bundle, |module_id| {
        !externalized_modules.contains(&module_id)
    })
}

/// Generate the project and collect [`SubjectModule`]s for every emitted module
/// the `include_module` predicate keeps. Shared by [`subject_modules`] (which
/// keeps non-package modules) and the ownership-driven naming path (which keeps
/// exactly the package-owned modules instead).
fn generate_subject_modules(
    bundle: InputBundle,
    include_module: impl Fn(u32) -> bool,
) -> Result<(Vec<SubjectModule>, Option<String>), CliRunError> {
    // Subject module dependency edges (module_id -> imported module_ids), captured
    // before the bundle is consumed, for graph-based module-match propagation.
    let mut dependency_map: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
    for dependency in &bundle.dependencies {
        let from = dependency.from_module_id.0;
        if !include_module(from) {
            continue;
        }
        if let ModuleDependencyTarget::Module(to) = dependency.target
            && include_module(to.0)
        {
            dependency_map.entry(from).or_default().insert(to.0);
        }
    }
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
        if !include_module(entry.module_id.0) {
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
        .filter(|(module_id, _)| include_module(module_id.0))
        .map(|(module_id, path)| (path.clone(), module_id.0))
        .collect::<BTreeMap<_, _>>();

    let mut modules = Vec::new();
    // The entrypoint island aggregates thousands of root-scope first-party
    // functions but is not a model module (no `ModuleId`), so it is absent from
    // `module_for_path` and would otherwise be silently dropped. Capture its
    // source so its functions can be matched as synthetic-module subjects.
    let mut island_source = None;
    for file in &run.project.files {
        let Some(&module_id) = module_for_path.get(file.path.as_str()) else {
            if file.path == ENTRYPOINT_ISLAND_PATH {
                island_source = Some(file.source.clone());
            }
            continue; // scaffold/runtime file with no owning module
        };
        let Ok(fingerprint) = fingerprint_source(file.path.as_str(), file.source.as_str()) else {
            continue;
        };
        let profile = build_source_evidence_profile_with_fingerprint(
            file.path.as_str(),
            file.source.as_str(),
            fingerprint.clone(),
        );
        modules.push(SubjectModule {
            module_id,
            file_path: file.path.clone(),
            source: file.source.clone(),
            fingerprint,
            profile,
            dependencies: dependency_map.get(&module_id).cloned().unwrap_or_default(),
            bindings: bindings_for_path
                .remove(file.path.as_str())
                .unwrap_or_default(),
        });
    }
    Ok((modules, island_source))
}

fn subject_modules_from_extracted_input(
    args: &ReferenceSourceNamesArgs,
) -> Result<Vec<SubjectModule>, CliRunError> {
    let rows = load_project_rows_from_sqlite(&args.input, args.project_id)
        .map_err(|error| CliRunError::ReferenceSourceNames(format!("load input rows: {error}")))?;
    let package_owned_modules = package_owned_modules_from_rows(&rows);
    let prepared = prepare_input_rows_for_pipeline(rows);
    Ok(subject_modules_from_prepared_rows(
        &prepared.rows,
        &package_owned_modules,
    ))
}

/// Externalized (`Accepted`) package modules only — see [`subject_modules`] for
/// why `Rejected` (inlined / possibly mis-attributed) modules stay eligible as
/// first-party subjects.
fn package_owned_modules_from_rows(rows: &InputRows) -> BTreeSet<u32> {
    rows.package_attributions
        .iter()
        .filter(|attribution| {
            matches!(attribution.status, PackageAttributionStatus::Accepted)
                && attribution.package_version.is_some()
        })
        .map(|attribution| attribution.module_id.0)
        .collect()
}

fn subject_modules_from_prepared_rows(
    rows: &InputRows,
    package_owned_modules: &BTreeSet<u32>,
) -> Vec<SubjectModule> {
    let mut modules = Vec::new();
    let dependency_map = subject_dependency_map(rows, package_owned_modules);
    for module in &rows.modules {
        if package_owned_modules.contains(&module.id.0) {
            continue;
        }
        let Some(slice) = rows.module_source_slice(module.id) else {
            continue;
        };
        let file_path = module.semantic_path.clone();
        let Ok(fingerprint) = fingerprint_source(file_path.as_str(), slice.source) else {
            continue;
        };
        let profile = build_source_evidence_profile_with_fingerprint(
            file_path.as_str(),
            slice.source,
            fingerprint.clone(),
        );
        modules.push(SubjectModule {
            module_id: module.id.0,
            file_path,
            source: slice.source.to_string(),
            fingerprint,
            profile,
            dependencies: dependency_map
                .get(&module.id.0)
                .cloned()
                .unwrap_or_default(),
            bindings: Vec::new(),
        });
    }
    modules
}

fn subject_dependency_map(
    rows: &InputRows,
    package_owned_modules: &BTreeSet<u32>,
) -> BTreeMap<u32, BTreeSet<u32>> {
    rows.dependencies
        .iter()
        .filter_map(|dependency| {
            let from = dependency.from_module_id.0;
            if package_owned_modules.contains(&from) {
                return None;
            }
            let ModuleDependencyTarget::Module(module_id) = dependency.target else {
                return None;
            };
            let to = module_id.0;
            if package_owned_modules.contains(&to) {
                return None;
            }
            Some((from, to))
        })
        .fold(
            BTreeMap::<u32, BTreeSet<u32>>::new(),
            |mut acc, (from, to)| {
                acc.entry(from).or_default().insert(to);
                acc
            },
        )
}

fn write_match_summary_if_requested(
    args: &ReferenceSourceNamesArgs,
    subject_count: usize,
    plans: &[ModulePlan],
) -> Result<(), CliRunError> {
    let Some(path) = &args.summary_json else {
        return Ok(());
    };
    let high = plans
        .iter()
        .filter(|plan| plan.matched.tier == MatchTier::High)
        .count();
    let medium = plans
        .iter()
        .filter(|plan| plan.matched.tier == MatchTier::Medium)
        .count();
    let low = plans
        .iter()
        .filter(|plan| plan.matched.tier == MatchTier::Low)
        .count();
    let accepted = plans
        .iter()
        .filter(|plan| tier_passes(plan.matched.tier, args.min_tier))
        .count();
    let distinct_reference_files = plans
        .iter()
        .filter(|plan| tier_passes(plan.matched.tier, args.min_tier))
        .map(|plan| plan.matched.file_path.as_str())
        .collect::<BTreeSet<_>>()
        .len();
    let accepted_rule_counts = match_rule_counts(
        plans
            .iter()
            .filter(|plan| tier_passes(plan.matched.tier, args.min_tier))
            .map(|plan| &plan.matched),
    );
    let match_rate = if subject_count == 0 {
        0.0
    } else {
        accepted as f64 / subject_count as f64
    };
    let summary = serde_json::json!({
        "subject_modules": subject_count,
        "planned_matches": plans.len(),
        "accepted_matches": accepted,
        "match_rate": match_rate,
        "min_tier": match args.min_tier {
            MinTier::High => "high",
            MinTier::Medium => "medium",
        },
        "tiers": {
            "high": high,
            "medium": medium,
            "low": low,
        },
        "accepted_rule_counts": accepted_rule_counts,
        "distinct_reference_files": distinct_reference_files,
        "module_only": args.module_only,
    });
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|source| CliRunError::WriteOutput {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(
        path,
        serde_json::to_string_pretty(&summary)
            .expect("reference source match summary is serializable"),
    )
    .map_err(|source| CliRunError::WriteOutput {
        path: path.clone(),
        source,
    })
}

fn write_match_diagnostics_if_requested(
    args: &ReferenceSourceNamesArgs,
    subject_count: usize,
    plans: &[ModulePlan],
) -> Result<(), CliRunError> {
    let Some(path) = &args.diagnostics_json else {
        return Ok(());
    };
    let collision_groups = reference_collision_groups(plans);
    let mut reason_counts = BTreeMap::<&'static str, usize>::new();
    let mut low_rows = plans
        .iter()
        .filter(|plan| plan.matched.tier == MatchTier::Low)
        .map(|plan| {
            let reason = low_boundary_reason(&plan.matched);
            *reason_counts.entry(reason).or_default() += 1;
            (low_medium_closeness(&plan.matched), reason, plan)
        })
        .collect::<Vec<_>>();
    low_rows.sort_by(|left, right| {
        right
            .0
            .total_cmp(&left.0)
            .then_with(|| left.2.module_id.cmp(&right.2.module_id))
    });
    let near_medium = low_rows
        .iter()
        .filter(|(closeness, _reason, _plan)| *closeness >= 0.80)
        .count();
    let accepted = plans
        .iter()
        .filter(|plan| tier_passes(plan.matched.tier, args.min_tier))
        .count();
    let reason_counts_json = reason_counts
        .iter()
        .map(|(reason, count)| {
            serde_json::json!({
                "reason": reason,
                "count": count,
            })
        })
        .collect::<Vec<_>>();
    let low_details = low_rows
        .iter()
        .map(|(closeness, reason, plan)| {
            low_boundary_row_json(plan, reason, *closeness, &collision_groups)
        })
        .collect::<Vec<_>>();
    let ambiguous_report = ambiguous_runner_up_report_json(&low_rows);
    let anchor_quality_report = anchor_quality_report_json(&low_rows);
    let dry_run_evaluator =
        dry_run_evaluator_json(args.min_tier, plans, &low_rows, &collision_groups);
    let summary = serde_json::json!({
        "subject_modules": subject_count,
        "planned_matches": plans.len(),
        "accepted_matches": accepted,
        "low_matches": low_rows.len(),
        "near_medium_low_matches": near_medium,
        "min_tier": match args.min_tier {
            MinTier::High => "high",
            MinTier::Medium => "medium",
        },
        "module_only": args.module_only,
        "thresholds": {
            "medium_weighted_anchor": MEDIUM_WEIGHTED_ANCHOR,
            "medium_normalized_anchor": MEDIUM_NORMALIZED_ANCHOR,
            "medium_score_margin": MEDIUM_SCORE_MARGIN,
            "medium_strong_normalized_anchor": MEDIUM_STRONG_NANCHOR,
            "medium_guarded_strong_normalized_anchor": MEDIUM_GUARDED_STRONG_NANCHOR,
            "medium_sourced_guarded_strong_normalized_anchor": MEDIUM_SOURCED_GUARDED_STRONG_NANCHOR,
            "medium_reciprocal_weighted_anchor": MEDIUM_RECIPROCAL_WEIGHTED_ANCHOR,
            "medium_reciprocal_normalized_anchor": MEDIUM_RECIPROCAL_NORMALIZED_ANCHOR,
            "medium_reciprocal_near_weighted_anchor": MEDIUM_RECIPROCAL_NEAR_WEIGHTED_ANCHOR,
            "medium_reciprocal_near_normalized_anchor": MEDIUM_RECIPROCAL_NEAR_NORMALIZED_ANCHOR,
            "medium_structural_score": MEDIUM_STRUCTURAL_SCORE,
            "medium_structural_weighted_anchor": MEDIUM_STRUCTURAL_WEIGHTED_ANCHOR,
            "medium_structural_normalized_anchor": MEDIUM_STRUCTURAL_NORMALIZED_ANCHOR,
            "medium_content_normalized_floor": MEDIUM_CONTENT_NORMALIZED_FLOOR,
        },
        "reason_counts": reason_counts_json,
        "ambiguous_runner_up_report": ambiguous_report,
        "anchor_quality_report": anchor_quality_report,
        "dry_run_evaluator": dry_run_evaluator,
        "low_matches_by_closeness": low_details,
    });
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|source| CliRunError::WriteOutput {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(
        path,
        serde_json::to_string_pretty(&summary)
            .expect("reference source match diagnostics are serializable"),
    )
    .map_err(|source| CliRunError::WriteOutput {
        path: path.clone(),
        source,
    })
}

fn reference_collision_groups(plans: &[ModulePlan]) -> BTreeMap<&str, Vec<&ModulePlan>> {
    let mut groups = BTreeMap::<&str, Vec<&ModulePlan>>::new();
    for plan in plans {
        groups
            .entry(plan.matched.file_path.as_str())
            .or_default()
            .push(plan);
    }
    groups
}

fn low_boundary_row_json(
    plan: &ModulePlan,
    reason: &'static str,
    closeness: f64,
    collision_groups: &BTreeMap<&str, Vec<&ModulePlan>>,
) -> serde_json::Value {
    let matched = &plan.matched;
    let collision_group = collision_groups
        .get(matched.file_path.as_str())
        .map(|group| collision_group_json(group));
    serde_json::json!({
        "module_id": plan.module_id,
        "subject_path": plan.subject_path,
        "reference_file": matched.file_path,
        "reason": reason,
        "closeness_to_medium": closeness,
        "top_candidate": candidate_diagnostic_json(
            plan.top_candidate.relevance,
            &plan.top_candidate.matched
        ),
        "runner_up": plan
            .runner_up
            .as_ref()
            .map(|candidate| candidate_diagnostic_json(candidate.relevance, &candidate.matched)),
        "collision_group": collision_group,
        "shared_anchors": shared_anchor_statistics_json(plan),
        "source_score": source_score_json(matched.source_score),
        "rule_tags": match_rule_tags(matched),
        "metrics": {
            "margin": matched.margin,
            "reciprocal_best": matched.reciprocal_best,
            "anchor_overlap": matched.anchor_overlap,
            "weighted_anchor": matched.weighted_anchor,
            "normalized_anchor": matched.normalized_anchor,
            "asset_overlap": matched.asset_overlap,
            "export_overlap": matched.export_overlap,
            "function_overlap": matched.function_overlap,
            "top_level_declaration_overlap": matched.top_level_declaration_overlap,
            "import_export_surface_overlap": matched.import_export_surface_overlap,
            "class_member_overlap": matched.class_member_overlap,
            "statement_window_overlap": matched.statement_window_overlap,
            "block_branch_overlap": matched.block_branch_overlap,
            "pq_gram_overlap": matched.pq_gram_overlap,
            "wl_overlap": matched.wl_overlap,
            "granular_hash_overlap": granular_match_overlap(matched),
            "granular_hash_containment": matched.granular_hash_containment,
            "statement_window_containment": matched.statement_window_containment,
            "block_branch_containment": matched.block_branch_containment,
            "structural_score": matched.structural_score,
            "graph_support": matched.graph_support,
            "graph_known_edges": matched.graph_known_edges,
            "matched_neighbor_ratio": matched_neighbor_ratio(matched),
        },
        "graph_structure": graph_structure_json(matched.graph_structure),
        "runner_up_delta": plan
            .runner_up
            .as_ref()
            .map(|candidate| candidate_delta_json(matched, &candidate.matched)),
        "shortfalls": {
            "strong_normalized_anchor": positive_shortfall(MEDIUM_STRONG_NANCHOR, matched.normalized_anchor),
            "guarded_strong_normalized_anchor": positive_shortfall(MEDIUM_GUARDED_STRONG_NANCHOR, matched.normalized_anchor),
            "sourced_guarded_strong_normalized_anchor": positive_shortfall(MEDIUM_SOURCED_GUARDED_STRONG_NANCHOR, matched.normalized_anchor),
            "weighted_anchor": positive_shortfall(MEDIUM_WEIGHTED_ANCHOR, matched.weighted_anchor),
            "normalized_anchor": positive_shortfall(MEDIUM_NORMALIZED_ANCHOR, matched.normalized_anchor),
            "reciprocal_weighted_anchor": positive_shortfall(MEDIUM_RECIPROCAL_WEIGHTED_ANCHOR, matched.weighted_anchor),
            "reciprocal_normalized_anchor": positive_shortfall(MEDIUM_RECIPROCAL_NORMALIZED_ANCHOR, matched.normalized_anchor),
            "reciprocal_near_weighted_anchor": positive_shortfall(MEDIUM_RECIPROCAL_NEAR_WEIGHTED_ANCHOR, matched.weighted_anchor),
            "reciprocal_near_normalized_anchor": positive_shortfall(MEDIUM_RECIPROCAL_NEAR_NORMALIZED_ANCHOR, matched.normalized_anchor),
            "content_normalized_floor": positive_shortfall(MEDIUM_CONTENT_NORMALIZED_FLOOR, matched.normalized_anchor),
            "margin": positive_shortfall(MEDIUM_SCORE_MARGIN, matched.margin),
        },
    })
}

fn ambiguous_runner_up_report_json(
    low_rows: &[(f64, &'static str, &ModulePlan)],
) -> serde_json::Value {
    let ambiguous_rows = low_rows
        .iter()
        .filter(|(_closeness, reason, _plan)| *reason == "ambiguous_runner_up_or_weak_content")
        .collect::<Vec<_>>();
    let mut reference_families = BTreeMap::<String, usize>::new();
    let mut runner_up_families = BTreeMap::<String, usize>::new();
    let mut family_pairs = BTreeMap::<(String, String), usize>::new();
    let mut exact_references = BTreeMap::<String, usize>::new();
    let mut same_family_runner_up = 0usize;
    let mut promising = Vec::new();
    for (_closeness, _reason, plan) in &ambiguous_rows {
        let reference_family = diagnostic_path_family(&plan.matched.file_path);
        let runner_up_reference = plan
            .runner_up
            .as_ref()
            .map(|runner_up| runner_up.matched.file_path.as_str())
            .unwrap_or("<none>");
        let runner_up_family = diagnostic_path_family(runner_up_reference);
        if reference_family == runner_up_family {
            same_family_runner_up += 1;
        }
        *reference_families
            .entry(reference_family.clone())
            .or_default() += 1;
        *runner_up_families
            .entry(runner_up_family.clone())
            .or_default() += 1;
        *family_pairs
            .entry((reference_family, runner_up_family))
            .or_default() += 1;
        *exact_references
            .entry(plan.matched.file_path.clone())
            .or_default() += 1;
        if is_promising_ambiguous_diagnostic(plan) {
            promising.push(*plan);
        }
    }
    promising.sort_by(|left, right| {
        right
            .matched
            .source_score
            .unique_string_anchor_overlap
            .cmp(&left.matched.source_score.unique_string_anchor_overlap)
            .then_with(|| {
                right
                    .matched
                    .source_score
                    .function_axis_jaccard
                    .total_cmp(&left.matched.source_score.function_axis_jaccard)
            })
            .then_with(|| right.matched.margin.total_cmp(&left.matched.margin))
    });
    serde_json::json!({
        "count": ambiguous_rows.len(),
        "same_family_runner_up_count": same_family_runner_up,
        "reference_families": sorted_count_map_json(&reference_families, "family"),
        "runner_up_families": sorted_count_map_json(&runner_up_families, "family"),
        "family_pairs": sorted_pair_count_map_json(&family_pairs),
        "exact_references": sorted_count_map_json(&exact_references, "reference_file"),
        "promising_review_candidates": promising
            .into_iter()
            .take(50)
            .map(promising_ambiguous_json)
            .collect::<Vec<_>>(),
    })
}

fn anchor_quality_report_json(low_rows: &[(f64, &'static str, &ModulePlan)]) -> serde_json::Value {
    let global_rows = low_rows
        .iter()
        .filter(|(_closeness, reason, _plan)| reason.starts_with("global_uniqueness_demoted"))
        .collect::<Vec<_>>();
    let mut anchor_counts = BTreeMap::<String, usize>::new();
    let mut weak_only_rows = 0usize;
    let mut strongish_rows = 0usize;
    let mut no_anchor_rows = 0usize;
    for (_closeness, _reason, plan) in &global_rows {
        if plan.shared_string_anchors.is_empty() {
            no_anchor_rows += 1;
        } else if plan
            .shared_string_anchors
            .iter()
            .all(|anchor| is_weak_diagnostic_anchor(anchor))
        {
            weak_only_rows += 1;
        } else {
            strongish_rows += 1;
        }
        for anchor in &plan.shared_string_anchors {
            *anchor_counts.entry(anchor.clone()).or_default() += 1;
        }
    }
    let mut top_anchors = anchor_counts.into_iter().collect::<Vec<_>>();
    top_anchors.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    serde_json::json!({
        "global_demoted_count": global_rows.len(),
        "weak_only_rows": weak_only_rows,
        "strongish_rows": strongish_rows,
        "no_anchor_rows": no_anchor_rows,
        "top_shared_anchors": top_anchors
            .into_iter()
            .take(100)
            .map(|(anchor, count)| {
                serde_json::json!({
                    "anchor": anchor,
                    "count": count,
                    "weak": is_weak_diagnostic_anchor(&anchor),
                })
            })
            .collect::<Vec<_>>(),
    })
}

fn sorted_count_map_json(map: &BTreeMap<String, usize>, label: &str) -> Vec<serde_json::Value> {
    let mut rows = map.iter().collect::<Vec<_>>();
    rows.sort_by(|left, right| right.1.cmp(left.1).then_with(|| left.0.cmp(right.0)));
    rows.into_iter()
        .map(|(key, count)| {
            let mut object = serde_json::Map::new();
            object.insert(label.to_string(), serde_json::json!(key));
            object.insert("count".to_string(), serde_json::json!(count));
            serde_json::Value::Object(object)
        })
        .collect()
}

fn sorted_pair_count_map_json(map: &BTreeMap<(String, String), usize>) -> Vec<serde_json::Value> {
    let mut rows = map.iter().collect::<Vec<_>>();
    rows.sort_by(|left, right| right.1.cmp(left.1).then_with(|| left.0.cmp(right.0)));
    rows.into_iter()
        .map(|((reference_family, runner_up_family), count)| {
            serde_json::json!({
                "reference_family": reference_family,
                "runner_up_family": runner_up_family,
                "count": count,
            })
        })
        .collect()
}

fn promising_ambiguous_json(plan: &ModulePlan) -> serde_json::Value {
    serde_json::json!({
        "module_id": plan.module_id,
        "reference_file": plan.matched.file_path,
        "runner_up_reference": plan
            .runner_up
            .as_ref()
            .map(|runner_up| runner_up.matched.file_path.as_str()),
        "margin": plan.matched.margin,
        "normalized_anchor": plan.matched.normalized_anchor,
        "weighted_anchor": plan.matched.weighted_anchor,
        "source_score": source_score_json(plan.matched.source_score),
        "shared_anchors": shared_anchor_statistics_json(plan),
    })
}

fn is_promising_ambiguous_diagnostic(plan: &ModulePlan) -> bool {
    let score = plan.matched.source_score;
    (score.function_axis_jaccard >= 0.10
        && score.function_axis_containment >= 0.30
        && plan.matched.margin >= 0.10)
        || score.unique_string_anchor_overlap >= 2
        || score.jsx_react_shape_jaccard >= 0.20
        || score.anchor_cooccurrence_jaccard >= 0.12
}

fn diagnostic_path_family(path: &str) -> String {
    let mut parts = path.split('/');
    let first = parts.next().unwrap_or(path);
    if first == "<none>" {
        return first.to_string();
    }
    match parts.next() {
        Some(second) => format!("{first}/{second}"),
        None => first.to_string(),
    }
}

fn is_weak_diagnostic_anchor(anchor: &str) -> bool {
    let anchor = anchor.trim();
    let generic = anchor
        .strip_prefix("object-key:")
        .or_else(|| anchor.strip_prefix("class-method:instance:method:"))
        .or_else(|| anchor.strip_prefix("class-method:static:method:"))
        .or_else(|| anchor.strip_prefix("class-method:instance:get:"))
        .unwrap_or(anchor);
    generic.len() <= 3
        || matches!(
            generic,
            "function"
                | "data"
                | "error"
                | "buffer"
                | "base64"
                | "config"
                | "stats"
                | "stream"
                | "message"
                | "request"
                | "response"
                | "content"
                | "value"
                | "string"
                | "number"
                | "object"
                | "array"
                | "result"
                | "event"
                | "input"
                | "output"
                | "type"
                | "name"
                | "path"
                | "file"
                | "files"
                | "start"
                | "end"
                | "line"
                | "token"
                | "status"
                | "command"
                | "model"
                | "tool"
                | "tools"
                | "user"
                | "system"
                | "true"
                | "false"
                | "get"
                | "set"
                | "load"
                | "size"
                | "description"
                | "required"
                | "dependencies"
                | "stub"
                | "local"
                | "util"
                | "class"
                | "interface"
                | "namespace"
                | "const"
                | "boolean"
                | "symbol"
                | "left"
                | "right"
                | "&amp;"
                | "&gt;"
                | "&lt;"
                | "&quot;"
                | "─"
                | "│"
                | "┌"
                | "┐"
                | "└"
                | "┘"
                | "├"
                | "┤"
                | "┼"
        )
}

fn candidate_diagnostic_json(relevance: f64, matched: &ModuleMatch) -> serde_json::Value {
    serde_json::json!({
        "reference_file": matched.file_path,
        "tier": tier_str(matched.tier),
        "relevance": relevance,
        "metrics": {
            "anchor_overlap": matched.anchor_overlap,
            "weighted_anchor": matched.weighted_anchor,
            "normalized_anchor": matched.normalized_anchor,
            "asset_overlap": matched.asset_overlap,
            "export_overlap": matched.export_overlap,
            "function_overlap": matched.function_overlap,
            "top_level_declaration_overlap": matched.top_level_declaration_overlap,
            "import_export_surface_overlap": matched.import_export_surface_overlap,
            "class_member_overlap": matched.class_member_overlap,
            "statement_window_overlap": matched.statement_window_overlap,
            "block_branch_overlap": matched.block_branch_overlap,
            "pq_gram_overlap": matched.pq_gram_overlap,
            "wl_overlap": matched.wl_overlap,
            "granular_hash_overlap": granular_match_overlap(matched),
            "granular_hash_containment": matched.granular_hash_containment,
            "statement_window_containment": matched.statement_window_containment,
            "block_branch_containment": matched.block_branch_containment,
            "structural_score": matched.structural_score,
            "graph_support": matched.graph_support,
            "graph_known_edges": matched.graph_known_edges,
            "matched_neighbor_ratio": matched_neighbor_ratio(matched),
        },
        "graph_structure": graph_structure_json(matched.graph_structure),
        "source_score": source_score_json(matched.source_score),
    })
}

fn candidate_delta_json(top: &ModuleMatch, runner_up: &ModuleMatch) -> serde_json::Value {
    serde_json::json!({
        "weighted_anchor": top.weighted_anchor - runner_up.weighted_anchor,
        "normalized_anchor": top.normalized_anchor - runner_up.normalized_anchor,
        "statement_window_overlap": top.statement_window_overlap as isize
            - runner_up.statement_window_overlap as isize,
        "block_branch_overlap": top.block_branch_overlap as isize
            - runner_up.block_branch_overlap as isize,
        "granular_hash_overlap": granular_match_overlap(top) as isize
            - granular_match_overlap(runner_up) as isize,
        "granular_hash_containment": top.granular_hash_containment
            - runner_up.granular_hash_containment,
        "statement_window_containment": top.statement_window_containment
            - runner_up.statement_window_containment,
        "block_branch_containment": top.block_branch_containment
            - runner_up.block_branch_containment,
        "structural_score": top.structural_score - runner_up.structural_score,
        "graph_support": top.graph_support as isize - runner_up.graph_support as isize,
        "matched_neighbor_ratio": matched_neighbor_ratio(top) - matched_neighbor_ratio(runner_up),
        "unique_string_anchor_overlap": top.source_score.unique_string_anchor_overlap as isize
            - runner_up.source_score.unique_string_anchor_overlap as isize,
        "function_axis_overlap": top.source_score.function_axis_overlap as isize
            - runner_up.source_score.function_axis_overlap as isize,
        "function_axis_jaccard": top.source_score.function_axis_jaccard
            - runner_up.source_score.function_axis_jaccard,
        "jsx_react_shape_overlap": top.source_score.jsx_react_shape_overlap as isize
            - runner_up.source_score.jsx_react_shape_overlap as isize,
        "anchor_cooccurrence_overlap": top.source_score.anchor_cooccurrence_overlap as isize
            - runner_up.source_score.anchor_cooccurrence_overlap as isize,
        "anchor_cooccurrence_jaccard": top.source_score.anchor_cooccurrence_jaccard
            - runner_up.source_score.anchor_cooccurrence_jaccard,
    })
}

fn collision_group_json(group: &[&ModulePlan]) -> serde_json::Value {
    let strongest = group.iter().max_by(|left, right| {
        left.top_candidate
            .relevance
            .total_cmp(&right.top_candidate.relevance)
    });
    let medium_module_ids = group
        .iter()
        .filter(|plan| plan.matched.tier == MatchTier::Medium)
        .map(|plan| plan.module_id)
        .collect::<Vec<_>>();
    let low_module_ids = group
        .iter()
        .filter(|plan| plan.matched.tier == MatchTier::Low)
        .map(|plan| plan.module_id)
        .collect::<Vec<_>>();
    serde_json::json!({
        "size": group.len(),
        "medium_count": medium_module_ids.len(),
        "low_count": low_module_ids.len(),
        "strongest_module_id": strongest.map(|plan| plan.module_id),
        "module_ids_sample": group.iter().map(|plan| plan.module_id).take(20).collect::<Vec<_>>(),
        "medium_module_ids_sample": medium_module_ids.into_iter().take(20).collect::<Vec<_>>(),
        "low_module_ids_sample": low_module_ids.into_iter().take(20).collect::<Vec<_>>(),
    })
}

fn shared_anchor_statistics_json(plan: &ModulePlan) -> serde_json::Value {
    serde_json::json!({
        "count": plan.shared_string_anchors.len(),
        "weighted_overlap": plan.matched.weighted_anchor,
        "normalized_overlap": plan.matched.normalized_anchor,
        "sample": plan
            .shared_string_anchors
            .iter()
            .take(20)
            .collect::<Vec<_>>(),
    })
}

fn source_score_json(score: SourceEvidenceScore) -> serde_json::Value {
    serde_json::json!({
        "hash_match": score.hash_match,
        "function_axis_overlap": score.function_axis_overlap,
        "function_axis_jaccard": score.function_axis_jaccard,
        "function_axis_containment": score.function_axis_containment,
        "weighted_string_anchor": score.weighted_string_anchor,
        "normalized_string_anchor": score.normalized_string_anchor,
        "unique_string_anchor_overlap": score.unique_string_anchor_overlap,
        "jsx_react_shape_overlap": score.jsx_react_shape_overlap,
        "jsx_react_shape_jaccard": score.jsx_react_shape_jaccard,
        "anchor_cooccurrence_overlap": score.anchor_cooccurrence_overlap,
        "anchor_cooccurrence_jaccard": score.anchor_cooccurrence_jaccard,
    })
}

fn dry_run_evaluator_json(
    min_tier: MinTier,
    plans: &[ModulePlan],
    low_rows: &[(f64, &'static str, &ModulePlan)],
    collision_groups: &BTreeMap<&str, Vec<&ModulePlan>>,
) -> serde_json::Value {
    let accepted = plans
        .iter()
        .filter(|plan| tier_passes(plan.matched.tier, min_tier))
        .flat_map(|plan| {
            match_rule_tags(&plan.matched).into_iter().map(move |rule| {
                rule_contribution_json(plan, rule, true, min_tier, collision_groups)
            })
        })
        .collect::<Vec<_>>();
    let near_medium = low_rows
        .iter()
        .filter(|(closeness, _reason, _plan)| *closeness >= 0.80)
        .flat_map(|(_closeness, _reason, plan)| {
            match_rule_tags(&plan.matched).into_iter().map(move |rule| {
                rule_contribution_json(plan, rule, false, min_tier, collision_groups)
            })
        })
        .collect::<Vec<_>>();
    let accepted_new_rule_counts = rule_contribution_counts(plans.iter().filter_map(|plan| {
        if tier_passes(plan.matched.tier, min_tier) {
            Some(&plan.matched)
        } else {
            None
        }
    }));
    serde_json::json!({
        "description": "Dry-run rule contribution rows for accepted and near-medium candidates. baseline_accept_proxy is an in-run approximation of pre-advanced-rule acceptance, not persisted history.",
        "new_rule_tags": NEW_RULE_TAGS,
        "accepted_rule_counts": accepted_new_rule_counts,
        "accepted_rule_contributions": accepted,
        "near_medium_rule_contributions": near_medium,
    })
}

const NEW_RULE_TAGS: &[&str] = &[
    "import_export_surface",
    "object_class_shape",
    "top_level_ts_shape",
];

fn rule_contribution_counts<'a>(
    matches: impl Iterator<Item = &'a ModuleMatch>,
) -> Vec<serde_json::Value> {
    let mut counts = BTreeMap::<&'static str, (usize, usize, usize)>::new();
    for matched in matches {
        let baseline = baseline_acceptance_proxy(matched);
        for tag in match_rule_tags(matched) {
            let entry = counts.entry(tag).or_default();
            entry.0 += 1;
            if !baseline {
                entry.1 += 1;
            }
            if is_new_rule_tag(tag) {
                entry.2 += 1;
            }
        }
    }
    let mut rows = counts.into_iter().collect::<Vec<_>>();
    rows.sort_by(|left, right| right.1.0.cmp(&left.1.0).then_with(|| left.0.cmp(right.0)));
    rows.into_iter()
        .map(|(rule, (count, baseline_delta_count, new_rule_count))| {
            serde_json::json!({
                "rule": rule,
                "count": count,
                "baseline_delta_count": baseline_delta_count,
                "new_rule_count": new_rule_count,
            })
        })
        .collect()
}

fn rule_contribution_json(
    plan: &ModulePlan,
    rule: &'static str,
    accepted: bool,
    min_tier: MinTier,
    collision_groups: &BTreeMap<&str, Vec<&ModulePlan>>,
) -> serde_json::Value {
    let collision_group = collision_groups
        .get(plan.matched.file_path.as_str())
        .map(|group| collision_group_json(group));
    serde_json::json!({
        "rule": rule,
        "new_rule": is_new_rule_tag(rule),
        "module_id": plan.module_id,
        "subject_path": plan.subject_path,
        "reference_file": plan.matched.file_path,
        "reference_version": plan.reference_version,
        "tier": tier_str(plan.matched.tier),
        "accepted": accepted,
        "passes_min_tier": tier_passes(plan.matched.tier, min_tier),
        "baseline_accept_proxy": baseline_acceptance_proxy(&plan.matched),
        "top_relevance": plan.top_candidate.relevance,
        "runner_up_reference": plan
            .runner_up
            .as_ref()
            .map(|runner_up| runner_up.matched.file_path.as_str()),
        "runner_up_relevance": plan.runner_up.as_ref().map(|runner_up| runner_up.relevance),
        "runner_up_delta": plan
            .runner_up
            .as_ref()
            .map(|runner_up| candidate_delta_json(&plan.matched, &runner_up.matched)),
        "collision_group": collision_group,
        "source_score": source_score_json(plan.matched.source_score),
    })
}

fn is_new_rule_tag(tag: &str) -> bool {
    NEW_RULE_TAGS.contains(&tag)
}

fn baseline_acceptance_proxy(matched: &ModuleMatch) -> bool {
    matched.asset_overlap > 0
        || matched.source_score.hash_match
        || matched.anchor_overlap > 0
        || matched.weighted_anchor >= MEDIUM_WEIGHTED_ANCHOR
        || matched.normalized_anchor >= MEDIUM_NORMALIZED_ANCHOR
        || matched.function_overlap > 0
        || matched.source_score.function_axis_overlap > 0
        || matched.source_score.unique_string_anchor_overlap > 0
}

fn match_rule_counts<'a>(matches: impl Iterator<Item = &'a ModuleMatch>) -> Vec<serde_json::Value> {
    let mut counts = BTreeMap::<&'static str, usize>::new();
    for matched in matches {
        for tag in match_rule_tags(matched) {
            *counts.entry(tag).or_default() += 1;
        }
    }
    let mut rows = counts.into_iter().collect::<Vec<_>>();
    rows.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    rows.into_iter()
        .map(|(rule, count)| {
            serde_json::json!({
                "rule": rule,
                "count": count,
            })
        })
        .collect()
}

fn match_rule_tags(matched: &ModuleMatch) -> Vec<&'static str> {
    let mut tags = Vec::new();
    if matched.asset_overlap > 0 {
        tags.push("asset");
    }
    if matched.anchor_overlap > 0 || matched.weighted_anchor > 0.0 {
        tags.push("string_anchor");
    }
    if matched.source_score.unique_string_anchor_overlap > 0 {
        tags.push("unique_string_anchor");
    }
    if matched.source_score.anchor_cooccurrence_overlap > 0 {
        tags.push("anchor_cooccurrence");
    }
    if matched.source_score.jsx_react_shape_overlap > 0 {
        tags.push("jsx_react_shape");
    }
    if matched.source_score.function_axis_overlap > 0 || matched.function_overlap > 0 {
        tags.push("function_axis");
    }
    if granular_match_overlap(matched) > 0 {
        tags.push("multi_granular_hash");
    }
    if matched.import_export_surface_overlap > 0 {
        tags.push("import_export_surface");
    }
    if matched.class_member_overlap > 0 {
        tags.push("object_class_shape");
    }
    if matched.top_level_declaration_overlap > 0 {
        tags.push("top_level_ts_shape");
    }
    if matched.structural_score > 0.0 {
        tags.push("structural_score");
    }
    if matched.graph_support > 0 || matched.graph_structure.role_match {
        tags.push("graph");
    }
    if matched.reciprocal_best {
        tags.push("reciprocal_best");
    }
    tags
}

fn graph_structure_json(evidence: GraphStructureEvidence) -> serde_json::Value {
    serde_json::json!({
        "subject_role_signature": graph_role_signature(
            evidence.subject_in_degree,
            evidence.subject_out_degree
        ),
        "reference_role_signature": graph_role_signature(
            evidence.reference_in_degree,
            evidence.reference_out_degree
        ),
        "role_match": evidence.role_match,
        "subject_has_edges": evidence.subject_has_edges,
        "reference_has_edges": evidence.reference_has_edges,
        "subject_in_degree": evidence.subject_in_degree,
        "subject_out_degree": evidence.subject_out_degree,
        "reference_in_degree": evidence.reference_in_degree,
        "reference_out_degree": evidence.reference_out_degree,
        "subject_neighborhood_hash": format!("{:016x}", evidence.subject_neighborhood_hash),
        "reference_neighborhood_hash": format!("{:016x}", evidence.reference_neighborhood_hash),
        "neighborhood_hash_match": evidence.neighborhood_hash_match,
    })
}

fn matched_neighbor_ratio(matched: &ModuleMatch) -> f64 {
    if matched.graph_known_edges == 0 {
        0.0
    } else {
        matched.graph_support as f64 / matched.graph_known_edges as f64
    }
}

fn low_boundary_reason(matched: &ModuleMatch) -> &'static str {
    if matched.normalized_anchor >= MEDIUM_STRONG_NANCHOR {
        return "global_uniqueness_demoted_strong_content";
    }
    if matched.normalized_anchor >= MEDIUM_GUARDED_STRONG_NANCHOR
        && matched.margin >= MEDIUM_SCORE_MARGIN
    {
        return "global_uniqueness_demoted_guarded_strong_content";
    }
    if matched.normalized_anchor >= MEDIUM_SOURCED_GUARDED_STRONG_NANCHOR
        && matched.margin >= MEDIUM_SCORE_MARGIN
        && has_sourced_near_strong_support(matched.source_score)
    {
        return "global_uniqueness_demoted_sourced_guarded_strong_content";
    }
    if matched.reciprocal_best
        && matched.weighted_anchor >= MEDIUM_RECIPROCAL_WEIGHTED_ANCHOR
        && matched.normalized_anchor >= MEDIUM_RECIPROCAL_NORMALIZED_ANCHOR
    {
        return "global_uniqueness_demoted_reciprocal_anchors";
    }
    if matched.reciprocal_best
        && matched.weighted_anchor >= MEDIUM_RECIPROCAL_NEAR_WEIGHTED_ANCHOR
        && matched.normalized_anchor >= MEDIUM_RECIPROCAL_NEAR_NORMALIZED_ANCHOR
        && matched.margin >= MEDIUM_SCORE_MARGIN
        && has_sourced_reciprocal_shortfall_support(matched.source_score)
    {
        return "global_uniqueness_demoted_sourced_reciprocal_anchors";
    }
    if matched.normalized_anchor >= MEDIUM_STRONG_NANCHOR * 0.80 {
        return "near_strong_normalized_anchor";
    }
    if matched.reciprocal_best {
        if matched.weighted_anchor >= MEDIUM_RECIPROCAL_WEIGHTED_ANCHOR {
            return "reciprocal_normalized_anchor_shortfall";
        }
        if matched.normalized_anchor >= MEDIUM_RECIPROCAL_NORMALIZED_ANCHOR {
            return "reciprocal_weighted_anchor_shortfall";
        }
        return "reciprocal_anchor_shortfall";
    }
    if matched.weighted_anchor >= MEDIUM_WEIGHTED_ANCHOR
        && matched.normalized_anchor < MEDIUM_NORMALIZED_ANCHOR
    {
        return "anchor_fraction_shortfall";
    }
    if matched.normalized_anchor >= MEDIUM_NORMALIZED_ANCHOR
        && matched.weighted_anchor < MEDIUM_WEIGHTED_ANCHOR
    {
        return "anchor_mass_shortfall";
    }
    if (matched.export_overlap >= 2 || matched.function_overlap >= 2)
        && matched.normalized_anchor < MEDIUM_CONTENT_NORMALIZED_FLOOR
    {
        return "content_floor_shortfall";
    }
    if matched.structural_score >= MEDIUM_STRUCTURAL_SCORE
        && (matched.weighted_anchor < MEDIUM_STRUCTURAL_WEIGHTED_ANCHOR
            || matched.normalized_anchor < MEDIUM_STRUCTURAL_NORMALIZED_ANCHOR)
    {
        return "structural_anchor_corroboration_shortfall";
    }
    if matched.graph_support >= MEDIUM_GRAPH_SUPPORT
        && (matched.weighted_anchor < MEDIUM_STRUCTURAL_WEIGHTED_ANCHOR
            || matched.normalized_anchor < MEDIUM_STRUCTURAL_NORMALIZED_ANCHOR)
    {
        return "graph_anchor_corroboration_shortfall";
    }
    if matched.margin < MEDIUM_SCORE_MARGIN && matched.normalized_anchor < MEDIUM_STRONG_NANCHOR {
        return "ambiguous_runner_up_or_weak_content";
    }
    "insufficient_evidence"
}

fn low_medium_closeness(matched: &ModuleMatch) -> f64 {
    let anchor_pair = ratio(matched.weighted_anchor, MEDIUM_WEIGHTED_ANCHOR)
        .min(ratio(matched.normalized_anchor, MEDIUM_NORMALIZED_ANCHOR));
    let guarded_strong_pair = ratio(matched.normalized_anchor, MEDIUM_GUARDED_STRONG_NANCHOR)
        .min(ratio(matched.margin, MEDIUM_SCORE_MARGIN));
    let sourced_guarded_strong_pair = ratio(
        matched.normalized_anchor,
        MEDIUM_SOURCED_GUARDED_STRONG_NANCHOR,
    )
    .min(ratio(matched.margin, MEDIUM_SCORE_MARGIN));
    let reciprocal_pair = if matched.reciprocal_best {
        ratio(matched.weighted_anchor, MEDIUM_RECIPROCAL_WEIGHTED_ANCHOR).min(ratio(
            matched.normalized_anchor,
            MEDIUM_RECIPROCAL_NORMALIZED_ANCHOR,
        ))
    } else {
        0.0
    };
    let structural_pair = ratio(matched.structural_score, MEDIUM_STRUCTURAL_SCORE).min(
        ratio(matched.weighted_anchor, MEDIUM_STRUCTURAL_WEIGHTED_ANCHOR).min(ratio(
            matched.normalized_anchor,
            MEDIUM_STRUCTURAL_NORMALIZED_ANCHOR,
        )),
    );
    let graph_pair = ratio(matched.graph_support as f64, MEDIUM_GRAPH_SUPPORT as f64).min(
        ratio(matched.weighted_anchor, MEDIUM_STRUCTURAL_WEIGHTED_ANCHOR).min(ratio(
            matched.normalized_anchor,
            MEDIUM_STRUCTURAL_NORMALIZED_ANCHOR,
        )),
    );
    let content_floor = if matched.export_overlap >= 2 || matched.function_overlap >= 2 {
        ratio(matched.normalized_anchor, MEDIUM_CONTENT_NORMALIZED_FLOOR)
    } else {
        0.0
    };
    ratio(matched.normalized_anchor, MEDIUM_STRONG_NANCHOR)
        .max(anchor_pair)
        .max(guarded_strong_pair)
        .max(sourced_guarded_strong_pair)
        .max(reciprocal_pair)
        .max(structural_pair)
        .max(graph_pair)
        .max(content_floor)
        .min(1.0)
}

fn ratio(value: f64, threshold: f64) -> f64 {
    if threshold <= f64::EPSILON {
        0.0
    } else {
        (value / threshold).max(0.0)
    }
}

fn positive_shortfall(threshold: f64, value: f64) -> f64 {
    (threshold - value).max(0.0)
}

/// One source file from the reference tree, fingerprinted for matching.
#[derive(Debug, Clone)]
pub(crate) struct ReferenceSourceModule {
    /// Path relative to the source root, e.g. `features/audio-capture.ts`.
    pub file_path: String,
    pub source: String,
    pub fingerprint: SourceFingerprint,
    pub profile: SourceEvidenceProfile,
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
    pub evidence_idf: SourceEvidenceIdf,
    candidate_index: ReferenceCandidateIndex,
}

#[derive(Debug, Clone, Default)]
struct ReferenceCandidateIndex {
    normalized_source_hashes: BTreeMap<String, BTreeSet<usize>>,
    function_signature_hashes: BTreeMap<String, BTreeSet<usize>>,
    top_level_declaration_hashes: BTreeMap<String, BTreeSet<usize>>,
    granular_hashes: BTreeMap<String, BTreeSet<usize>>,
    string_anchors: BTreeMap<String, BTreeSet<usize>>,
    asset_literals: BTreeMap<String, BTreeSet<usize>>,
    export_names: BTreeMap<String, BTreeSet<usize>>,
    path_to_index: BTreeMap<String, usize>,
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
        let profile = build_source_evidence_profile_with_fingerprint(
            relative.as_str(),
            source.as_str(),
            fingerprint.clone(),
        );
        let (export_names, asset_literals) = classify_anchors(&fingerprint);
        modules.push(ReferenceSourceModule {
            file_path: relative,
            source,
            fingerprint,
            profile,
            export_names,
            asset_literals,
        });
    }
    let anchor_idf = compute_anchor_idf(&modules);
    let evidence_idf = source_evidence_idf(modules.iter().map(|module| &module.profile));
    let candidate_index = build_candidate_index(&modules);
    Ok(ReferenceSourceIndex {
        version: version.to_string(),
        modules,
        anchor_idf,
        evidence_idf,
        candidate_index,
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

fn build_candidate_index(modules: &[ReferenceSourceModule]) -> ReferenceCandidateIndex {
    let mut index = ReferenceCandidateIndex::default();
    for (module_index, module) in modules.iter().enumerate() {
        index
            .path_to_index
            .insert(module.file_path.clone(), module_index);
        for hash in &module.fingerprint.normalized_source_hashes {
            index
                .normalized_source_hashes
                .entry(hash.clone())
                .or_default()
                .insert(module_index);
        }
        for hash in &module.fingerprint.function_signature_hashes {
            index
                .function_signature_hashes
                .entry(hash.clone())
                .or_default()
                .insert(module_index);
        }
        for hash in &module.fingerprint.top_level_declaration_hashes {
            index
                .top_level_declaration_hashes
                .entry(hash.clone())
                .or_default()
                .insert(module_index);
        }
        for hash in granular_fingerprint_hashes(&module.fingerprint) {
            index
                .granular_hashes
                .entry(hash)
                .or_default()
                .insert(module_index);
        }
        for anchor in &module.fingerprint.string_anchors {
            index
                .string_anchors
                .entry(anchor.clone())
                .or_default()
                .insert(module_index);
        }
        for asset in &module.asset_literals {
            index
                .asset_literals
                .entry(asset.clone())
                .or_default()
                .insert(module_index);
        }
        for export in &module.export_names {
            index
                .export_names
                .entry(export.clone())
                .or_default()
                .insert(module_index);
        }
    }
    index
}

fn source_structural_support(
    subjects: &[SubjectModule],
    index: &ReferenceSourceIndex,
) -> BTreeMap<u32, BTreeMap<String, f64>> {
    let reference_by_path = index
        .modules
        .iter()
        .map(|module| (module.file_path.clone(), module))
        .collect::<BTreeMap<_, _>>();
    let mut reference_bags = BTreeMap::<String, (StructuralBag, f64)>::new();
    let mut support = BTreeMap::<u32, BTreeMap<String, f64>>::new();
    for subject in subjects {
        let fingerprints = FunctionExtractor::fingerprint_primary(
            ModuleId(subject.module_id),
            subject.source.as_str(),
        );
        let Some(subject_bag) = build_structural_bag(&fingerprints) else {
            continue;
        };
        let subject_self = score_structural_bags(&subject_bag, &subject_bag).unwrap_or(0.0);
        // Use the cheap anchor/export/hash scorer as a candidate generator,
        // then reuse the package matcher's structural-bag scorer only on the
        // short list. Full subject x reference cascade is too expensive and
        // noisy; structural scoring is evidence refinement, not discovery.
        for module in ranked_module_matches(&subject.profile, index, None, None)
            .into_iter()
            .take(SOURCE_STRUCTURAL_CANDIDATE_LIMIT)
            .filter_map(|candidate| reference_by_path.get(candidate.matched.file_path.as_str()))
        {
            if !reference_bags.contains_key(module.file_path.as_str()) {
                let fingerprints =
                    FunctionExtractor::fingerprint_primary(ModuleId(0), module.source.as_str());
                let Some(bag) = build_structural_bag(&fingerprints) else {
                    continue;
                };
                let self_score = score_structural_bags(&bag, &bag).unwrap_or(0.0);
                reference_bags.insert(module.file_path.clone(), (bag, self_score));
            }
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
                &subject.profile,
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
    let edges = build_source_graph_edges(subjects, index);

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
            &edges.subject_deps,
            &edges.subject_incoming,
            &edges.reference_deps,
            &edges.reference_incoming,
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

fn source_graph_support_for_assignment(
    subjects: &[SubjectModule],
    index: &ReferenceSourceIndex,
    assignment: &BTreeMap<u32, String>,
) -> BTreeMap<u32, BTreeMap<String, GraphEvidence>> {
    if assignment.is_empty() {
        return BTreeMap::new();
    }
    let edges = build_source_graph_edges(subjects, index);
    graph_neighborhood_support(
        &edges.subject_deps,
        &edges.subject_incoming,
        &edges.reference_deps,
        &edges.reference_incoming,
        assignment,
    )
}

fn attach_post_match_graph_support(
    module_id: u32,
    matched: &mut ModuleMatch,
    post_match_graph_support: &BTreeMap<u32, BTreeMap<String, GraphEvidence>>,
) {
    let graph = post_match_graph_support
        .get(&module_id)
        .and_then(|support| support.get(matched.file_path.as_str()))
        .copied()
        .unwrap_or_default();
    if graph.known_edges > 0 {
        matched.graph_support = graph.matched_edges;
        matched.graph_known_edges = graph.known_edges;
    }
}

#[derive(Debug, Clone, Default)]
struct SourceGraphEdges {
    subject_deps: BTreeMap<u32, BTreeSet<u32>>,
    subject_incoming: BTreeMap<u32, BTreeSet<u32>>,
    reference_deps: BTreeMap<String, BTreeSet<String>>,
    reference_incoming: BTreeMap<String, BTreeSet<String>>,
}

fn build_source_graph_edges(
    subjects: &[SubjectModule],
    index: &ReferenceSourceIndex,
) -> SourceGraphEdges {
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
            let deps = if subject.dependencies.is_empty() {
                extract_import_specifiers(subject.source.as_str())
                    .into_iter()
                    .filter_map(|specifier| {
                        resolve_relative_source_path(
                            subject.file_path.as_str(),
                            specifier.as_str(),
                            &subject_paths,
                        )
                    })
                    .filter_map(|path| subject_id_by_path.get(path.as_str()).copied())
                    .collect::<BTreeSet<_>>()
            } else {
                subject.dependencies.clone()
            };
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
    SourceGraphEdges {
        subject_deps,
        subject_incoming,
        reference_deps,
        reference_incoming,
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct GraphStructureProfile {
    in_degree: usize,
    out_degree: usize,
    neighborhood_hash: u64,
}

#[derive(Debug, Clone, Default)]
struct GraphStructureContext {
    subjects: BTreeMap<u32, GraphStructureProfile>,
    references: BTreeMap<String, GraphStructureProfile>,
}

fn source_graph_structure(
    subjects: &[SubjectModule],
    index: &ReferenceSourceIndex,
) -> GraphStructureContext {
    let edges = build_source_graph_edges(subjects, index);
    GraphStructureContext {
        subjects: graph_structure_profiles(&edges.subject_deps, &edges.subject_incoming),
        references: graph_structure_profiles(&edges.reference_deps, &edges.reference_incoming),
    }
}

fn graph_structure_profiles<T>(
    outgoing: &BTreeMap<T, BTreeSet<T>>,
    incoming: &BTreeMap<T, BTreeSet<T>>,
) -> BTreeMap<T, GraphStructureProfile>
where
    T: Ord + Clone,
{
    let mut nodes = BTreeSet::<T>::new();
    nodes.extend(outgoing.keys().cloned());
    nodes.extend(incoming.keys().cloned());
    for targets in outgoing.values() {
        nodes.extend(targets.iter().cloned());
    }
    for sources in incoming.values() {
        nodes.extend(sources.iter().cloned());
    }
    nodes
        .into_iter()
        .map(|node| {
            let out_degree = outgoing.get(&node).map_or(0, BTreeSet::len);
            let in_degree = incoming.get(&node).map_or(0, BTreeSet::len);
            let mut tokens = vec![format!(
                "self:{}",
                graph_role_signature(in_degree, out_degree)
            )];
            for target in outgoing.get(&node).into_iter().flatten() {
                tokens.push(format!(
                    "out:{}",
                    graph_role_signature(
                        incoming.get(target).map_or(0, BTreeSet::len),
                        outgoing.get(target).map_or(0, BTreeSet::len)
                    )
                ));
            }
            for source in incoming.get(&node).into_iter().flatten() {
                tokens.push(format!(
                    "in:{}",
                    graph_role_signature(
                        incoming.get(source).map_or(0, BTreeSet::len),
                        outgoing.get(source).map_or(0, BTreeSet::len)
                    )
                ));
            }
            tokens.sort();
            (
                node,
                GraphStructureProfile {
                    in_degree,
                    out_degree,
                    neighborhood_hash: stable_diagnostic_hash(tokens.join("|").as_str()),
                },
            )
        })
        .collect()
}

fn graph_structure_evidence(
    subject_id: u32,
    reference_path: &str,
    context: &GraphStructureContext,
) -> GraphStructureEvidence {
    let subject = context
        .subjects
        .get(&subject_id)
        .copied()
        .unwrap_or_default();
    let reference = context
        .references
        .get(reference_path)
        .copied()
        .unwrap_or_default();
    let subject_has_edges = subject.in_degree + subject.out_degree > 0;
    let reference_has_edges = reference.in_degree + reference.out_degree > 0;
    let comparable = subject_has_edges && reference_has_edges;
    GraphStructureEvidence {
        subject_in_degree: subject.in_degree,
        subject_out_degree: subject.out_degree,
        reference_in_degree: reference.in_degree,
        reference_out_degree: reference.out_degree,
        subject_neighborhood_hash: subject.neighborhood_hash,
        reference_neighborhood_hash: reference.neighborhood_hash,
        subject_has_edges,
        reference_has_edges,
        role_match: comparable
            && graph_role_signature(subject.in_degree, subject.out_degree)
                == graph_role_signature(reference.in_degree, reference.out_degree),
        neighborhood_hash_match: comparable
            && subject.neighborhood_hash == reference.neighborhood_hash,
    }
}

fn graph_role_signature(in_degree: usize, out_degree: usize) -> String {
    format!(
        "in:{};out:{}",
        graph_degree_bucket(in_degree),
        graph_degree_bucket(out_degree)
    )
}

fn graph_degree_bucket(degree: usize) -> &'static str {
    match degree {
        0 => "0",
        1 => "1",
        2 => "2",
        3..=4 => "3-4",
        5..=9 => "5-9",
        _ => "10+",
    }
}

fn stable_diagnostic_hash(value: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
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
/// A near-strong normalized content overlap may promote when the candidate is
/// also clearly separated from the runner-up. This is intentionally below the
/// self-sufficient strong threshold and gated by the normal Medium margin so it
/// only recovers confident near misses, not ambiguous hub collisions.
const MEDIUM_GUARDED_STRONG_NANCHOR: f64 = 0.25;
/// Slightly lower near-strong normalized content can promote only when an
/// independent source-evidence axis also corroborates the same candidate. This
/// recovers a narrow band identified by diagnostics while avoiding pure common
/// string-anchor matches.
const MEDIUM_SOURCED_GUARDED_STRONG_NANCHOR: f64 = 0.24;
/// A reciprocal-best assignment that shares a substantial mass of rare string
/// anchors is strong enough to promote even when the normalized fraction is
/// below the generic Medium floor. This recovers large first-party files whose
/// minified split module preserves many distinctive literals but only a modest
/// fraction of the full source file's anchor surface.
const MEDIUM_RECIPROCAL_WEIGHTED_ANCHOR: f64 = 20.0;
const MEDIUM_RECIPROCAL_NORMALIZED_ANCHOR: f64 = 0.10;
/// Narrow reciprocal near miss: diagnostics showed a small, high-margin band
/// just below the 20.0 weighted-anchor gate. It still requires unique string
/// evidence so common-anchor reciprocal ties do not promote.
const MEDIUM_RECIPROCAL_NEAR_WEIGHTED_ANCHOR: f64 = 18.0;
const MEDIUM_RECIPROCAL_NEAR_NORMALIZED_ANCHOR: f64 = 0.13;
const SOURCE_STRUCTURAL_CANDIDATE_LIMIT: usize = 12;
const SOURCE_CANDIDATE_MAX_ANCHOR_FANOUT: usize = 64;
const SOURCE_CANDIDATE_MAX_GRANULAR_HASH_FANOUT: usize = 96;
const SOURCE_CANDIDATE_MIN_ANCHOR_IDF: f64 = 1.0;
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
const MAX_PROPAGATION_ROUNDS: usize = 1;
/// Minimum normalized-anchor (content) corroboration required for the
/// otherwise-content-free Medium criteria — `export>=2 || function>=2` and the
/// graph-all-edges-matched promotion. Without it, coincidental function/export
/// hashes or a trivial 2-edge "all dependencies matched" forge a Medium with
/// zero content overlap (measured on 2.1.89: ~30 of 130 mediums, e.g. six
/// unrelated modules all "matching" `utils/debug.ts`, three "matching"
/// `cli/print.ts`, all at nanchor=0). Content is the true/false discriminator.
const MEDIUM_CONTENT_NORMALIZED_FLOOR: f64 = 0.05;
/// A narrow Low->Medium recovery path for the largest diagnostic bucket:
/// top candidate has meaningful source/content evidence but was demoted only
/// because the runner-up is close on aggregate relevance. This is deliberately
/// below `MEDIUM_SCORE_MARGIN` and must be accompanied by axis-specific deltas
/// over the runner-up, so WL/PQ/multigranular shape never promote by themselves.
const AMBIGUOUS_PROMOTION_MIN_MARGIN: f64 = 0.05;
const AMBIGUOUS_PROMOTION_MIN_NANCHOR: f64 = 0.03;
const AMBIGUOUS_PROMOTION_MIN_WEIGHTED_ANCHOR: f64 = 6.0;
const AMBIGUOUS_PROMOTION_NANCHOR_DELTA: f64 = 0.035;
const AMBIGUOUS_PROMOTION_WEIGHTED_DELTA: f64 = 4.0;
const AMBIGUOUS_PROMOTION_GRANULAR_DELTA: usize = 8;
const AMBIGUOUS_PROMOTION_WINDOW_DELTA: usize = 2;
const AMBIGUOUS_PROMOTION_STRUCTURAL_SCORE: f64 = 0.10;
const AMBIGUOUS_PROMOTION_STRUCTURAL_DELTA: f64 = 0.06;
const AMBIGUOUS_PROMOTION_FUNCTION_AXIS_DELTA: usize = 8;
const SPLIT_CLUSTER_MIN_MARGIN: f64 = 0.03;
const SPLIT_CLUSTER_STRUCTURAL_SCORE: f64 = 0.10;

type GraphEvidence = GraphNeighborhoodEvidence;

#[derive(Debug, Clone)]
pub(crate) struct ModuleMatch {
    pub file_path: String,
    pub tier: MatchTier,
    pub asset_overlap: usize,
    pub export_overlap: usize,
    pub function_overlap: usize,
    pub top_level_declaration_overlap: usize,
    pub import_export_surface_overlap: usize,
    pub class_member_overlap: usize,
    pub statement_window_overlap: usize,
    pub block_branch_overlap: usize,
    pub pq_gram_overlap: usize,
    pub wl_overlap: usize,
    /// Coverage of the smaller granular-hash set by the overlap. This acts as a
    /// partial/region match signal for bundle slices that correspond to only a
    /// subregion of a larger reference file.
    pub granular_hash_containment: f64,
    pub statement_window_containment: f64,
    pub block_branch_containment: f64,
    /// Aggregate normalized structural score. Full-output mode can populate it
    /// from package matcher's structural-bag scorer; all modes also compute a
    /// lightweight score from the already-extracted multi-granularity source
    /// fingerprint axes so module-only matching still has structural evidence.
    pub structural_score: f64,
    /// Count of matched outgoing/incoming neighbor edges between the subject
    /// graph and the reference graph. This is the source-side analogue of
    /// package matcher dependency-neighborhood promotion.
    pub graph_support: usize,
    /// Number of subject neighbor edges that had preliminary source matches and
    /// therefore could participate in graph-neighborhood evidence.
    pub graph_known_edges: usize,
    pub graph_structure: GraphStructureEvidence,
    pub anchor_overlap: usize,
    pub source_score: SourceEvidenceScore,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct GraphStructureEvidence {
    pub subject_in_degree: usize,
    pub subject_out_degree: usize,
    pub reference_in_degree: usize,
    pub reference_out_degree: usize,
    pub subject_neighborhood_hash: u64,
    pub reference_neighborhood_hash: u64,
    pub subject_has_edges: bool,
    pub reference_has_edges: bool,
    pub role_match: bool,
    pub neighborhood_hash_match: bool,
}

#[derive(Debug, Clone)]
struct RankedModuleMatch {
    relevance: f64,
    matched: ModuleMatch,
}

#[derive(Debug, Clone)]
struct SubjectRankedModuleMatch {
    best: RankedModuleMatch,
    runner_up: Option<RankedModuleMatch>,
}

#[derive(Debug, Clone, Copy)]
struct MatchEvidence {
    hash_match: bool,
    asset_overlap: usize,
    export_overlap: usize,
    function_overlap: usize,
    top_level_declaration_overlap: usize,
    import_export_surface_overlap: usize,
    class_member_overlap: usize,
    statement_window_overlap: usize,
    block_branch_overlap: usize,
    pq_gram_overlap: usize,
    wl_overlap: usize,
    source_score: SourceEvidenceScore,
    structural_score: f64,
    graph: GraphEvidence,
    weighted_anchor: f64,
    normalized_anchor: f64,
}

fn overlap_len(left: &BTreeSet<String>, right: &BTreeSet<String>) -> usize {
    left.intersection(right).count()
}

fn granular_fingerprint_hashes(fingerprint: &SourceFingerprint) -> BTreeSet<String> {
    fingerprint
        .import_export_surface_hashes
        .iter()
        .chain(fingerprint.class_member_hashes.iter())
        .chain(fingerprint.statement_window_hashes.iter())
        .chain(fingerprint.block_branch_hashes.iter())
        .chain(fingerprint.pq_gram_hashes.iter())
        .chain(fingerprint.wl_hashes.iter())
        .cloned()
        .collect()
}

fn granular_hash_overlap(evidence: MatchEvidence) -> usize {
    evidence.import_export_surface_overlap
        + evidence.class_member_overlap
        + evidence.statement_window_overlap
        + evidence.block_branch_overlap
        + evidence.pq_gram_overlap
        + evidence.wl_overlap
}

fn granular_match_overlap(matched: &ModuleMatch) -> usize {
    matched.import_export_surface_overlap
        + matched.class_member_overlap
        + matched.statement_window_overlap
        + matched.block_branch_overlap
        + matched.pq_gram_overlap
        + matched.wl_overlap
}

fn containment_ratio(overlap: usize, left_size: usize, right_size: usize) -> f64 {
    let denominator = left_size.min(right_size);
    if denominator == 0 {
        0.0
    } else {
        overlap as f64 / denominator as f64
    }
}

#[derive(Debug, Clone, Copy)]
struct StructuralAxisOverlap {
    overlap: usize,
    subject_len: usize,
    reference_len: usize,
    weight: f64,
}

fn normalized_weighted_axis_overlap(axes: &[StructuralAxisOverlap]) -> f64 {
    let overlap = axes
        .iter()
        .map(|axis| axis.overlap as f64 * axis.weight)
        .sum::<f64>();
    let subject_mass = axes
        .iter()
        .map(|axis| axis.subject_len as f64 * axis.weight)
        .sum::<f64>();
    let reference_mass = axes
        .iter()
        .map(|axis| axis.reference_len as f64 * axis.weight)
        .sum::<f64>();
    if subject_mass <= f64::EPSILON || reference_mass <= f64::EPSILON {
        0.0
    } else {
        (overlap / (subject_mass * reference_mass).sqrt()).clamp(0.0, 1.0)
    }
}

fn source_fingerprint_structural_score(
    subject: &SourceFingerprint,
    reference: &SourceFingerprint,
    evidence: MatchEvidence,
) -> f64 {
    normalized_weighted_axis_overlap(&[
        StructuralAxisOverlap {
            overlap: evidence.top_level_declaration_overlap,
            subject_len: subject.top_level_declaration_hashes.len(),
            reference_len: reference.top_level_declaration_hashes.len(),
            weight: 20.0,
        },
        StructuralAxisOverlap {
            overlap: evidence.import_export_surface_overlap,
            subject_len: subject.import_export_surface_hashes.len(),
            reference_len: reference.import_export_surface_hashes.len(),
            weight: 18.0,
        },
        StructuralAxisOverlap {
            overlap: evidence.class_member_overlap,
            subject_len: subject.class_member_hashes.len(),
            reference_len: reference.class_member_hashes.len(),
            weight: 18.0,
        },
        StructuralAxisOverlap {
            overlap: evidence.statement_window_overlap,
            subject_len: subject.statement_window_hashes.len(),
            reference_len: reference.statement_window_hashes.len(),
            weight: 10.0,
        },
        StructuralAxisOverlap {
            overlap: evidence.block_branch_overlap,
            subject_len: subject.block_branch_hashes.len(),
            reference_len: reference.block_branch_hashes.len(),
            weight: 9.0,
        },
        StructuralAxisOverlap {
            overlap: evidence.pq_gram_overlap,
            subject_len: subject.pq_gram_hashes.len(),
            reference_len: reference.pq_gram_hashes.len(),
            weight: 6.0,
        },
        StructuralAxisOverlap {
            overlap: evidence.wl_overlap,
            subject_len: subject.wl_hashes.len(),
            reference_len: reference.wl_hashes.len(),
            weight: 5.0,
        },
    ])
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
    // Hub-resistant signals rank candidates on their own: content anchors
    // (IDF-weighted / normalized) and Jaccard similarities (union-normalized, so
    // a large reference file does NOT score high against a small module).
    (if evidence.hash_match { 1.0e9 } else { 0.0 })
        + (evidence.asset_overlap as f64) * 1.0e6
        + evidence.normalized_anchor * 150.0
        + evidence.weighted_anchor
        + (evidence.export_overlap as f64) * 8.0
        + (evidence.function_overlap as f64) * 4.0
        + evidence.source_score.function_axis_jaccard * 120.0
        + evidence.source_score.jsx_react_shape_jaccard * 80.0
        + evidence.source_score.anchor_cooccurrence_jaccard * 70.0
        + (evidence.graph.matched_edges as f64) * 35.0
        + evidence.graph.coverage() * 75.0
        + corroborated_structural_relevance(evidence)
}

/// Raw-count and containment structural/shape overlaps reward reference-file
/// MAGNITUDE: a large hub file (`services/mcp/client.ts`, `screens/REPL.tsx`)
/// shares many declarations / members / grams with any small module by chance,
/// and `function_axis_containment` is ~1.0 whenever a small module's generic
/// functions are a subset of a big file. Added un-gated, these let hubs out-rank
/// and DISPLACE genuine content matches (observed: 161 of 166 lost modules were
/// displaced onto a few hub files). So they count only when corroborated by real
/// content (anchors), the dependency graph, or a unique string anchor — the same
/// "structural is corroborating, not standalone" rule the pre-refactor matcher
/// documented and relied on. Jaccard similarities (above) stay standalone
/// because union normalization already makes them hub-resistant.
fn corroborated_structural_relevance(evidence: MatchEvidence) -> f64 {
    let corroborated = evidence.weighted_anchor >= MEDIUM_STRUCTURAL_WEIGHTED_ANCHOR
        || evidence.normalized_anchor >= MEDIUM_STRUCTURAL_NORMALIZED_ANCHOR
        || evidence.source_score.unique_string_anchor_overlap >= 1
        || evidence.graph.matched_edges >= 1;
    if !corroborated {
        return 0.0;
    }
    (evidence.top_level_declaration_overlap as f64) * 18.0
        + (evidence.import_export_surface_overlap as f64) * 16.0
        + (evidence.class_member_overlap as f64) * 18.0
        + (evidence.statement_window_overlap as f64) * 10.0
        + (evidence.block_branch_overlap as f64) * 8.0
        + (evidence.pq_gram_overlap as f64) * 6.0
        + (evidence.wl_overlap as f64) * 5.0
        + evidence.source_score.function_axis_containment * 60.0
        + (evidence.source_score.jsx_react_shape_overlap as f64) * 8.0
        + (evidence.source_score.anchor_cooccurrence_overlap as f64) * 4.0
        + evidence.structural_score * 60.0
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
        || (evidence.source_score.function_axis_jaccard >= 0.20
            && evidence.source_score.function_axis_containment >= 0.50
            && (evidence.normalized_anchor >= 0.03
                || evidence.source_score.unique_string_anchor_overlap >= 1
                || evidence.source_score.jsx_react_shape_jaccard >= 0.15))
        || (evidence.source_score.jsx_react_shape_jaccard >= 0.35
            && (evidence.normalized_anchor >= 0.08
                || evidence.source_score.unique_string_anchor_overlap >= 1
                || evidence.source_score.function_axis_containment >= 0.35))
        || (evidence.source_score.anchor_cooccurrence_jaccard >= 0.18
            && evidence.source_score.anchor_cooccurrence_overlap >= 3
            && (evidence.normalized_anchor >= 0.03
                || evidence.source_score.unique_string_anchor_overlap >= 1
                || evidence.source_score.function_axis_containment >= 0.25
                || evidence.source_score.jsx_react_shape_jaccard >= 0.15))
        || (evidence.top_level_declaration_overlap >= 2
            && (evidence.normalized_anchor >= 0.03
                || evidence.source_score.unique_string_anchor_overlap >= 1
                || evidence.source_score.function_axis_containment >= 0.25))
        || (evidence.top_level_declaration_overlap >= 1
            && evidence.normalized_anchor >= MEDIUM_CONTENT_NORMALIZED_FLOOR
            && evidence.source_score.function_axis_containment >= 0.35)
        || (evidence.wl_overlap >= 6
            && (evidence.normalized_anchor >= 0.03
                || evidence.source_score.unique_string_anchor_overlap >= 1
                || evidence.source_score.function_axis_containment >= 0.25
                || evidence.pq_gram_overlap >= 6
                || evidence.statement_window_overlap >= 2
                || evidence.class_member_overlap >= 1))
        || (evidence.pq_gram_overlap >= 6
            && (evidence.normalized_anchor >= 0.03
                || evidence.source_score.unique_string_anchor_overlap >= 1
                || evidence.source_score.function_axis_containment >= 0.25
                || evidence.top_level_declaration_overlap >= 1
                || evidence.class_member_overlap >= 1))
        || (granular_hash_overlap(evidence) >= 4
            && (evidence.normalized_anchor >= 0.03
                || evidence.source_score.unique_string_anchor_overlap >= 1
                || evidence.source_score.function_axis_containment >= 0.30))
        || (evidence.import_export_surface_overlap >= 1
            && (evidence.export_overlap >= 1 || evidence.normalized_anchor >= 0.08))
        || (evidence.class_member_overlap >= 1
            && evidence.statement_window_overlap >= 1
            && evidence.normalized_anchor >= 0.04)
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
    source_score: SourceEvidenceScore,
    weighted_anchor: f64,
    normalized_anchor: f64,
) -> MatchTier {
    match tier {
        MatchTier::High => tier,
        MatchTier::Low
            if reciprocal_best
                && weighted_anchor >= MEDIUM_RECIPROCAL_WEIGHTED_ANCHOR
                && normalized_anchor >= MEDIUM_RECIPROCAL_NORMALIZED_ANCHOR =>
        {
            MatchTier::Medium
        }
        MatchTier::Low
            if reciprocal_best
                && weighted_anchor >= MEDIUM_RECIPROCAL_NEAR_WEIGHTED_ANCHOR
                && normalized_anchor >= MEDIUM_RECIPROCAL_NEAR_NORMALIZED_ANCHOR
                && margin >= MEDIUM_SCORE_MARGIN
                && has_sourced_reciprocal_shortfall_support(source_score) =>
        {
            MatchTier::Medium
        }
        MatchTier::Low
            if normalized_anchor >= MEDIUM_GUARDED_STRONG_NANCHOR
                && margin >= MEDIUM_SCORE_MARGIN =>
        {
            MatchTier::Medium
        }
        MatchTier::Low
            if normalized_anchor >= MEDIUM_SOURCED_GUARDED_STRONG_NANCHOR
                && margin >= MEDIUM_SCORE_MARGIN
                && has_sourced_near_strong_support(source_score) =>
        {
            MatchTier::Medium
        }
        MatchTier::Low => tier,
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

fn guarded_ambiguous_promotion(top: &ModuleMatch, runner_up: &ModuleMatch) -> bool {
    if top.margin < AMBIGUOUS_PROMOTION_MIN_MARGIN {
        return false;
    }
    if !has_ambiguous_promotion_content(top) {
        return false;
    }
    if has_clear_anchor_delta(top, runner_up) {
        return true;
    }
    if has_clear_source_axis_delta(top, runner_up) {
        return true;
    }
    if has_clear_granular_delta(top, runner_up) {
        return true;
    }
    if has_clear_region_containment_delta(top, runner_up) {
        return true;
    }
    if has_clear_structural_delta(top, runner_up) {
        return true;
    }
    if has_clear_function_axis_delta(top, runner_up) {
        return true;
    }
    has_clear_graph_delta(top, runner_up)
}

fn guarded_graph_placement_promotion(matched: &ModuleMatch) -> bool {
    if matched.graph_support == 0 || matched.graph_known_edges == 0 {
        return false;
    }
    let anchored_graph_match = matched.graph_support >= MEDIUM_GRAPH_SUPPORT
        && matched.weighted_anchor >= MEDIUM_STRUCTURAL_WEIGHTED_ANCHOR
        && matched.normalized_anchor >= MEDIUM_STRUCTURAL_NORMALIZED_ANCHOR;
    let strong_graph_match = matched.graph_support >= 3
        && matched.graph_support * 5 >= matched.graph_known_edges * 3
        && has_ambiguous_promotion_content(matched);
    let complete_small_neighborhood = matched.graph_known_edges >= 2
        && matched.graph_support == matched.graph_known_edges
        && matched.normalized_anchor >= MEDIUM_CONTENT_NORMALIZED_FLOOR;
    let complete_tiny_neighborhood = matched.graph_known_edges <= 2
        && matched.graph_support == matched.graph_known_edges
        && matched.weighted_anchor >= 6.0
        && matched.normalized_anchor >= 0.10
        && matched.margin >= AMBIGUOUS_PROMOTION_MIN_MARGIN;
    let graph_with_granular_context = matched.graph_support >= 2
        && matched_neighbor_ratio(matched) >= 0.25
        && matched.weighted_anchor >= 12.0
        && matched.normalized_anchor >= 0.04
        && granular_match_overlap(matched) >= 12
        && matched.margin >= AMBIGUOUS_PROMOTION_MIN_MARGIN;
    let high_ratio_graph_content = matched.graph_support >= 3
        && matched_neighbor_ratio(matched) >= 0.40
        && matched.weighted_anchor >= 6.0
        && matched.normalized_anchor >= MEDIUM_CONTENT_NORMALIZED_FLOOR
        && matched.margin >= AMBIGUOUS_PROMOTION_MIN_MARGIN;
    anchored_graph_match
        || strong_graph_match
        || complete_small_neighborhood
        || complete_tiny_neighborhood
        || graph_with_granular_context
        || high_ratio_graph_content
}

fn has_ambiguous_promotion_content(matched: &ModuleMatch) -> bool {
    matched.normalized_anchor >= AMBIGUOUS_PROMOTION_MIN_NANCHOR
        || matched.weighted_anchor >= AMBIGUOUS_PROMOTION_MIN_WEIGHTED_ANCHOR
        || matched.source_score.unique_string_anchor_overlap >= 1
        || (matched.source_score.anchor_cooccurrence_overlap >= 3
            && matched.source_score.anchor_cooccurrence_jaccard >= 0.12)
        || (matched.source_score.jsx_react_shape_overlap >= 3
            && matched.source_score.jsx_react_shape_jaccard >= 0.12)
        || (matched.source_score.function_axis_overlap >= 4
            && matched.source_score.function_axis_containment >= 0.25)
}

fn has_high_unique_anchor_mass(matched: &ModuleMatch) -> bool {
    has_high_unique_anchor_mass_values(
        matched.source_score.unique_string_anchor_overlap,
        matched.weighted_anchor,
        matched.normalized_anchor,
        matched.margin,
    )
}

fn has_high_unique_anchor_mass_values(
    unique_string_anchor_overlap: usize,
    weighted_anchor: f64,
    normalized_anchor: f64,
    margin: f64,
) -> bool {
    (unique_string_anchor_overlap >= 5
        && weighted_anchor >= 50.0
        && normalized_anchor >= 0.18
        && margin >= MEDIUM_SCORE_MARGIN)
        || (unique_string_anchor_overlap >= 5
            && weighted_anchor >= 80.0
            && normalized_anchor >= 0.12
            && margin >= MEDIUM_SCORE_MARGIN)
}

fn has_high_cooccurrence_source_mass(matched: &ModuleMatch) -> bool {
    matched.source_score.anchor_cooccurrence_overlap >= 8
        && matched.source_score.anchor_cooccurrence_jaccard >= 0.03
        && matched.margin >= MEDIUM_SCORE_MARGIN
        && (matched.normalized_anchor >= 0.12
            || matched.weighted_anchor >= MEDIUM_RECIPROCAL_WEIGHTED_ANCHOR
            || matched.source_score.unique_string_anchor_overlap >= 1)
}

fn has_clear_anchor_delta(top: &ModuleMatch, runner_up: &ModuleMatch) -> bool {
    top.normalized_anchor >= runner_up.normalized_anchor + AMBIGUOUS_PROMOTION_NANCHOR_DELTA
        && top.weighted_anchor >= runner_up.weighted_anchor + AMBIGUOUS_PROMOTION_WEIGHTED_DELTA
}

fn has_clear_source_axis_delta(top: &ModuleMatch, runner_up: &ModuleMatch) -> bool {
    let unique_delta = top
        .source_score
        .unique_string_anchor_overlap
        .saturating_sub(runner_up.source_score.unique_string_anchor_overlap);
    let function_delta = top
        .source_score
        .function_axis_overlap
        .saturating_sub(runner_up.source_score.function_axis_overlap);
    let jsx_delta = top
        .source_score
        .jsx_react_shape_overlap
        .saturating_sub(runner_up.source_score.jsx_react_shape_overlap);
    let cooccurrence_delta = top
        .source_score
        .anchor_cooccurrence_overlap
        .saturating_sub(runner_up.source_score.anchor_cooccurrence_overlap);
    (unique_delta >= 1
        && (function_delta >= 4
            || top.source_score.function_axis_jaccard
                >= runner_up.source_score.function_axis_jaccard + 0.05
            || positive_metric_delta(top, runner_up, |matched| matched.statement_window_overlap)
                >= AMBIGUOUS_PROMOTION_WINDOW_DELTA
            || positive_metric_delta(top, runner_up, |matched| matched.block_branch_overlap)
                >= AMBIGUOUS_PROMOTION_WINDOW_DELTA))
        || (jsx_delta >= 3
            && top.source_score.jsx_react_shape_jaccard
                >= runner_up.source_score.jsx_react_shape_jaccard + 0.05
            && (unique_delta >= 1 || top.source_score.function_axis_containment >= 0.20))
        || (cooccurrence_delta >= 3
            && top.source_score.anchor_cooccurrence_jaccard
                >= runner_up.source_score.anchor_cooccurrence_jaccard + 0.05
            && (unique_delta >= 1
                || top.normalized_anchor >= AMBIGUOUS_PROMOTION_MIN_NANCHOR
                || top.source_score.function_axis_containment >= 0.20))
}

fn has_clear_granular_delta(top: &ModuleMatch, runner_up: &ModuleMatch) -> bool {
    let granular_delta =
        granular_match_overlap(top).saturating_sub(granular_match_overlap(runner_up));
    let statement_delta =
        positive_metric_delta(top, runner_up, |matched| matched.statement_window_overlap);
    let block_delta = positive_metric_delta(top, runner_up, |matched| matched.block_branch_overlap);
    let wl_delta = positive_metric_delta(top, runner_up, |matched| matched.wl_overlap);
    granular_delta >= AMBIGUOUS_PROMOTION_GRANULAR_DELTA
        && (statement_delta >= AMBIGUOUS_PROMOTION_WINDOW_DELTA
            || block_delta >= AMBIGUOUS_PROMOTION_WINDOW_DELTA
            || wl_delta >= 3)
        && (top.normalized_anchor >= AMBIGUOUS_PROMOTION_MIN_NANCHOR
            || top.source_score.unique_string_anchor_overlap >= 1
            || top.source_score.function_axis_containment >= 0.25)
}

fn has_clear_region_containment_delta(top: &ModuleMatch, runner_up: &ModuleMatch) -> bool {
    top.granular_hash_containment >= 0.65
        && top.statement_window_containment >= 0.30
        && top.block_branch_containment >= 0.30
        && top.granular_hash_containment >= runner_up.granular_hash_containment + 0.20
        && (top.statement_window_containment >= runner_up.statement_window_containment + 0.15
            || top.block_branch_containment >= runner_up.block_branch_containment + 0.15)
        && (top.normalized_anchor >= AMBIGUOUS_PROMOTION_MIN_NANCHOR
            || top.weighted_anchor >= AMBIGUOUS_PROMOTION_MIN_WEIGHTED_ANCHOR
            || top.source_score.unique_string_anchor_overlap >= 1
            || top.graph_support >= 1)
}

fn has_clear_structural_delta(top: &ModuleMatch, runner_up: &ModuleMatch) -> bool {
    top.structural_score >= AMBIGUOUS_PROMOTION_STRUCTURAL_SCORE
        && top.structural_score >= runner_up.structural_score + AMBIGUOUS_PROMOTION_STRUCTURAL_DELTA
        && (top.statement_window_overlap >= 2
            || top.block_branch_overlap >= 2
            || top.top_level_declaration_overlap >= 1
            || top.import_export_surface_overlap >= 1
            || top.class_member_overlap >= 1)
        && (top.normalized_anchor >= AMBIGUOUS_PROMOTION_MIN_NANCHOR
            || top.weighted_anchor >= AMBIGUOUS_PROMOTION_MIN_WEIGHTED_ANCHOR
            || top.source_score.unique_string_anchor_overlap >= 1
            || (top.source_score.function_axis_overlap >= 4
                && top.source_score.function_axis_containment >= 0.25)
            || top.graph_support >= 1)
}

fn has_clear_function_axis_delta(top: &ModuleMatch, runner_up: &ModuleMatch) -> bool {
    let function_delta = top
        .source_score
        .function_axis_overlap
        .saturating_sub(runner_up.source_score.function_axis_overlap);
    function_delta >= AMBIGUOUS_PROMOTION_FUNCTION_AXIS_DELTA
        && top.source_score.function_axis_containment >= 0.30
        && top.source_score.function_axis_jaccard
            >= runner_up.source_score.function_axis_jaccard + 0.03
        && (top.structural_score >= 0.08
            || top.graph_support >= 1
            || top.normalized_anchor >= AMBIGUOUS_PROMOTION_MIN_NANCHOR
            || top.weighted_anchor >= AMBIGUOUS_PROMOTION_MIN_WEIGHTED_ANCHOR
            || top.source_score.unique_string_anchor_overlap >= 1
            || top.source_score.anchor_cooccurrence_overlap >= 3)
}

fn has_clear_graph_delta(top: &ModuleMatch, runner_up: &ModuleMatch) -> bool {
    if top.graph_known_edges == 0 {
        return false;
    }
    let support_delta = top.graph_support.saturating_sub(runner_up.graph_support);
    let ratio_delta = matched_neighbor_ratio(top) - matched_neighbor_ratio(runner_up);
    support_delta >= 1
        && ratio_delta >= 0.25
        && (top.normalized_anchor >= AMBIGUOUS_PROMOTION_MIN_NANCHOR
            || top.source_score.unique_string_anchor_overlap >= 1
            || top.weighted_anchor >= MEDIUM_STRUCTURAL_WEIGHTED_ANCHOR)
}

fn positive_metric_delta(
    top: &ModuleMatch,
    runner_up: &ModuleMatch,
    metric: impl Fn(&ModuleMatch) -> usize,
) -> usize {
    metric(top).saturating_sub(metric(runner_up))
}

fn has_sourced_near_strong_support(score: SourceEvidenceScore) -> bool {
    score.unique_string_anchor_overlap >= 1
        || (score.function_axis_overlap >= 4 && score.function_axis_containment >= 0.40)
        || (score.jsx_react_shape_overlap >= 3 && score.jsx_react_shape_jaccard >= 0.15)
        || (score.anchor_cooccurrence_overlap >= 3 && score.anchor_cooccurrence_jaccard >= 0.15)
}

fn has_sourced_reciprocal_shortfall_support(score: SourceEvidenceScore) -> bool {
    score.unique_string_anchor_overlap >= 1
}

fn ranked_module_matches(
    subject: &SourceEvidenceProfile,
    index: &ReferenceSourceIndex,
    structural_support: Option<&BTreeMap<String, f64>>,
    graph_support: Option<&BTreeMap<String, GraphEvidence>>,
) -> Vec<RankedModuleMatch> {
    let fingerprint = &subject.fingerprint;
    let (subject_exports, subject_assets) = classify_anchors(fingerprint);
    let candidate_indices = candidate_module_indices(
        fingerprint,
        &subject_exports,
        &subject_assets,
        index,
        structural_support,
        graph_support,
    );
    let mut ranked = Vec::new();
    for module_index in candidate_indices {
        let Some(module) = index.modules.get(module_index) else {
            continue;
        };
        let asset_overlap = overlap_len(&subject_assets, &module.asset_literals);
        let export_overlap = overlap_len(&subject_exports, &module.export_names);
        let function_overlap = overlap_len(
            &fingerprint.function_signature_hashes,
            &module.fingerprint.function_signature_hashes,
        );
        let top_level_declaration_overlap = overlap_len(
            &fingerprint.top_level_declaration_hashes,
            &module.fingerprint.top_level_declaration_hashes,
        );
        let import_export_surface_overlap = overlap_len(
            &fingerprint.import_export_surface_hashes,
            &module.fingerprint.import_export_surface_hashes,
        );
        let class_member_overlap = overlap_len(
            &fingerprint.class_member_hashes,
            &module.fingerprint.class_member_hashes,
        );
        let statement_window_overlap = overlap_len(
            &fingerprint.statement_window_hashes,
            &module.fingerprint.statement_window_hashes,
        );
        let block_branch_overlap = overlap_len(
            &fingerprint.block_branch_hashes,
            &module.fingerprint.block_branch_hashes,
        );
        let pq_gram_overlap = overlap_len(
            &fingerprint.pq_gram_hashes,
            &module.fingerprint.pq_gram_hashes,
        );
        let wl_overlap = overlap_len(&fingerprint.wl_hashes, &module.fingerprint.wl_hashes);
        let granular_hash_containment = containment_ratio(
            import_export_surface_overlap
                + class_member_overlap
                + statement_window_overlap
                + block_branch_overlap
                + pq_gram_overlap
                + wl_overlap,
            fingerprint.import_export_surface_hashes.len()
                + fingerprint.class_member_hashes.len()
                + fingerprint.statement_window_hashes.len()
                + fingerprint.block_branch_hashes.len()
                + fingerprint.pq_gram_hashes.len()
                + fingerprint.wl_hashes.len(),
            module.fingerprint.import_export_surface_hashes.len()
                + module.fingerprint.class_member_hashes.len()
                + module.fingerprint.statement_window_hashes.len()
                + module.fingerprint.block_branch_hashes.len()
                + module.fingerprint.pq_gram_hashes.len()
                + module.fingerprint.wl_hashes.len(),
        );
        let statement_window_containment = containment_ratio(
            statement_window_overlap,
            fingerprint.statement_window_hashes.len(),
            module.fingerprint.statement_window_hashes.len(),
        );
        let block_branch_containment = containment_ratio(
            block_branch_overlap,
            fingerprint.block_branch_hashes.len(),
            module.fingerprint.block_branch_hashes.len(),
        );
        let anchor_overlap = overlap_len(
            &fingerprint.string_anchors,
            &module.fingerprint.string_anchors,
        );
        let structural_bag_score = structural_support
            .and_then(|support| support.get(module.file_path.as_str()).copied())
            .unwrap_or(0.0);
        let graph = graph_support
            .and_then(|support| support.get(module.file_path.as_str()).copied())
            .unwrap_or_default();
        let weighted_anchor = weighted_anchor_overlap(
            &fingerprint.string_anchors,
            &module.fingerprint.string_anchors,
            &index.anchor_idf,
        );
        let normalized_anchor = normalized_anchor_overlap(
            &fingerprint.string_anchors,
            &module.fingerprint.string_anchors,
            &index.anchor_idf,
            weighted_anchor,
        );
        let hash_match = !fingerprint
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
            && top_level_declaration_overlap == 0
            && import_export_surface_overlap == 0
            && class_member_overlap == 0
            && statement_window_overlap == 0
            && block_branch_overlap == 0
            && structural_bag_score < MEDIUM_STRUCTURAL_SCORE
            && graph.matched_edges == 0
            && weighted_anchor < 1.0
        {
            continue;
        }
        let source_score = score_source_evidence(subject, &module.profile, &index.evidence_idf);
        let mut evidence = MatchEvidence {
            hash_match,
            asset_overlap,
            export_overlap,
            function_overlap,
            top_level_declaration_overlap,
            import_export_surface_overlap,
            class_member_overlap,
            statement_window_overlap,
            block_branch_overlap,
            pq_gram_overlap,
            wl_overlap,
            source_score,
            structural_score: structural_bag_score,
            graph,
            weighted_anchor,
            normalized_anchor,
        };
        evidence.structural_score = structural_bag_score.max(source_fingerprint_structural_score(
            fingerprint,
            &module.fingerprint,
            evidence,
        ));
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
                top_level_declaration_overlap,
                import_export_surface_overlap,
                class_member_overlap,
                statement_window_overlap,
                block_branch_overlap,
                pq_gram_overlap,
                wl_overlap,
                granular_hash_containment,
                statement_window_containment,
                block_branch_containment,
                structural_score: evidence.structural_score,
                graph_support: graph.matched_edges,
                graph_known_edges: graph.known_edges,
                graph_structure: GraphStructureEvidence::default(),
                anchor_overlap,
                source_score,
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

fn candidate_module_indices(
    subject: &SourceFingerprint,
    subject_exports: &BTreeSet<String>,
    subject_assets: &BTreeSet<String>,
    index: &ReferenceSourceIndex,
    structural_support: Option<&BTreeMap<String, f64>>,
    graph_support: Option<&BTreeMap<String, GraphEvidence>>,
) -> BTreeSet<usize> {
    let mut candidates = BTreeSet::new();
    extend_candidates(
        &mut candidates,
        &index.candidate_index.normalized_source_hashes,
        &subject.normalized_source_hashes,
    );
    extend_candidates(
        &mut candidates,
        &index.candidate_index.function_signature_hashes,
        &subject.function_signature_hashes,
    );
    extend_candidates_with_fanout_limit(
        &mut candidates,
        &index.candidate_index.top_level_declaration_hashes,
        &subject.top_level_declaration_hashes,
        SOURCE_CANDIDATE_MAX_GRANULAR_HASH_FANOUT,
    );
    extend_candidates_with_fanout_limit(
        &mut candidates,
        &index.candidate_index.granular_hashes,
        &granular_fingerprint_hashes(subject),
        SOURCE_CANDIDATE_MAX_GRANULAR_HASH_FANOUT,
    );
    extend_candidates(
        &mut candidates,
        &index.candidate_index.asset_literals,
        subject_assets,
    );
    extend_candidates(
        &mut candidates,
        &index.candidate_index.export_names,
        subject_exports,
    );
    for anchor in &subject.string_anchors {
        let Some(posting) = index.candidate_index.string_anchors.get(anchor) else {
            continue;
        };
        if posting.len() > SOURCE_CANDIDATE_MAX_ANCHOR_FANOUT {
            continue;
        }
        if index.anchor_idf.get(anchor).copied().unwrap_or(0.0) < SOURCE_CANDIDATE_MIN_ANCHOR_IDF {
            continue;
        }
        candidates.extend(posting.iter().copied());
    }
    if let Some(support) = structural_support {
        extend_supported_paths(&mut candidates, &index.candidate_index, support.keys());
    }
    if let Some(support) = graph_support {
        extend_supported_paths(&mut candidates, &index.candidate_index, support.keys());
    }
    candidates
}

fn extend_candidates(
    candidates: &mut BTreeSet<usize>,
    postings: &BTreeMap<String, BTreeSet<usize>>,
    keys: &BTreeSet<String>,
) {
    for key in keys {
        if let Some(indices) = postings.get(key) {
            candidates.extend(indices.iter().copied());
        }
    }
}

fn extend_candidates_with_fanout_limit(
    candidates: &mut BTreeSet<usize>,
    postings: &BTreeMap<String, BTreeSet<usize>>,
    keys: &BTreeSet<String>,
    max_fanout: usize,
) {
    for key in keys {
        let Some(indices) = postings.get(key) else {
            continue;
        };
        if indices.len() > max_fanout {
            continue;
        }
        candidates.extend(indices.iter().copied());
    }
}

fn extend_supported_paths<'a>(
    candidates: &mut BTreeSet<usize>,
    candidate_index: &ReferenceCandidateIndex,
    paths: impl Iterator<Item = &'a String>,
) {
    for path in paths {
        if let Some(module_index) = candidate_index.path_to_index.get(path.as_str()) {
            candidates.insert(*module_index);
        }
    }
}

#[cfg(test)]
pub(crate) fn best_module_match(
    subject: &SourceFingerprint,
    index: &ReferenceSourceIndex,
) -> Option<ModuleMatch> {
    let profile = SourceEvidenceProfile {
        path: "<test>".to_string(),
        fingerprint: subject.clone(),
        function_axis_anchors: BTreeSet::new(),
        jsx_react_shape_anchors: BTreeSet::new(),
        anchor_cooccurrence_anchors: BTreeSet::new(),
    };
    let ranked = ranked_module_matches(&profile, index, None, None);
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
        best.source_score,
        best.weighted_anchor,
        best.normalized_anchor,
    );
    Some(best)
}

fn best_module_match_with_reciprocal(
    subject_id: u32,
    subject: &SourceEvidenceProfile,
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
        matched.source_score,
        matched.weighted_anchor,
        matched.normalized_anchor,
    );
    Some(matched)
}

fn best_ranked_by_subject(
    subjects: &[SubjectModule],
    index: &ReferenceSourceIndex,
    structural_support_by_subject: &BTreeMap<u32, BTreeMap<String, f64>>,
    graph_support_by_subject: &BTreeMap<u32, BTreeMap<String, GraphEvidence>>,
) -> BTreeMap<u32, SubjectRankedModuleMatch> {
    subjects
        .iter()
        .filter_map(|subject| {
            let ranked = ranked_module_matches(
                &subject.profile,
                index,
                structural_support_by_subject.get(&subject.module_id),
                graph_support_by_subject.get(&subject.module_id),
            );
            let mut best = ranked.first()?.clone();
            best.matched.margin = match (ranked.first(), ranked.get(1)) {
                (Some(top), Some(runner_up)) if top.relevance > f64::EPSILON => {
                    (top.relevance - runner_up.relevance).max(0.0) / top.relevance
                }
                (Some(_), None) => 1.0,
                _ => 0.0,
            };
            Some((
                subject.module_id,
                SubjectRankedModuleMatch {
                    best,
                    runner_up: ranked.get(1).cloned(),
                },
            ))
        })
        .collect()
}

fn best_subject_by_reference_matches(
    subject_best_matches: &BTreeMap<u32, SubjectRankedModuleMatch>,
) -> BTreeMap<String, u32> {
    let mut best = BTreeMap::<String, (f64, u32)>::new();
    for (module_id, candidate) in subject_best_matches {
        best.entry(candidate.best.matched.file_path.clone())
            .and_modify(|current| {
                if candidate.best.relevance > current.0
                    || (candidate.best.relevance == current.0 && *module_id < current.1)
                {
                    *current = (candidate.best.relevance, *module_id);
                }
            })
            .or_insert((candidate.best.relevance, *module_id));
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
        // ast_hash == 0 marks a symbol-propagation row (named by positional
        // alignment inside matched functions); `score` is the supporting vote
        // count. Otherwise it's a direct function-hash match.
        let tier = match (self.ast_hash == 0, self.accepted) {
            (true, true) => "symbol-propagation",
            (true, false) => "symbol-propagation-proposal",
            (false, true) => "fn-hash-unique",
            (false, false) => "fn-hash-proposal",
        };
        if self.accepted {
            format!(
                "{{\"tier\":\"{tier}\",\"ast\":\"{:016x}\",\"params\":{},\"stmts\":{},\"votes\":{:.0}}}",
                self.ast_hash, self.param_count, self.statement_count, self.score
            )
        } else {
            format!(
                "{{\"tier\":\"{tier}\",\"ast\":\"{:016x}\",\"params\":{},\"stmts\":{},\"score\":{:.1}}}",
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
    if sanitize_identifier(name) != name {
        return false;
    }
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
    // Decompiler path-derived placeholders for never-named symbols, e.g.
    // `app_bootstrap_agent_config_bX5`, `init_install_command_wrapper_MK7`,
    // `app_runtime_environment_detection_DQ_1177`. When the reference tree is
    // itself a reconstruction, these synthetic names pass the generic-word
    // filter but carry no meaning — propagating them is pure noise.
    if is_synthetic_path_derived_name(name) {
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

/// Whether a short token looks like an esbuild-minified identifier (`bX5`, `MK7`,
/// `DQ`, `al2`, `eQ3`): 2–4 alphanumerics that mix case or include a digit, or a
/// short all-caps run. Pure short lowercase words (`api`, `cli`) are NOT minified.
fn looks_like_minified_token(token: &str) -> bool {
    let len = token.chars().count();
    if !(2..=4).contains(&len) || !token.chars().all(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    let has_upper = token.chars().any(|c| c.is_ascii_uppercase());
    let has_lower = token.chars().any(|c| c.is_ascii_lowercase());
    let has_digit = token.chars().any(|c| c.is_ascii_digit());
    // Any uppercase (mixed-case or all-caps run), or a digit alongside a
    // lowercase letter — both are minified-identifier shapes, never plain words.
    has_upper || (has_digit && has_lower)
}

/// Whether a name is a decompiler path-derived placeholder for a never-named
/// symbol: lowercase path words joined by `_` with a trailing minified-token
/// segment (optionally followed by a numeric disambiguator), e.g.
/// `app_bootstrap_agent_config_bX5`, `app_runtime_environment_detection_DQ_1177`.
///
/// Real `UPPER_SNAKE` constants (`ALGORITHM_IDENTIFIER_V4A`) and camelCase names
/// keep their case in the path words, so the all-lowercase-prefix requirement
/// leaves them untouched.
fn is_synthetic_path_derived_name(name: &str) -> bool {
    let mut segments: Vec<&str> = name.split('_').filter(|s| !s.is_empty()).collect();
    // Strip a trailing pure-digit disambiguator (`..._DQ_1177`).
    if segments.len() >= 2
        && segments
            .last()
            .is_some_and(|s| s.chars().all(|c| c.is_ascii_digit()))
    {
        segments.pop();
    }
    if segments.len() < 3 {
        return false; // needs at least two path words plus the minified token
    }
    let Some(token) = segments.pop() else {
        return false;
    };
    if !looks_like_minified_token(token) {
        return false;
    }
    // Remaining segments must be path words: lowercase letters or numeric
    // disambiguators only (any uppercase => an UPPER_SNAKE constant, not
    // synthetic), with at least one genuine lowercase word.
    let no_uppercase = segments.iter().all(|word| {
        word.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    });
    let has_real_word = segments
        .iter()
        .any(|word| word.len() >= 2 && word.chars().all(|c| c.is_ascii_lowercase()));
    no_uppercase && has_real_word
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
    literals: BTreeSet<String>,
    /// Call targets (`c:name` identifier callees, `m:method` method callees) for
    /// topology-based propagation.
    callees: BTreeSet<String>,
    /// All identifier references (raw names) — calls plus module-level
    /// variable/const/class references. Translated through the confirmed
    /// function+symbol name map, these are cross-evidence reinforcement anchors.
    references: BTreeSet<String>,
}

/// One subject (emitted) function with a recoverable name.
struct SubjectFunction {
    module_id: u32,
    subject_path: String,
    name: String,
    fingerprint: FunctionFingerprint,
    literals: BTreeSet<String>,
    callees: BTreeSet<String>,
    references: BTreeSet<String>,
}

/// Every named, specifically-named function across the whole reference tree.
fn collect_reference_functions(index: &ReferenceSourceIndex) -> Vec<ReferenceFunction> {
    let mut out = Vec::new();
    for module in &index.modules {
        let names: BTreeMap<reverts_ir::ByteRange, String> = function_names(module.source.as_str())
            .into_iter()
            .filter(|(_, name)| is_specific_reference_name(name))
            .collect();
        let mut literals = function_anchor_tokens(module.source.as_str());
        let mut callees = function_callee_names(module.source.as_str());
        let mut references = function_referenced_names(module.source.as_str());
        for fingerprint in
            FunctionExtractor::fingerprint_primary(ModuleId(0), module.source.as_str())
        {
            if let Some(name) = names.get(&fingerprint.id.span) {
                let function_literals = literals.remove(&fingerprint.id.span).unwrap_or_default();
                let function_callees = callees.remove(&fingerprint.id.span).unwrap_or_default();
                let function_references =
                    references.remove(&fingerprint.id.span).unwrap_or_default();
                out.push(ReferenceFunction {
                    file: module.file_path.clone(),
                    name: name.clone(),
                    fingerprint,
                    literals: function_literals,
                    callees: function_callees,
                    references: function_references,
                });
            }
        }
    }
    out
}

/// Synthetic module id for entrypoint-island functions. Real module ids are small
/// u32s; `u32::MAX` never collides and is absent from every `module_matched_file`,
/// so island functions are eligible ONLY for the module-INDEPENDENT accept passes
/// (0/3/4/5/6) and never the module-pair passes (1/2/2b). That is exactly the
/// global per-function matching the island requires (it has no single owner file).
const ISLAND_MODULE_ID: u32 = u32::MAX;

/// Fixed synthetic emit path of the entrypoint island (mirrors reverts-planner's
/// `ENTRYPOINT_ISLAND_PATH`). The island is not a model module, so it never reaches
/// the per-module subject set; this path keys its recovered binding names so the
/// planner applies them to the emitted `modules/entrypoint.ts`.
const ENTRYPOINT_ISLAND_PATH: &str = "modules/entrypoint.ts";

/// Every named function across all subject (emitted) modules.
fn collect_subject_functions(subjects: &[SubjectModule]) -> Vec<SubjectFunction> {
    let mut out = Vec::new();
    for subject in subjects {
        push_subject_functions_from_source(
            &mut out,
            subject.module_id,
            subject.file_path.as_str(),
            subject.source.as_str(),
        );
    }
    out
}

/// Entrypoint-island functions as subjects under a synthetic module id. The island
/// is a single emitted file aggregating thousands of root-scope first-party
/// functions that carry no model `ModuleId`, so they never enter the per-module
/// subject set. Matched with an empty `module_matched_file` they earn names only
/// from the module-independent (precision-gated) passes.
fn collect_island_functions(island_source: &str) -> Vec<SubjectFunction> {
    let mut out = Vec::new();
    push_subject_functions_from_source(
        &mut out,
        ISLAND_MODULE_ID,
        ENTRYPOINT_ISLAND_PATH,
        island_source,
    );
    out
}

/// Extract every named function from one emitted `source`, pushing a
/// [`SubjectFunction`] per function. Shared by the per-module subject collection
/// and the entrypoint-island collection so both use identical fingerprint/anchor
/// extraction.
fn push_subject_functions_from_source(
    out: &mut Vec<SubjectFunction>,
    module_id: u32,
    subject_path: &str,
    source: &str,
) {
    let names = function_names(source);
    let mut literals = function_anchor_tokens(source);
    let mut callees = function_callee_names(source);
    let mut references = function_referenced_names(source);
    for fingerprint in FunctionExtractor::fingerprint_primary(ModuleId(module_id), source) {
        if let Some(name) = names.get(&fingerprint.id.span) {
            let function_literals = literals.remove(&fingerprint.id.span).unwrap_or_default();
            let function_callees = callees.remove(&fingerprint.id.span).unwrap_or_default();
            let function_references = references.remove(&fingerprint.id.span).unwrap_or_default();
            out.push(SubjectFunction {
                module_id,
                subject_path: subject_path.to_string(),
                name: name.clone(),
                fingerprint,
                literals: function_literals,
                callees: function_callees,
                references: function_references,
            });
        }
    }
}

/// Minimum statement count for the corroboration-free (globally-unique composite)
/// accept pass. Below this, a unique composite signature is too low-entropy to
/// imply identity on its own, so trivial functions must earn acceptance through
/// module corroboration instead. See ACCEPT pass 0.
const MIN_CORROBORATION_FREE_STATEMENTS: u32 = 2;

/// Minimum body size for the within-pair optimal-assignment accept pass. Trivial
/// bodies carry too little entropy for a graded similarity to imply identity even
/// inside a confirmed module pair, so they stay proposals.
const MIN_WITHIN_PAIR_ASSIGN_STATEMENTS: u32 = 3;
/// Similarity floor for a within-pair assignment to be accepted (scale matches
/// `within_pair_similarity`: a single shared composite signature is 120).
const WITHIN_PAIR_ASSIGN_FLOOR: i64 = 150;
/// Skip the O(k³) assignment for pathologically large module pairs.
const WITHIN_PAIR_ASSIGN_LIMIT: usize = 300;

/// Graded similarity between a subject and reference function for the within-pair
/// optimal assignment. Returns `(score, hard)` where `hard` is true only when
/// there is real structural/content evidence (shared composite signature, exact
/// AST hash, or strong literal overlap) — arity/stmt closeness alone is soft and
/// never sets `hard`, so it can rank candidates but never justify an accept.
fn within_pair_similarity(
    s: &FunctionFingerprint,
    s_literals: &BTreeSet<String>,
    r: &FunctionFingerprint,
    r_literals: &BTreeSet<String>,
) -> (i64, bool) {
    let mut score = 0i64;
    let mut hard = false;
    let s_ast: BTreeSet<u64> = function_ast_hashes(s).into_iter().collect();
    if function_ast_hashes(r).iter().any(|h| s_ast.contains(h)) {
        score += 1000;
        hard = true;
    }
    let s_comp: BTreeSet<u64> = function_composites(s).into_iter().collect();
    let shared_comp = function_composites(r)
        .into_iter()
        .filter(|c| s_comp.contains(c))
        .count();
    if shared_comp > 0 {
        score += (shared_comp.min(5) as i64) * 120;
        hard = true;
    }
    let inter = s_literals.intersection(r_literals).count();
    let union = s_literals.union(r_literals).count().max(1);
    let jaccard = inter as f64 / union as f64;
    if jaccard > 0.0 {
        score += (jaccard * 300.0) as i64;
        if jaccard >= 0.34 {
            hard = true;
        }
    }
    // Previously-unused fingerprint evidence. `normalized_cfg` (control flow after
    // normalization) survives the decompiler's statement reshaping but is coarse
    // — many functions share a skeleton — so it ranks candidates without alone
    // making a match `hard`. `expression_shape` (the expression skeleton) is a
    // finer content signal the composite omits, so a match there counts as hard.
    if s.primary.normalized_cfg == r.primary.normalized_cfg {
        score += 120;
    }
    if let (Some(subject_shape), Some(reference_shape)) =
        (s.primary.expression_shape, r.primary.expression_shape)
        && subject_shape == reference_shape
    {
        score += 150;
        hard = true;
    }
    if s.param_count == r.param_count {
        score += 30;
    }
    if (s.statement_count as i64 - r.statement_count as i64).abs() <= 1 {
        score += 30;
    }
    (score, hard)
}

/// Max-weight perfect assignment (Kuhn-Munkres) on an n×m similarity matrix.
/// Returns, per row, the assigned column (or `None` if padded/unassigned). Pads to
/// square internally; O(k³) with k = max(n,m), bounded by `WITHIN_PAIR_ASSIGN_LIMIT`.
fn hungarian_max(sim: &[Vec<i64>]) -> Vec<Option<usize>> {
    let n = sim.len();
    if n == 0 {
        return Vec::new();
    }
    let m = sim[0].len();
    let k = n.max(m);
    let max_sim = sim
        .iter()
        .flat_map(|row| row.iter().copied())
        .max()
        .unwrap_or(0)
        .max(0);
    let big = max_sim + 1;
    // cost[i][j] = big - sim (min-cost ⇔ max-weight); padded entries similarity 0.
    let mut cost = vec![vec![big; k + 1]; k + 1];
    for i in 0..k {
        for j in 0..k {
            let s = if i < n && j < m { sim[i][j] } else { 0 };
            cost[i + 1][j + 1] = big - s;
        }
    }
    const INF: i64 = i64::MAX / 4;
    let mut u = vec![0i64; k + 1];
    let mut v = vec![0i64; k + 1];
    let mut p = vec![0usize; k + 1];
    let mut way = vec![0usize; k + 1];
    for i in 1..=k {
        p[0] = i;
        let mut j0 = 0usize;
        let mut minv = vec![INF; k + 1];
        let mut used = vec![false; k + 1];
        loop {
            used[j0] = true;
            let i0 = p[j0];
            let mut delta = INF;
            let mut j1 = 0usize;
            for j in 1..=k {
                if !used[j] {
                    let cur = cost[i0][j] - u[i0] - v[j];
                    if cur < minv[j] {
                        minv[j] = cur;
                        way[j] = j0;
                    }
                    if minv[j] < delta {
                        delta = minv[j];
                        j1 = j;
                    }
                }
            }
            for j in 0..=k {
                if used[j] {
                    u[p[j]] += delta;
                    v[j] -= delta;
                } else {
                    minv[j] -= delta;
                }
            }
            j0 = j1;
            if p[j0] == 0 {
                break;
            }
        }
        loop {
            let j1 = way[j0];
            p[j0] = p[j1];
            j0 = j1;
            if j0 == 0 {
                break;
            }
        }
    }
    let mut assign = vec![None; n];
    for j in 1..=k {
        let i = p[j];
        if i >= 1 && i <= n && j <= m {
            assign[i - 1] = Some(j - 1);
        }
    }
    assign
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
    match_function_lists_inner(subject_fns, reference_fns, module_matched_file, false)
}

/// Core matcher. `island_mode` keeps the passes that carry a DISTINCTIVE
/// per-function signature — pass 0 (globally-unique composite with exact param/stmt
/// agreement), pass 3 (anchor-set uniqueness), pass 4 (AST×rare-anchor joint) — and
/// disables the two that accept on graded structure or topology WITHOUT a
/// distinctive per-function signature: pass 5 (global within-pair similarity) and
/// pass 6 (call-graph propagation). Those two are safe inside a confirmed module
/// pair (a tiny candidate set, seeded by prior accepts) but when run GLOBALLY over
/// the entrypoint island with no module constraint they reach the least-distinctive
/// functions — where there is no ground truth to validate them and the
/// false-positive risk is unbounded (measured: 0/3/4 are 100% precise on the
/// real-named ground-truth set; 5/6 touch zero of it). The per-module flow keeps
/// `false` (full pass set, where 5/6 earn their recall under module corroboration).
fn match_function_lists_inner(
    subject_fns: &[SubjectFunction],
    reference_fns: &[ReferenceFunction],
    module_matched_file: &BTreeMap<u32, String>,
    island_mode: bool,
) -> Vec<BindingNameRow> {
    let mut ref_by_file: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    let mut ref_by_any: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    // Distinctive in-body string literal -> reference functions containing it.
    // A literal in exactly ONE reference function anchors AST-drifted matches.
    let mut ref_by_literal: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, r) in reference_fns.iter().enumerate() {
        ref_by_file.entry(r.file.as_str()).or_default().push(index);
        for hash in function_ast_hashes(&r.fingerprint) {
            ref_by_any.entry(hash).or_default().push(index);
        }
        for literal in &r.literals {
            ref_by_literal
                .entry(literal.as_str())
                .or_default()
                .push(index);
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
    // reference_fns indices already claimed by an accepted row, so the within-pair
    // assignment pass cannot reuse a reference function already taken.
    let mut used_ref: BTreeSet<usize> = BTreeSet::new();

    // ACCEPT pass 0: globally-unique COMPOSITE signature — identical across every
    // structural axis and one-of-a-kind in both corpora. Strong enough to accept
    // with no module match, so it names functions in unmatched modules too.
    for (subject_index, subject) in subject_fns.iter().enumerate() {
        let _ = subject_index;
        // Corroboration-free acceptance rests entirely on the composite signature
        // being one-of-a-kind in both corpora. A trivial body carries too little
        // structural entropy for that global uniqueness to imply identity:
        // single-statement helpers with the same shape recur across semantically
        // unrelated functions, so a "unique" trivial match is usually coincidence
        // (e.g. `globChars` -> `sdkCompatToolName`, placed in a file unrelated to
        // its own module's best candidate). Require a minimum body size here; below
        // it, the function can still be accepted via the module-corroborated pass
        // (where the matched-file constraint supplies the missing evidence) or fall
        // through to a proposal.
        if subject.fingerprint.statement_count < MIN_CORROBORATION_FREE_STATEMENTS {
            continue;
        }
        let Some(sig) = function_composites(&subject.fingerprint)
            .into_iter()
            .find(|sig| {
                subject_composite_freq.get(sig).copied().unwrap_or(0) == 1
                    && ref_by_composite.get(sig).is_some_and(|v| v.len() == 1)
            })
        else {
            continue;
        };
        let reference_idx = ref_by_composite[&sig][0];
        let reference = &reference_fns[reference_idx];
        if subject.fingerprint.param_count != reference.fingerprint.param_count
            || subject.fingerprint.statement_count != reference.fingerprint.statement_count
        {
            continue;
        }
        accepted.insert((subject.module_id, subject.name.clone()));
        used_ref.insert(reference_idx);
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
            used_ref.insert(ris[0]);
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

    // ACCEPT pass 2: within-pair OPTIMAL ASSIGNMENT (Kuhn-Munkres) for functions
    // whose AST hash drifted across versions, which the exact-hash pass 1 misses.
    // Inside a confirmed module pair the candidate set is tiny, so a graded
    // similarity (shared composite signatures + distinctive literal overlap +
    // arity/stmt closeness) plus optimal 1:1 assignment recovers the right pair.
    // Precision gates (kept conservative to preserve the ~0-false-positive accept
    // guarantee): (a) non-trivial body, (b) HARD structural evidence on the chosen
    // pair (shared composite signature or strong literal overlap — never arity
    // alone), (c) a clear MARGIN over the runner-up reference for that function.
    for (module_id, subject_indices) in &subject_by_module {
        let Some(file) = module_matched_file.get(module_id) else {
            continue;
        };
        let Some(reference_indices) = ref_by_file.get(file.as_str()) else {
            continue;
        };
        let s_pool: Vec<usize> = subject_indices
            .iter()
            .copied()
            .filter(|&si| {
                let s = &subject_fns[si];
                s.fingerprint.statement_count >= MIN_WITHIN_PAIR_ASSIGN_STATEMENTS
                    && !accepted.contains(&(s.module_id, s.name.clone()))
            })
            .collect();
        let r_pool: Vec<usize> = reference_indices
            .iter()
            .copied()
            .filter(|&ri| !used_ref.contains(&ri))
            .collect();
        if s_pool.is_empty() || r_pool.is_empty() {
            continue;
        }
        if s_pool.len().min(r_pool.len()) > WITHIN_PAIR_ASSIGN_LIMIT {
            continue;
        }
        let n = s_pool.len();
        let m = r_pool.len();
        let mut sim = vec![vec![0i64; m]; n];
        let mut hard = vec![vec![false; m]; n];
        for (a, &si) in s_pool.iter().enumerate() {
            for (b, &ri) in r_pool.iter().enumerate() {
                let (score, is_hard) = within_pair_similarity(
                    &subject_fns[si].fingerprint,
                    &subject_fns[si].literals,
                    &reference_fns[ri].fingerprint,
                    &reference_fns[ri].literals,
                );
                sim[a][b] = score;
                hard[a][b] = is_hard;
            }
        }
        let assignment = hungarian_max(&sim);
        for (a, b_opt) in assignment.iter().enumerate() {
            let Some(b) = *b_opt else { continue };
            if !hard[a][b] || sim[a][b] < WITHIN_PAIR_ASSIGN_FLOOR {
                continue;
            }
            // Margin: the chosen reference must clearly beat the runner-up for this
            // function, else the assignment is ambiguous and we leave it to review.
            let runner_up = (0..m)
                .filter(|&b2| b2 != b)
                .map(|b2| sim[a][b2])
                .max()
                .unwrap_or(0);
            if sim[a][b] < runner_up * 3 / 2 {
                continue;
            }
            let subject = &subject_fns[s_pool[a]];
            let reference = &reference_fns[r_pool[b]];
            accepted.insert((subject.module_id, subject.name.clone()));
            used_ref.insert(r_pool[b]);
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
                score: 1.5,
            });
        }
    }

    // ACCEPT pass 2b: module-prior RELAXED bijective assignment. Prior: the two
    // builds are the same app, so within a confirmed module pair functions
    // correspond ~1:1. That over-constrains the optimal assignment, so a soft
    // per-pair signal (shared normalized control flow / expression shape /
    // partial anchor overlap) is trustworthy when it is the unique best 1:1 match
    // in the matched module. Drop pass 2's HARD requirement, keep arity + margin.
    const RELAXED_ASSIGN_FLOOR: i64 = 150;
    for (module_id, subject_indices) in &subject_by_module {
        let Some(file) = module_matched_file.get(module_id) else {
            continue;
        };
        let Some(reference_indices) = ref_by_file.get(file.as_str()) else {
            continue;
        };
        let s_pool: Vec<usize> = subject_indices
            .iter()
            .copied()
            .filter(|&si| {
                let s = &subject_fns[si];
                s.fingerprint.statement_count >= MIN_WITHIN_PAIR_ASSIGN_STATEMENTS
                    && !accepted.contains(&(s.module_id, s.name.clone()))
            })
            .collect();
        let r_pool: Vec<usize> = reference_indices
            .iter()
            .copied()
            .filter(|&ri| !used_ref.contains(&ri))
            .collect();
        if s_pool.is_empty()
            || r_pool.is_empty()
            || s_pool.len().min(r_pool.len()) > WITHIN_PAIR_ASSIGN_LIMIT
        {
            continue;
        }
        let (n, m) = (s_pool.len(), r_pool.len());
        let mut sim = vec![vec![0i64; m]; n];
        for (a, &si) in s_pool.iter().enumerate() {
            for (b, &ri) in r_pool.iter().enumerate() {
                sim[a][b] = within_pair_similarity(
                    &subject_fns[si].fingerprint,
                    &subject_fns[si].literals,
                    &reference_fns[ri].fingerprint,
                    &reference_fns[ri].literals,
                )
                .0;
            }
        }
        for (a, b_opt) in hungarian_max(&sim).iter().enumerate() {
            let Some(b) = *b_opt else { continue };
            if sim[a][b] < RELAXED_ASSIGN_FLOOR {
                continue;
            }
            let runner_up = (0..m)
                .filter(|&b2| b2 != b)
                .map(|b2| sim[a][b2])
                .max()
                .unwrap_or(0);
            if sim[a][b] * 5 < runner_up * 6 {
                continue; // require >=1.2x margin over the runner-up
            }
            let subject = &subject_fns[s_pool[a]];
            let reference = &reference_fns[r_pool[b]];
            if subject.fingerprint.param_count != reference.fingerprint.param_count {
                continue;
            }
            accepted.insert((subject.module_id, subject.name.clone()));
            used_ref.insert(r_pool[b]);
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
                score: 1.7,
            });
        }
    }

    // ACCEPT pass 3: distinctive ANCHOR-SET uniqueness (module-INDEPENDENT).
    //
    // The decompiler-output -> clean-source structural gap defeats the AST and
    // composite axes (they barely fire), so passes 0/1/2 only reach the small
    // set of modules that matched. But the NAMES a function calls and accesses
    // plus its string literals — its "anchor set" — survive that gap. A subject
    // function whose anchor set overlaps exactly one reference function with high
    // Jaccard, a clear margin over the runner-up, AND shares >=2 RARE anchors
    // (carried by <=3 reference functions, so the overlap is not just common API
    // calls) is the same function — even in a module that never matched. This is
    // what reaches the ~1865 unmatched modules.
    const MIN_ANCHOR_ACCEPT_TOKENS: usize = 4;
    const ANCHOR_ACCEPT_JACCARD: f64 = 0.6;
    const ANCHOR_ACCEPT_MARGIN: f64 = 0.2;
    const RARE_ANCHOR_MAX_REFS: usize = 4;
    const MIN_RARE_SHARED_ANCHORS: usize = 2;
    for subject in subject_fns {
        if accepted.contains(&(subject.module_id, subject.name.clone()))
            || subject.literals.len() < MIN_ANCHOR_ACCEPT_TOKENS
        {
            continue;
        }
        let mut candidates: BTreeSet<usize> = BTreeSet::new();
        for anchor in &subject.literals {
            if let Some(indices) = ref_by_literal.get(anchor.as_str()) {
                for &ri in indices {
                    candidates.insert(ri);
                }
            }
        }
        let (mut best, mut runner_up): (Option<(f64, usize)>, f64) = (None, 0.0);
        for ri in candidates {
            if used_ref.contains(&ri) || reference_fns[ri].literals.len() < 4 {
                continue;
            }
            let reference = &reference_fns[ri];
            let intersection = subject.literals.intersection(&reference.literals).count();
            let union = subject.literals.union(&reference.literals).count().max(1);
            let jaccard = intersection as f64 / union as f64;
            match best {
                Some((best_jaccard, _)) if jaccard <= best_jaccard => {
                    runner_up = runner_up.max(jaccard);
                }
                Some((best_jaccard, _)) => {
                    runner_up = runner_up.max(best_jaccard);
                    best = Some((jaccard, ri));
                }
                None => best = Some((jaccard, ri)),
            }
        }
        let Some((best_jaccard, ri)) = best else {
            continue;
        };
        if best_jaccard < ANCHOR_ACCEPT_JACCARD || runner_up > best_jaccard - ANCHOR_ACCEPT_MARGIN {
            continue;
        }
        let reference = &reference_fns[ri];
        let rare_shared = subject
            .literals
            .intersection(&reference.literals)
            .filter(|anchor| {
                ref_by_literal
                    .get(anchor.as_str())
                    .is_some_and(|refs| refs.len() <= RARE_ANCHOR_MAX_REFS)
            })
            .count();
        if rare_shared < MIN_RARE_SHARED_ANCHORS {
            continue;
        }
        accepted.insert((subject.module_id, subject.name.clone()));
        used_ref.insert(ri);
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
            score: best_jaccard,
        });
    }

    // ACCEPT pass 3b: multi-rare-anchor agreement, Jaccard-free (module-INDEPENDENT).
    // Pass 3 gates on Jaccard >= 0.6, which blocks a function with many anchors but
    // few shared (getReport: 3 shared / 10 union -> Jaccard 0.3). But >=3 GLOBALLY
    // -RARE anchors (each carried by <=2 reference functions) all pointing to the
    // SAME reference function is decisive identity on its own, regardless of Jaccard.
    // Gate on a strict count margin over the runner-up + exact arity (the AST
    // param_count is reliable — see the param-counting finding).
    const PASS3B_RARE_MAX_REFS: usize = 2;
    const PASS3B_MIN_RARE_SHARED: usize = 3;
    for subject in subject_fns {
        if accepted.contains(&(subject.module_id, subject.name.clone())) {
            continue;
        }
        let mut rare_shared: BTreeMap<usize, usize> = BTreeMap::new();
        for anchor in &subject.literals {
            if let Some(indices) = ref_by_literal.get(anchor.as_str())
                && indices.len() <= PASS3B_RARE_MAX_REFS
            {
                for &ri in indices {
                    if !used_ref.contains(&ri) {
                        *rare_shared.entry(ri).or_default() += 1;
                    }
                }
            }
        }
        let (mut best_count, mut best_ri, mut runner) = (0usize, usize::MAX, 0usize);
        for (&ri, &count) in &rare_shared {
            if count > best_count {
                runner = best_count;
                best_count = count;
                best_ri = ri;
            } else if count > runner {
                runner = count;
            }
        }
        // Need >=3 globally-rare shared anchors AND a strict margin over any other
        // reference function (no second contender with as many).
        if best_count < PASS3B_MIN_RARE_SHARED || best_count <= runner {
            continue;
        }
        let reference = &reference_fns[best_ri];
        if subject.fingerprint.param_count != reference.fingerprint.param_count {
            continue;
        }
        accepted.insert((subject.module_id, subject.name.clone()));
        used_ref.insert(best_ri);
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
            score: 3.6,
        });
    }

    // ACCEPT pass 4: AST-hash + rare-anchor JOINT corroboration (module-INDEPENDENT).
    //
    // A shared AST hash alone is too collision-prone to accept (esbuild helpers,
    // generic one-line shapes), and a single rare anchor alone can be a
    // coincidence. But a subject that shares an AST hash with a reference AND
    // shares a RARE anchor with that SAME reference has two INDEPENDENT
    // agreements — structural and semantic. Requiring exactly one reference to
    // satisfy both makes the joint signal decisive, converting the strongest
    // proposals to accepts and reaching drifted functions in unmatched modules.
    const PASS4_RARE_MAX_REFS: usize = 4;
    for subject in subject_fns {
        if accepted.contains(&(subject.module_id, subject.name.clone())) {
            continue;
        }
        let mut ast_candidates: BTreeSet<usize> = BTreeSet::new();
        for hash in function_ast_hashes(&subject.fingerprint) {
            if let Some(indices) = ref_by_any.get(&hash) {
                for &ri in indices {
                    ast_candidates.insert(ri);
                }
            }
        }
        let mut corroborated: Vec<usize> = Vec::new();
        for &ri in &ast_candidates {
            if used_ref.contains(&ri) {
                continue;
            }
            let reference = &reference_fns[ri];
            let rare_shared = subject
                .literals
                .intersection(&reference.literals)
                .filter(|anchor| {
                    ref_by_literal
                        .get(anchor.as_str())
                        .is_some_and(|refs| refs.len() <= PASS4_RARE_MAX_REFS)
                })
                .count();
            if rare_shared >= 1 {
                corroborated.push(ri);
            }
        }
        // Unique joint match only — two functions agreeing on BOTH axes with the
        // subject would be ambiguous, so leave those as proposals.
        if corroborated.len() != 1 {
            continue;
        }
        let ri = corroborated[0];
        let reference = &reference_fns[ri];
        // Arity must agree: a shared AST hash ignores parameter count, so a
        // 1-param and 2-param function can collide — that is not a match.
        if subject.fingerprint.param_count != reference.fingerprint.param_count {
            continue;
        }
        accepted.insert((subject.module_id, subject.name.clone()));
        used_ref.insert(ri);
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
            score: 3.0,
        });
    }

    // ACCEPT pass 4b: GLOBALLY-UNIQUE single anchor + exact arity/stmt
    // (module-INDEPENDENT). A string literal carried by EXACTLY ONE reference
    // function (a distinctive error message / URL / config key) that also appears in
    // a subject function is decisive identity evidence on its own. Pass 3 requires
    // >=2 rare shared anchors and pass 4 requires an AST-hash agreement, so a
    // function with a single decisive anchor whose body drifted past the AST hash
    // falls through both and stays a proposal. Promote it when (a) the subject shares
    // a globally-unique reference literal with exactly ONE reference function (no
    // cross-function ambiguity), (b) param AND statement counts agree EXACTLY
    // (structural corroboration that the lone anchor is not a coincidence), and (c)
    // the body is non-trivial. This mines the strongest tail of the proposal pool.
    const PASS4B_MIN_STATEMENTS: u32 = 2;
    for subject in subject_fns {
        if accepted.contains(&(subject.module_id, subject.name.clone()))
            || subject.fingerprint.statement_count < PASS4B_MIN_STATEMENTS
        {
            continue;
        }
        let mut targets: BTreeSet<usize> = BTreeSet::new();
        for anchor in &subject.literals {
            if let Some(indices) = ref_by_literal.get(anchor.as_str())
                && indices.len() == 1
                && !used_ref.contains(&indices[0])
            {
                targets.insert(indices[0]);
            }
        }
        if targets.len() != 1 {
            continue; // no decisive anchor, or ambiguous across reference functions
        }
        let ri = *targets
            .iter()
            .next()
            .expect("exactly one target checked above");
        let reference = &reference_fns[ri];
        // Param count must agree exactly; statement count may drift by 1 — the body
        // already drifted past the AST hash, so a one-statement difference is
        // expected, and the globally-unique anchor is near-decisive on its own.
        if subject.fingerprint.param_count != reference.fingerprint.param_count
            || (i64::from(subject.fingerprint.statement_count)
                - i64::from(reference.fingerprint.statement_count))
            .abs()
                > 1
        {
            continue;
        }
        accepted.insert((subject.module_id, subject.name.clone()));
        used_ref.insert(ri);
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
            score: 3.5,
        });
    }

    // ACCEPT pass 5: global anchor-seeded WITHIN-PAIR similarity (module-INDEPENDENT).
    //
    // Generalizes the module-gated within-pair assignment (pass 2) to the whole
    // corpus: gather candidate references by shared AST hash OR shared anchor,
    // score each with `within_pair_similarity` (AST + composite + anchor Jaccard +
    // arity), and accept the best reference iff the evidence is HARD (a real
    // structural or content agreement, never arity alone), clears a score floor,
    // and beats the runner-up by a decisive margin. This catches functions whose
    // combined-axis similarity is strong but that no single earlier pass pinned.
    const PASS5_SCORE_FLOOR: i64 = 420;
    const PASS5_MARGIN: i64 = 200;
    // Tiny bodies (1-2 statements) recur identically across unrelated helpers, so
    // a bare AST-hash agreement on them is ambiguous, not identity. Require a
    // body with enough structural entropy that a within-pair agreement implies
    // the same function.
    const PASS5_MIN_STATEMENTS: u32 = 3;
    for subject in subject_fns {
        if island_mode
            || accepted.contains(&(subject.module_id, subject.name.clone()))
            || subject.fingerprint.statement_count < PASS5_MIN_STATEMENTS
        {
            continue;
        }
        let mut candidates: BTreeSet<usize> = BTreeSet::new();
        for hash in function_ast_hashes(&subject.fingerprint) {
            if let Some(indices) = ref_by_any.get(&hash) {
                candidates.extend(indices.iter().copied());
            }
        }
        for anchor in &subject.literals {
            if let Some(indices) = ref_by_literal.get(anchor.as_str()) {
                candidates.extend(indices.iter().copied());
            }
        }
        let mut scored: Vec<(i64, bool, usize)> = candidates
            .into_iter()
            .filter(|ri| !used_ref.contains(ri))
            .map(|ri| {
                let reference = &reference_fns[ri];
                let (score, hard) = within_pair_similarity(
                    &subject.fingerprint,
                    &subject.literals,
                    &reference.fingerprint,
                    &reference.literals,
                );
                (score, hard, ri)
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        let Some(&(best_score, hard, ri)) = scored.first() else {
            continue;
        };
        let runner_up = scored.get(1).map_or(0, |entry| entry.0);
        if !hard || best_score < PASS5_SCORE_FLOOR || best_score - runner_up < PASS5_MARGIN {
            continue;
        }
        let reference = &reference_fns[ri];
        if subject.fingerprint.param_count != reference.fingerprint.param_count {
            continue;
        }
        accepted.insert((subject.module_id, subject.name.clone()));
        used_ref.insert(ri);
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
            score: 4.0,
        });
    }

    // ACCEPT pass 6: CALL-GRAPH PROPAGATION (topology, not body; iterative).
    //
    // The call graph survives minify+decompile even when bodies diverge: which
    // functions a function calls is a build-invariant. Seeded by the matches
    // above, a subject function whose RESOLVED call targets — its minified
    // identifier callees translated through the accepted subject->reference name
    // map, plus preserved method names — uniquely overlap one reference
    // function's call targets on >=2 matched FUNCTION callees is the same
    // function. Each round's new matches grow the map, so matching propagates
    // outward along call edges (the mechanism that turns matched modules into
    // matched internal functions, which body fingerprints cannot).
    const PROPAGATE_MIN_SHARED_FN_CALLEES: usize = 2;
    const PROPAGATE_MIN_TOTAL_OVERLAP: usize = 3;
    const PROPAGATE_MAX_ROUNDS: usize = 6;
    let mut ref_by_callee: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, reference) in reference_fns.iter().enumerate() {
        for callee in &reference.callees {
            ref_by_callee
                .entry(callee.as_str())
                .or_default()
                .push(index);
        }
    }
    let propagate_rounds = if island_mode {
        0 // call-graph topology pass — disabled for global island matching
    } else {
        PROPAGATE_MAX_ROUNDS
    };
    for _round in 0..propagate_rounds {
        // subject minified name -> reference name, from everything accepted so far.
        let name_map: BTreeMap<String, String> = rows
            .iter()
            .filter(|row| row.accepted)
            .map(|row| (row.original_name.clone(), row.semantic_name.clone()))
            .collect();
        let mut new_rows: Vec<BindingNameRow> = Vec::new();
        let mut round_matches = 0usize;
        for subject in subject_fns {
            if accepted.contains(&(subject.module_id, subject.name.clone())) {
                continue;
            }
            // Resolve call targets: translate matched identifier callees to their
            // reference name; keep method names; drop still-minified callees.
            let resolved: BTreeSet<String> = subject
                .callees
                .iter()
                .map(|callee| {
                    // Keep every callee: translate a matched minified name to its
                    // confirmed real name, and keep already-real callees as-is
                    // (they match the reference's real callees directly). Method
                    // callees (`m:`) pass through unchanged.
                    if let Some(minified) = callee.strip_prefix("c:") {
                        format!(
                            "c:{}",
                            name_map.get(minified).map_or(minified, String::as_str)
                        )
                    } else {
                        callee.clone()
                    }
                })
                .collect();
            if resolved.len() < PROPAGATE_MIN_TOTAL_OVERLAP {
                continue;
            }
            let mut candidates: BTreeSet<usize> = BTreeSet::new();
            for token in &resolved {
                if let Some(indices) = ref_by_callee.get(token.as_str()) {
                    candidates.extend(indices.iter().copied());
                }
            }
            // Score each candidate by total overlap and shared matched-function
            // callees (the strong signal). Keep the best and the runner-up.
            // Best candidate as (shared_fn_callees, total_overlap, ref_index);
            // runner-up as (shared_fn_callees, total_overlap) for the margin check.
            let mut best: Option<(usize, usize, usize)> = None;
            let mut runner: (usize, usize) = (0, 0);
            for index in candidates {
                if used_ref.contains(&index) {
                    continue;
                }
                let reference = &reference_fns[index];
                let shared: BTreeSet<&String> = resolved.intersection(&reference.callees).collect();
                let total = shared.len();
                let shared_fns = shared
                    .iter()
                    .filter(|token| token.starts_with("c:"))
                    .count();
                let score = (shared_fns, total);
                match best {
                    Some((b_fns, b_total, _)) if (shared_fns, total) <= (b_fns, b_total) => {
                        runner = runner.max(score);
                    }
                    Some((b_fns, b_total, _)) => {
                        runner = runner.max((b_fns, b_total));
                        best = Some((shared_fns, total, index));
                    }
                    None => best = Some((shared_fns, total, index)),
                }
            }
            let Some((shared_fns, total, index)) = best else {
                continue;
            };
            // Require enough matched-function overlap, a clear margin over the
            // runner-up, and arity agreement.
            if shared_fns < PROPAGATE_MIN_SHARED_FN_CALLEES
                || total < PROPAGATE_MIN_TOTAL_OVERLAP
                || (shared_fns, total) <= runner
            {
                continue;
            }
            let reference = &reference_fns[index];
            if subject.fingerprint.param_count != reference.fingerprint.param_count {
                continue;
            }
            accepted.insert((subject.module_id, subject.name.clone()));
            used_ref.insert(index);
            new_rows.push(BindingNameRow {
                module_id: subject.module_id,
                subject_path: subject.subject_path.clone(),
                reference_file: reference.file.clone(),
                original_name: subject.name.clone(),
                semantic_name: reference.name.clone(),
                accepted: true,
                ast_hash: subject.fingerprint.primary.ast,
                param_count: subject.fingerprint.param_count,
                statement_count: subject.fingerprint.statement_count,
                score: 5.0,
            });
            round_matches += 1;
        }
        rows.extend(new_rows);
        if round_matches == 0 {
            break;
        }
    }

    // PROPOSAL pass: global, for every subject function not auto-accepted.
    // Candidates come from shared AST hashes AND shared DISTINCTIVE in-body
    // string literals (the latter recovers functions whose AST drifted across
    // versions but kept a unique error message / URL / config key).
    for subject in subject_fns {
        if accepted.contains(&(subject.module_id, subject.name.clone())) {
            continue;
        }
        let mut scores = BTreeMap::<usize, f64>::new();
        for h in function_ast_hashes(&subject.fingerprint) {
            if let Some(indices) = ref_by_any.get(&h) {
                for &ri in indices {
                    if let Some(score) =
                        binding_proposal_score(&subject.fingerprint, &reference_fns[ri].fingerprint)
                    {
                        let entry = scores.entry(ri).or_insert(0.0);
                        *entry = entry.max(score);
                    }
                }
            }
        }
        for literal in &subject.literals {
            // Only literals that are distinctive on the reference side (present
            // in exactly one reference function) carry identifying signal.
            if let Some(indices) = ref_by_literal.get(literal.as_str())
                && indices.len() == 1
            {
                let ri = indices[0];
                let reference = &reference_fns[ri];
                let mut score = 60.0; // a unique shared literal is strong evidence
                if subject.fingerprint.param_count == reference.fingerprint.param_count {
                    score += 5.0;
                }
                let entry = scores.entry(ri).or_insert(0.0);
                *entry = entry.max(score);
            }
        }
        let mut scored: Vec<(f64, usize)> = scores.into_iter().map(|(ri, s)| (s, ri)).collect();
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

/// Identifier `uses` (in AST order) and the set of names bound anywhere inside a
/// function span. Module-level symbols are uses not in the bound set.
fn span_uses_and_bound(
    streams: &IdentifierStreams,
    span: reverts_ir::ByteRange,
) -> (Vec<&str>, BTreeSet<&str>) {
    let uses = streams
        .uses
        .iter()
        .filter(|(start, _)| *start >= span.start && *start < span.end)
        .map(|(_, name)| name.as_str())
        .collect();
    let bound = streams
        .bindings
        .iter()
        .filter(|(start, _, _)| *start >= span.start && *start < span.end)
        .map(|(_, _, name)| name.as_str())
        .collect();
    (uses, bound)
}

/// Symbol propagation (forward references). Each ACCEPTED function match whose
/// pair is AST-isomorphic (`primary.ast` equal) gives a positional identifier
/// alignment: walking both functions' `uses` streams in lockstep, a subject
/// identifier NOT bound inside the function is a MODULE-LEVEL symbol whose
/// reference-side counterpart is its real name. Votes aggregate across all
/// anchors; a module-level minified symbol named consistently by ≥2 anchors is
/// accepted, a single anchor proposes. This names the non-function symbols
/// (consts, classes, imported bindings) that matched functions reference.
fn propagate_symbols(
    subjects: &[SubjectModule],
    reference_source: &BTreeMap<&str, &str>,
    subject_fns: &[SubjectFunction],
    reference_fns: &[ReferenceFunction],
    binding_rows: &[BindingNameRow],
) -> Vec<BindingNameRow> {
    let subject_by_module_name: BTreeMap<(u32, &str), usize> = subject_fns
        .iter()
        .enumerate()
        .map(|(i, f)| ((f.module_id, f.name.as_str()), i))
        .collect();
    let reference_by_file_name: BTreeMap<(&str, &str), usize> = reference_fns
        .iter()
        .enumerate()
        .map(|(i, f)| ((f.file.as_str(), f.name.as_str()), i))
        .collect();
    let subject_source: BTreeMap<u32, &str> = subjects
        .iter()
        .map(|s| (s.module_id, s.source.as_str()))
        .collect();
    let subject_path: BTreeMap<u32, &str> = subjects
        .iter()
        .map(|s| (s.module_id, s.file_path.as_str()))
        .collect();

    let mut subject_streams: BTreeMap<u32, IdentifierStreams> = BTreeMap::new();
    let mut reference_streams: BTreeMap<String, IdentifierStreams> = BTreeMap::new();
    // (module_id, subject_name) -> reference_name -> (votes, sample reference file)
    let mut votes: BTreeMap<(u32, String), BTreeMap<String, (usize, String)>> = BTreeMap::new();
    // Names the function track already settled: don't re-propose them.
    let already: BTreeSet<(u32, &str)> = binding_rows
        .iter()
        .map(|r| (r.module_id, r.original_name.as_str()))
        .collect();

    for row in binding_rows.iter().filter(|r| r.accepted) {
        let (Some(&si), Some(&ri)) = (
            subject_by_module_name.get(&(row.module_id, row.original_name.as_str())),
            reference_by_file_name.get(&(row.reference_file.as_str(), row.semantic_name.as_str())),
        ) else {
            continue;
        };
        let subject = &subject_fns[si];
        let reference = &reference_fns[ri];
        if subject.fingerprint.primary.ast != reference.fingerprint.primary.ast {
            continue; // only lockstep-align truly isomorphic bodies
        }
        let (Some(&s_src), Some(&r_src)) = (
            subject_source.get(&row.module_id),
            reference_source.get(row.reference_file.as_str()),
        ) else {
            continue;
        };
        subject_streams
            .entry(row.module_id)
            .or_insert_with(|| identifier_streams(s_src));
        reference_streams
            .entry(row.reference_file.clone())
            .or_insert_with(|| identifier_streams(r_src));
        let (s_uses, s_bound) = span_uses_and_bound(
            &subject_streams[&row.module_id],
            subject.fingerprint.id.span,
        );
        let (r_uses, _) = span_uses_and_bound(
            &reference_streams[&row.reference_file],
            reference.fingerprint.id.span,
        );
        if s_uses.len() != r_uses.len() {
            continue; // alignment would be unsafe
        }
        for (sn, rn) in s_uses.iter().zip(r_uses.iter()) {
            if sn == rn || s_bound.contains(sn) || !is_specific_reference_name(rn) {
                continue;
            }
            let entry = votes
                .entry((row.module_id, (*sn).to_string()))
                .or_default()
                .entry((*rn).to_string())
                .or_insert((0, row.reference_file.clone()));
            entry.0 += 1;
        }
    }

    let mut rows = Vec::new();
    for ((module_id, subject_name), tally) in votes {
        if already.contains(&(module_id, subject_name.as_str())) {
            continue; // function track already named it
        }
        // Winner = most votes; accepted only on a strict majority with >=2 anchors.
        let mut winner: Option<(&String, usize, &String)> = None;
        let mut runner_up = 0usize;
        for (name, (count, file)) in &tally {
            match winner {
                Some((_, best, _)) if best >= *count => {
                    runner_up = runner_up.max(*count);
                }
                _ => {
                    if let Some((_, best, _)) = winner {
                        runner_up = runner_up.max(best);
                    }
                    winner = Some((name, *count, file));
                }
            }
        }
        let Some((name, count, file)) = winner else {
            continue;
        };
        let accepted = count >= 2 && count > runner_up;
        let Some(&path) = subject_path.get(&module_id) else {
            continue;
        };
        rows.push(BindingNameRow {
            module_id,
            subject_path: path.to_string(),
            reference_file: file.clone(),
            original_name: subject_name,
            semantic_name: name.clone(),
            accepted,
            ast_hash: 0,
            param_count: 0,
            statement_count: 0,
            score: count as f64,
        });
    }
    rows
}

/// Drop accepted renames that would overwrite a function's already-real
/// (non-minified, esbuild-preserved) name with a DIFFERENT name. A preserved
/// name IS the correct name, so a structural/anchor/topology coincidence must
/// not overturn it (e.g. `setRegion` -> `setMcpAuthCacheEntry`). Self-renames and
/// renames of minified names are kept.
fn drop_real_name_remaps(binding_rows: &mut Vec<BindingNameRow>) {
    binding_rows.retain(|row| {
        // A preserved real name is a genuine word/identifier esbuild did NOT
        // minify: >= 6 chars with a lowercase letter (e.g. `setRegion`,
        // `getBitsLength`, `isValid`). Those are already correct, so never
        // overwrite them with a different match. Short minified names (`XCK`,
        // `GYK`, `Ju8` — <= 4 chars) are NOT protected, so genuine minified->real
        // renames are kept. (Using `is_specific_reference_name` here was a bug: it
        // treats `XCK` as "specific" and dropped every real recovered name.)
        let original_is_preserved_real = row.original_name.chars().count() >= 6
            && row.original_name.chars().any(|c| c.is_ascii_lowercase());
        !(row.accepted && row.semantic_name != row.original_name && original_is_preserved_real)
    });
}

/// Minimum CONFIRMED references a subject function must share with a unique
/// reference function to accept it by cross-evidence reinforcement.
const REINFORCE_MIN_SHARED: usize = 3;
/// Bound the reinforcement fixpoint iterations.
const REINFORCE_MAX_ROUNDS: usize = 6;

/// Iterative cross-evidence reinforcement (the "confirmed matches ARE evidence"
/// loop). Uses ALL confirmed matches so far — functions AND module-level symbols
/// — as anchors: a subject function's minified identifier references are
/// translated through the accepted `original -> semantic` name map, and a
/// function whose CONFIRMED references overlap exactly one reference function on
/// at least [`REINFORCE_MIN_SHARED`] names (clear margin, matching arity) is the
/// same function. This is build-invariant (it rests on WHICH confirmed
/// entities a function uses, not on its body shape), so it reaches functions the
/// fingerprint passes miss. Each round's new matches grow the map; iterate to a
/// fixpoint. Appends accepted rows to `binding_rows`.
fn propagate_by_reference_topology(
    subject_fns: &[SubjectFunction],
    reference_fns: &[ReferenceFunction],
    binding_rows: &mut Vec<BindingNameRow>,
) {
    let mut ref_by_reference: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, reference) in reference_fns.iter().enumerate() {
        for name in &reference.references {
            ref_by_reference
                .entry(name.as_str())
                .or_default()
                .push(index);
        }
    }
    let mut accepted: BTreeSet<(u32, String)> = binding_rows
        .iter()
        .filter(|row| row.accepted)
        .map(|row| (row.module_id, row.original_name.clone()))
        .collect();
    for _round in 0..REINFORCE_MAX_ROUNDS {
        let name_map: BTreeMap<String, String> = binding_rows
            .iter()
            .filter(|row| row.accepted)
            .map(|row| (row.original_name.clone(), row.semantic_name.clone()))
            .collect();
        // A reference function already claimed by any accepted row is off-limits,
        // so a single reference isn't assigned to two subjects.
        let mut used_reference: BTreeSet<(&str, &str)> = binding_rows
            .iter()
            .filter(|row| row.accepted)
            .map(|row| (row.reference_file.as_str(), row.semantic_name.as_str()))
            .collect();
        let mut new_rows: Vec<BindingNameRow> = Vec::new();
        for subject in subject_fns {
            if accepted.contains(&(subject.module_id, subject.name.clone())) {
                continue;
            }
            // Keep EVERY reference: translate matched minified names to their
            // confirmed real name, and keep names that are already real as-is
            // (they match the reference's real names directly). Unmatched minified
            // names are kept too but harmlessly never overlap a real reference
            // name. (Previously this DROPPED untranslated names, throwing away the
            // already-confirmed real references — the strongest topology signal.)
            let resolved: BTreeSet<String> = subject
                .references
                .iter()
                .map(|name| name_map.get(name).cloned().unwrap_or_else(|| name.clone()))
                .collect();
            if resolved.len() < REINFORCE_MIN_SHARED {
                continue;
            }
            let mut shared_by_reference: BTreeMap<usize, usize> = BTreeMap::new();
            for token in &resolved {
                if let Some(indices) = ref_by_reference.get(token.as_str()) {
                    for &index in indices {
                        *shared_by_reference.entry(index).or_default() += 1;
                    }
                }
            }
            let (mut best, mut runner_up): (Option<(usize, usize)>, usize) = (None, 0);
            for (index, shared) in shared_by_reference {
                let reference = &reference_fns[index];
                if used_reference.contains(&(reference.file.as_str(), reference.name.as_str())) {
                    continue;
                }
                match best {
                    Some((best_shared, _)) if shared <= best_shared => {
                        runner_up = runner_up.max(shared);
                    }
                    Some((best_shared, _)) => {
                        runner_up = runner_up.max(best_shared);
                        best = Some((shared, index));
                    }
                    None => best = Some((shared, index)),
                }
            }
            let Some((shared, index)) = best else {
                continue;
            };
            if shared < REINFORCE_MIN_SHARED || shared <= runner_up {
                continue;
            }
            let reference = &reference_fns[index];
            if subject.fingerprint.param_count != reference.fingerprint.param_count {
                continue;
            }
            // Never rename a function that already carries a real (non-minified)
            // name to a DIFFERENT name — topology overlap is not strong enough to
            // overturn a preserved name (e.g. `setRegion` -> `setMcpAuthCacheEntry`).
            if reference.name != subject.name && is_specific_reference_name(&subject.name) {
                continue;
            }
            accepted.insert((subject.module_id, subject.name.clone()));
            used_reference.insert((reference.file.as_str(), reference.name.as_str()));
            new_rows.push(BindingNameRow {
                module_id: subject.module_id,
                subject_path: subject.subject_path.clone(),
                reference_file: reference.file.clone(),
                original_name: subject.name.clone(),
                semantic_name: reference.name.clone(),
                accepted: true,
                ast_hash: subject.fingerprint.primary.ast,
                param_count: subject.fingerprint.param_count,
                statement_count: subject.fingerprint.statement_count,
                score: 6.0,
            });
        }
        if new_rows.is_empty() {
            break;
        }
        binding_rows.extend(new_rows);
    }
}

/// Minimum accepted function renames pointing at one reference file for the
/// function track to *promote* an otherwise-unmatched module to that file.
const MODULE_PROMOTE_MIN_FUNCTIONS: usize = 2;

/// #1 Function->module reinforcement. A module with ≥
/// [`MODULE_PROMOTE_MIN_FUNCTIONS`] ACCEPTED function renames all pointing at one
/// reference file is almost certainly that file, even when module-level content
/// matching missed it (minified module, few string anchors). Returns the
/// strongest such file per currently-unmatched module. Accepts are already
/// high-precision (globally-unique composite or corroborated), so two of them
/// agreeing on a file is strong, independent evidence.
fn derive_module_promotions(
    binding_rows: &[BindingNameRow],
    module_matched_file: &BTreeMap<u32, String>,
) -> BTreeMap<u32, (String, usize)> {
    let mut counts: BTreeMap<(u32, String), usize> = BTreeMap::new();
    for row in binding_rows {
        if row.accepted {
            *counts
                .entry((row.module_id, row.reference_file.clone()))
                .or_default() += 1;
        }
    }
    let mut best: BTreeMap<u32, (String, usize)> = BTreeMap::new();
    for ((module_id, file), count) in counts {
        if module_matched_file.contains_key(&module_id) {
            continue; // already matched at module level
        }
        match best.get(&module_id) {
            Some((_, existing)) if *existing >= count => {}
            _ => {
                best.insert(module_id, (file, count));
            }
        }
    }
    best.retain(|_, (_, count)| *count >= MODULE_PROMOTE_MIN_FUNCTIONS);
    best
}

/// Apply function-driven module promotions: upgrade an existing Low plan or add
/// a new Medium plan for each promoted module. The synthetic `ModuleMatch`
/// records the promotion via `function_overlap` (the supporting function count).
fn apply_module_promotions(
    plans: &mut Vec<ModulePlan>,
    promotions: &BTreeMap<u32, (String, usize)>,
    subjects: &[SubjectModule],
    index: &ReferenceSourceIndex,
) {
    let existing: BTreeMap<u32, usize> = plans
        .iter()
        .enumerate()
        .map(|(i, p)| (p.module_id, i))
        .collect();
    for (&module_id, (file, count)) in promotions {
        let reference_exports = index
            .modules
            .iter()
            .find(|m| m.file_path == *file)
            .map(|m| m.export_names.clone())
            .unwrap_or_default();
        if let Some(&idx) = existing.get(&module_id) {
            let plan = &mut plans[idx];
            plan.matched.file_path = file.clone();
            plan.matched.tier = MatchTier::Medium;
            plan.matched.function_overlap = *count;
            plan.top_candidate = RankedModuleMatch {
                relevance: *count as f64,
                matched: plan.matched.clone(),
            };
            plan.runner_up = None;
            plan.shared_string_anchors.clear();
            plan.module_semantic_name = strip_source_extension(file);
            plan.reference_exports = reference_exports;
        } else if let Some(subject) = subjects.iter().find(|s| s.module_id == module_id) {
            let synthetic_match = ModuleMatch {
                file_path: file.clone(),
                tier: MatchTier::Medium,
                asset_overlap: 0,
                export_overlap: 0,
                function_overlap: *count,
                top_level_declaration_overlap: 0,
                import_export_surface_overlap: 0,
                class_member_overlap: 0,
                statement_window_overlap: 0,
                block_branch_overlap: 0,
                pq_gram_overlap: 0,
                wl_overlap: 0,
                granular_hash_containment: 0.0,
                statement_window_containment: 0.0,
                block_branch_containment: 0.0,
                structural_score: 0.0,
                graph_support: 0,
                graph_known_edges: 0,
                graph_structure: GraphStructureEvidence::default(),
                anchor_overlap: 0,
                source_score: SourceEvidenceScore::default(),
                weighted_anchor: 0.0,
                normalized_anchor: 0.0,
                margin: 0.0,
                reciprocal_best: false,
            };
            plans.push(ModulePlan {
                module_id,
                subject_path: subject.file_path.clone(),
                reference_version: index.version.clone(),
                module_semantic_name: strip_source_extension(file),
                matched: synthetic_match.clone(),
                top_candidate: RankedModuleMatch {
                    relevance: *count as f64,
                    matched: synthetic_match,
                },
                runner_up: None,
                shared_string_anchors: BTreeSet::new(),
                subject_bindings: subject.bindings.clone(),
                reference_exports,
            });
        }
    }
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
    final_path_by_module: &BTreeMap<u32, String>,
    origin_prefix: &str,
    reference_version: &str,
) -> Result<(usize, usize), CliRunError> {
    crate::commands::binding_names::ensure_binding_names_table_if_writable(connection, true)?;
    let (mut accepted, mut proposed) = (0usize, 0usize);
    for row in rows {
        let binding_key = row.original_name.clone(); // no binding_index -> key on original
        let origin = format!("{origin_prefix}:{reference_version}:{}", row.reference_file);
        let evidence = row.evidence();
        let file_path = final_path_by_module
            .get(&row.module_id)
            .map(String::as_str)
            .unwrap_or(row.subject_path.as_str());
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
                    file_path,
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

fn accepted_module_paths(plans: &[ModulePlan], min_tier: MinTier) -> BTreeMap<u32, String> {
    selected_unique_path_plan_indices(plans, min_tier)
        .into_iter()
        .map(|index| {
            (
                plans[index].module_id,
                plans[index].matched.file_path.clone(),
            )
        })
        .collect()
}

fn selected_unique_path_plan_indices(plans: &[ModulePlan], min_tier: MinTier) -> BTreeSet<usize> {
    let mut selected = BTreeSet::new();
    let mut candidates_by_path = BTreeMap::<&str, Vec<usize>>::new();
    for (index, plan) in plans.iter().enumerate() {
        if tier_passes(plan.matched.tier, min_tier) {
            candidates_by_path
                .entry(plan.matched.file_path.as_str())
                .or_default()
                .push(index);
        }
    }
    for candidates in candidates_by_path.values_mut() {
        candidates.sort_by(|&left, &right| {
            let left_match = &plans[left].matched;
            let right_match = &plans[right].matched;
            right_match
                .reciprocal_best
                .cmp(&left_match.reciprocal_best)
                .then(tier_rank(right_match.tier).cmp(&tier_rank(left_match.tier)))
                .then(
                    right_match
                        .normalized_anchor
                        .total_cmp(&left_match.normalized_anchor),
                )
                .then(
                    right_match
                        .weighted_anchor
                        .total_cmp(&left_match.weighted_anchor),
                )
                .then(right_match.graph_support.cmp(&left_match.graph_support))
                .then(right_match.margin.total_cmp(&left_match.margin))
                .then(plans[left].module_id.cmp(&plans[right].module_id))
        });
        if let Some(index) = candidates.first() {
            selected.insert(*index);
        }
    }
    selected
}

fn ensure_module_path_overrides_table(connection: &Connection) -> Result<(), CliRunError> {
    connection
        .execute_batch(
            r"
            CREATE TABLE IF NOT EXISTS module_path_overrides (
                project_id INTEGER NOT NULL,
                module_id INTEGER NOT NULL,
                path TEXT NOT NULL,
                origin TEXT NOT NULL,
                evidence TEXT,
                accepted INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (project_id, module_id, origin, path)
            );
            CREATE INDEX IF NOT EXISTS idx_module_path_overrides_project_module
                ON module_path_overrides(project_id, module_id, accepted);
            ",
        )
        .map_err(|error| CliRunError::ReferenceSourceNames(error.to_string()))
}

fn write_module_path_overrides(
    connection: &Connection,
    project_id: u32,
    plans: &[ModulePlan],
    min_tier: MinTier,
    origin_prefix: &str,
    reference_version: &str,
) -> Result<usize, CliRunError> {
    ensure_module_path_overrides_table(connection)?;
    let mut written = 0usize;
    let selected = selected_unique_path_plan_indices(plans, min_tier);
    for index in selected {
        let plan = &plans[index];
        if !tier_passes(plan.matched.tier, min_tier) {
            continue;
        }
        let origin = format!(
            "{origin_prefix}:{reference_version}:{}",
            plan.matched.file_path
        );
        validate_module_path_acceptance(plan.matched.file_path.as_str(), origin.as_str())
            .map_err(|error| CliRunError::ReferenceSourceNames(error.message()))?;
        let evidence = format!(
            "{{\"tier\":\"{}\",\"anchor\":{},\"weighted_anchor\":{:.3},\"normalized_anchor\":{:.3},\"asset\":{},\"export\":{},\"function\":{},\"structural\":{:.3},\"graph\":{},\"reciprocal\":{}}}",
            tier_str(plan.matched.tier),
            plan.matched.anchor_overlap,
            plan.matched.weighted_anchor,
            plan.matched.normalized_anchor,
            plan.matched.asset_overlap,
            plan.matched.export_overlap,
            plan.matched.function_overlap,
            plan.matched.structural_score,
            plan.matched.graph_support,
            if plan.matched.reciprocal_best {
                "true"
            } else {
                "false"
            },
        );
        written += connection
            .execute(
                r"
                INSERT INTO module_path_overrides (
                    project_id, module_id, path, origin, evidence, accepted, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, 1, datetime('now'), datetime('now'))
                ON CONFLICT(project_id, module_id, origin, path) DO UPDATE SET
                    evidence = excluded.evidence,
                    accepted = excluded.accepted,
                    updated_at = datetime('now')
                ",
                params![
                    i64::from(project_id),
                    i64::from(plan.module_id),
                    plan.matched.file_path,
                    origin,
                    evidence,
                ],
            )
            .map_err(|error| CliRunError::ReferenceSourceNames(error.to_string()))?;
    }
    Ok(written)
}

// ---------------------------------------------------------------------------
// Ownership-driven package naming
//
// The package matcher establishes module->package@version "ownership" matches
// that it cannot safely externalize (the inlined esbuild bundle does not prove a
// clean single external import), so the attribution is persisted as `rejected`
// and the ownership evidence is otherwise discarded. Those modules stay inlined
// and decompiled, but they ARE the published source of a known package — so the
// package source file is an authoritative naming reference for the module's
// minified functions. This pass loads each owned module's matched package source
// from the global cache, matches bundle functions against it with the same
// engine `reference-source-names` uses, and writes recovered names. It is
// completely independent of externalization: a module that can never be turned
// into an `import` still gets `useState`/`parseSemVer`-grade names.
// ---------------------------------------------------------------------------

/// Request for [`run_ownership_source_names`].
#[derive(Debug, Clone)]
pub(crate) struct OwnershipNamingRequest {
    /// Per-run input database (holds `package_attributions` + the bundle).
    pub input: String,
    pub project_id: u32,
    /// Global package source cache database (`~/.reverts/.reverts.db`).
    pub cache_db: PathBuf,
    /// Write recovered names when true; otherwise dry-run summary only.
    pub apply: bool,
    /// Non-automated origin prefix so the vocabulary gate stays bypassed and
    /// domain names like `parseSemVer` are accepted on identifier evidence.
    pub origin_prefix: String,
}

/// One module's package-ownership match, parsed from `package_attributions`.
#[derive(Debug, Clone)]
struct OwnershipMatch {
    module_id: u32,
    package_name: String,
    package_version: String,
    /// Package-relative source file path (e.g. `cjs/react.development.js`), when
    /// the match pinned a specific source file; `None` for package-only matches.
    entry_path: Option<String>,
}

/// Build [`ReferenceFunction`]s from a single package source file. Mirrors the
/// per-module body of [`collect_reference_functions`] for one `(file, source)`.
fn reference_functions_from_source(file: &str, source: &str) -> Vec<ReferenceFunction> {
    let names: BTreeMap<reverts_ir::ByteRange, String> = function_names(source)
        .into_iter()
        .filter(|(_, name)| is_specific_reference_name(name))
        .collect();
    let mut literals = function_anchor_tokens(source);
    let mut callees = function_callee_names(source);
    let mut references = function_referenced_names(source);
    let mut out = Vec::new();
    for fingerprint in FunctionExtractor::fingerprint_primary(ModuleId(0), source) {
        if let Some(name) = names.get(&fingerprint.id.span) {
            let function_literals = literals.remove(&fingerprint.id.span).unwrap_or_default();
            let function_callees = callees.remove(&fingerprint.id.span).unwrap_or_default();
            let function_references = references.remove(&fingerprint.id.span).unwrap_or_default();
            out.push(ReferenceFunction {
                file: file.to_string(),
                name: name.clone(),
                fingerprint,
                literals: function_literals,
                callees: function_callees,
                references: function_references,
            });
        }
    }
    out
}

/// Recover the package-relative entry path from a matcher `source_path`.
///
/// The matcher decorates the path, e.g.
/// `anonymous-function-axis-source:react@19.2.4:react@19.2.4/cjs/react.development.js:score=72:...`
/// (specific file) or `anonymous-function-axis:react@19.2.4:score=43:...`
/// (package-only, no file). Using the separately-known `name`/`version`, locate
/// the `name@version/` marker and read the entry path up to the next `:`.
fn ownership_entry_path(source_path: &str, name: &str, version: &str) -> Option<String> {
    let marker = format!("{name}@{version}/");
    let start = source_path.find(&marker)?;
    let rest = &source_path[start + marker.len()..];
    let entry = rest.split(':').next().unwrap_or(rest).trim();
    if entry.is_empty() {
        None
    } else {
        Some(entry.to_string())
    }
}

/// Read ownership matches (module + package@version + optional source file) from
/// every `package_attributions` row whose evidence carries an `ownership_match`.
fn load_ownership_matches(
    connection: &Connection,
    project_id: u32,
) -> Result<Vec<OwnershipMatch>, CliRunError> {
    let map_err = |error: rusqlite::Error| CliRunError::ReferenceSourceNames(error.to_string());
    let _ = project_id; // package_attributions is per-run, not project-scoped.
    let mut statement = connection
        .prepare(
            "SELECT module_id, evidence_json FROM package_attributions \
              WHERE evidence_json LIKE '%\"ownership_match\"%'",
        )
        .map_err(map_err)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(map_err)?;
    let mut matches = Vec::new();
    for row in rows {
        let (module_id, evidence_json) = row.map_err(map_err)?;
        let Ok(evidence) = serde_json::from_str::<serde_json::Value>(&evidence_json) else {
            continue;
        };
        let Some(ownership) = evidence.get("ownership_match") else {
            continue;
        };
        let (Some(package_name), Some(package_version)) = (
            ownership.get("package_name").and_then(|v| v.as_str()),
            ownership.get("package_version").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        if package_name.trim().is_empty() || package_version.trim().is_empty() {
            continue;
        }
        let entry_path = ownership
            .get("source_path")
            .and_then(|v| v.as_str())
            .and_then(|path| ownership_entry_path(path, package_name, package_version));
        matches.push(OwnershipMatch {
            module_id: u32::try_from(module_id).unwrap_or_default(),
            package_name: package_name.to_string(),
            package_version: package_version.to_string(),
            entry_path,
        });
    }
    Ok(matches)
}

/// Distinct cached versions for a package, parsed to semver for resolution and
/// paired with their raw cache strings for exact lookup.
fn cached_package_versions(
    cache: &Connection,
    package_name: &str,
) -> Result<Vec<(semver::Version, String)>, CliRunError> {
    let map_err = |error: rusqlite::Error| CliRunError::ReferenceSourceNames(error.to_string());
    let mut statement = cache
        .prepare(
            "SELECT DISTINCT package_version FROM package_source_cache \
              WHERE package_name = ?1 AND TRIM(COALESCE(package_version, '')) != ''",
        )
        .map_err(map_err)?;
    let rows = statement
        .query_map(params![package_name], |row| row.get::<_, String>(0))
        .map_err(map_err)?;
    let mut versions = Vec::new();
    for row in rows {
        let raw = row.map_err(map_err)?;
        if let Ok(parsed) = raw.parse::<semver::Version>() {
            versions.push((parsed, raw));
        }
    }
    Ok(versions)
}

/// Cache rows `(entry_path, source_content)` for one resolved package version.
fn cached_package_files(
    cache: &Connection,
    package_name: &str,
    package_version: &str,
) -> Result<Vec<(String, String)>, CliRunError> {
    let map_err = |error: rusqlite::Error| CliRunError::ReferenceSourceNames(error.to_string());
    let mut statement = cache
        .prepare(
            "SELECT entry_path, source_content FROM package_source_cache \
              WHERE package_name = ?1 AND package_version = ?2 \
                AND TRIM(COALESCE(entry_path, '')) != ''",
        )
        .map_err(map_err)?;
    let rows = statement
        .query_map(params![package_name, package_version], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(map_err)?;
    let mut files = Vec::new();
    for row in rows {
        files.push(row.map_err(map_err)?);
    }
    Ok(files)
}

/// Largest single package source file to fingerprint. Guards against pathological
/// cost on giant bundled entry points (e.g. the TypeScript compiler) while
/// keeping every ordinary module file.
const MAX_OWNERSHIP_SOURCE_BYTES: usize = 2 * 1024 * 1024;

/// `(package, requested_version) -> resolved cache version` for the owned
/// packages, so callers can reconstruct each module's reference-file key.
type ResolvedPackageVersions = BTreeMap<(String, String), String>;

/// Loaded ownership reference corpus: reference functions, per-file source text
/// (keyed by `{name}@{version}/{entry}`), and resolved package versions.
type OwnershipReferenceCorpus = (
    Vec<ReferenceFunction>,
    BTreeMap<String, String>,
    ResolvedPackageVersions,
);

/// Build the reference-function corpus for every owned package, resolving each
/// requested version to the best available cached version. Returns the corpus
/// plus the resolved-version map.
fn load_ownership_reference_corpus(
    cache: &Connection,
    owned: &[OwnershipMatch],
) -> Result<OwnershipReferenceCorpus, CliRunError> {
    let mut requested: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for owner in owned {
        requested
            .entry(owner.package_name.clone())
            .or_default()
            .insert(owner.package_version.clone());
    }

    let mut reference_fns = Vec::new();
    let mut source_by_file: BTreeMap<String, String> = BTreeMap::new();
    let mut resolved_by_request: ResolvedPackageVersions = BTreeMap::new();
    let mut loaded: BTreeSet<(String, String)> = BTreeSet::new();

    for (package_name, requested_versions) in &requested {
        let available = cached_package_versions(cache, package_name)?;
        if available.is_empty() {
            continue;
        }
        let version_set: BTreeSet<semver::Version> = available
            .iter()
            .map(|(version, _)| version.clone())
            .collect();
        for requested_version in requested_versions {
            let Some(resolved) =
                best_matching_package_version_by_binary_search(requested_version, &version_set)
            else {
                continue;
            };
            let Some((_, resolved_raw)) =
                available.iter().find(|(version, _)| version == &resolved)
            else {
                continue;
            };
            resolved_by_request.insert(
                (package_name.clone(), requested_version.clone()),
                resolved_raw.clone(),
            );
            if !loaded.insert((package_name.clone(), resolved_raw.clone())) {
                continue;
            }
            for (entry_path, source) in cached_package_files(cache, package_name, resolved_raw)? {
                if source.len() > MAX_OWNERSHIP_SOURCE_BYTES {
                    continue;
                }
                let file_key = format!("{package_name}@{resolved_raw}/{entry_path}");
                reference_fns.extend(reference_functions_from_source(&file_key, &source));
                source_by_file.insert(file_key, source);
            }
        }
    }
    Ok((reference_fns, source_by_file, resolved_by_request))
}

pub(crate) fn run_ownership_source_names(
    request: &OwnershipNamingRequest,
) -> Result<(), CliRunError> {
    let connection = Connection::open(&request.input)
        .map_err(|error| CliRunError::ReferenceSourceNames(error.to_string()))?;
    let owned = load_ownership_matches(&connection, request.project_id)?;
    if owned.is_empty() {
        println!(
            "ownership-source-names: no ownership matches in {}",
            request.input
        );
        return Ok(());
    }
    let target_modules: BTreeSet<u32> = owned.iter().map(|owner| owner.module_id).collect();
    let package_count = owned
        .iter()
        .map(|owner| (owner.package_name.as_str(), owner.package_version.as_str()))
        .collect::<BTreeSet<_>>()
        .len();

    let cache = Connection::open(&request.cache_db).map_err(|error| {
        CliRunError::ReferenceSourceNames(format!(
            "open package cache {}: {error}",
            request.cache_db.display()
        ))
    })?;
    let (reference_fns, source_by_file, resolved_by_request) =
        load_ownership_reference_corpus(&cache, &owned)?;

    // Constrain the high-precision module-corroborated / within-pair passes to the
    // specific source file the matcher pinned (using the resolved cache version);
    // package-only matches fall through to the globally-unique-composite pass.
    let mut module_matched_file: BTreeMap<u32, String> = BTreeMap::new();
    for owner in &owned {
        let Some(entry_path) = owner.entry_path.as_deref() else {
            continue;
        };
        let key = (owner.package_name.clone(), owner.package_version.clone());
        let Some(resolved_raw) = resolved_by_request.get(&key) else {
            continue;
        };
        module_matched_file.insert(
            owner.module_id,
            format!("{}@{resolved_raw}/{entry_path}", owner.package_name),
        );
    }

    let bundle =
        load_project_bundle_with_package_externalization(&request.input, request.project_id)
            .map_err(|error| CliRunError::ReferenceSourceNames(format!("load input: {error}")))?;
    let (subjects, _island_source) =
        generate_subject_modules(bundle, |module_id| target_modules.contains(&module_id))?;
    let subject_fns = collect_subject_functions(&subjects);
    let mut binding_rows = match_function_lists(&subject_fns, &reference_fns, &module_matched_file);

    // Symbol propagation: for each accepted isomorphic function pair, lockstep-
    // align the bundle function's identifiers with the package-source function's
    // and carry the real names onto the module-level internal symbols they bind.
    // This extends naming from the function track to internal symbols.
    let reference_source: BTreeMap<&str, &str> = source_by_file
        .iter()
        .map(|(file, source)| (file.as_str(), source.as_str()))
        .collect();
    let propagated = propagate_symbols(
        &subjects,
        &reference_source,
        &subject_fns,
        &reference_fns,
        &binding_rows,
    );
    binding_rows.extend(propagated);
    propagate_by_reference_topology(&subject_fns, &reference_fns, &mut binding_rows);
    drop_real_name_remaps(&mut binding_rows);

    let accepted = binding_rows.iter().filter(|row| row.accepted).count();
    let proposed = binding_rows.len() - accepted;

    if request.apply {
        let final_path_by_module: BTreeMap<u32, String> = subjects
            .iter()
            .map(|subject| (subject.module_id, subject.file_path.clone()))
            .collect();
        let (written_accepted, written_proposed) = write_binding_names(
            &connection,
            request.project_id,
            &binding_rows,
            &final_path_by_module,
            &request.origin_prefix,
            "owned",
        )?;
        println!(
            "applied ownership-source-names: {written_accepted} accepted, {written_proposed} proposal(s) across {} owned module(s) / {package_count} package(s)",
            target_modules.len()
        );
    } else {
        println!(
            "dry-run ownership-source-names: {} owned module(s), {package_count} package(s), {} subject module(s), {} reference fn(s); {accepted} accepted name(s), {proposed} proposal(s); pass --apply to write",
            target_modules.len(),
            subjects.len(),
            reference_fns.len(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reverts_input::{ModuleInput, ProjectInput, SourceFileInput, SourceSpan};
    use reverts_ir::ModuleId;
    use reverts_package_matcher::fingerprint_source;

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
    fn module_only_subjects_reuse_prepared_input_slices_without_generation() {
        let source = r#"var E=(A,Q)=>()=>(A&&(Q=A(A=0)),Q);
var localValue,initFeature=E(()=>{localValue="distinct-anchor";});"#;
        let mut rows = InputRows::new(ProjectInput::new(1, "fixture"));
        rows.source_files
            .push(SourceFileInput::new(1, "cli.js", Some(source.to_string())));
        rows.modules.push(
            ModuleInput::application(ModuleId(1), "cli.js", "cli")
                .with_source_file(1)
                .with_source_span(SourceSpan::new(0, source.len() as u32)),
        );

        let prepared = prepare_input_rows_for_pipeline(rows);
        let subjects = subject_modules_from_prepared_rows(&prepared.rows, &BTreeSet::new());

        assert!(
            subjects.iter().any(|subject| {
                subject.module_id == 1
                    && subject.source.contains("distinct-anchor")
                    && !subject.source.contains("var E=")
            }),
            "module-only matching must consume reverts-bundle's prepared module slice, got {subjects:#?}"
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
            top_level_declaration_hashes: BTreeSet::new(),
            import_export_surface_hashes: BTreeSet::new(),
            class_member_hashes: BTreeSet::new(),
            statement_window_hashes: BTreeSet::new(),
            block_branch_hashes: BTreeSet::new(),
            pq_gram_hashes: BTreeSet::new(),
            wl_hashes: BTreeSet::new(),
            string_anchors: anchors.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn test_profile(path: &str, fingerprint: SourceFingerprint) -> SourceEvidenceProfile {
        SourceEvidenceProfile {
            path: path.to_string(),
            fingerprint,
            function_axis_anchors: BTreeSet::new(),
            jsx_react_shape_anchors: BTreeSet::new(),
            anchor_cooccurrence_anchors: BTreeSet::new(),
        }
    }

    fn refmod(path: &str, anchors: &[&str]) -> ReferenceSourceModule {
        let fingerprint = fp(anchors);
        ReferenceSourceModule {
            file_path: path.to_string(),
            source: String::new(),
            profile: test_profile(path, fingerprint.clone()),
            fingerprint,
            export_names: BTreeSet::new(),
            asset_literals: BTreeSet::new(),
        }
    }

    fn test_index(modules: Vec<ReferenceSourceModule>) -> ReferenceSourceIndex {
        let evidence_idf = source_evidence_idf(modules.iter().map(|module| &module.profile));
        ReferenceSourceIndex {
            version: "t".to_string(),
            anchor_idf: compute_anchor_idf(&modules),
            evidence_idf,
            candidate_index: build_candidate_index(&modules),
            modules,
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
                let fingerprint = fp(&["hubtoken", "filler-token"]);
                ReferenceSourceModule {
                    file_path: p,
                    source: String::new(),
                    profile: test_profile("hub.ts", fingerprint.clone()),
                    fingerprint,
                    export_names: BTreeSet::new(),
                    asset_literals: BTreeSet::new(),
                }
            })
            .collect();
        modules.push(refmod("distinctive.ts", &rare_refs));
        let index = test_index(modules);

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
            let fingerprint = fp(&[anchor.as_str()]);
            modules.push(ReferenceSourceModule {
                file_path: path,
                source: String::new(),
                profile: test_profile("filler.ts", fingerprint.clone()),
                fingerprint,
                export_names: BTreeSet::new(),
                asset_literals: BTreeSet::new(),
            });
        }
        let index = test_index(modules);
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
    fn reciprocal_rare_anchor_match_promotes_large_split_module() {
        assert_eq!(
            calibrate_tier(
                MatchTier::Low,
                0.05,
                true,
                SourceEvidenceScore::default(),
                25.0,
                0.12,
            ),
            MatchTier::Medium,
            "reciprocal-best plus substantial rare-anchor mass should recover large split modules"
        );
        assert_eq!(
            calibrate_tier(
                MatchTier::Low,
                1.0,
                false,
                SourceEvidenceScore::default(),
                25.0,
                0.12,
            ),
            MatchTier::Low,
            "the reciprocal-best guard prevents raw rare-anchor mass from promoting hub-like matches"
        );
        assert_eq!(
            calibrate_tier(
                MatchTier::Low,
                0.05,
                true,
                SourceEvidenceScore::default(),
                25.0,
                0.03,
            ),
            MatchTier::Low,
            "reciprocal matches still need a non-trivial normalized overlap"
        );
    }

    #[test]
    fn fn_export_overlap_needs_anchor_corroboration_for_medium() {
        let base = MatchEvidence {
            hash_match: false,
            asset_overlap: 0,
            export_overlap: 0,
            function_overlap: 2,
            top_level_declaration_overlap: 0,
            import_export_surface_overlap: 0,
            class_member_overlap: 0,
            statement_window_overlap: 0,
            block_branch_overlap: 0,
            pq_gram_overlap: 0,
            wl_overlap: 0,
            source_score: SourceEvidenceScore::default(),
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

    #[test]
    fn structural_hub_does_not_outrank_content_match_in_ranking() {
        // Regression: large hub files (e.g. services/mcp/client.ts) accumulated
        // raw-count structural overlaps + containment ~1.0 and DISPLACED genuine
        // content matches in candidate ranking (161 of 166 lost modules). A
        // content candidate must out-rank a structural-only hub, and an
        // uncorroborated hub's structural terms must not score at all.
        let hub = MatchEvidence {
            hash_match: false,
            asset_overlap: 0,
            export_overlap: 0,
            function_overlap: 0,
            top_level_declaration_overlap: 20,
            import_export_surface_overlap: 20,
            class_member_overlap: 20,
            statement_window_overlap: 20,
            block_branch_overlap: 20,
            pq_gram_overlap: 20,
            wl_overlap: 20,
            source_score: SourceEvidenceScore {
                function_axis_containment: 1.0,
                ..SourceEvidenceScore::default()
            },
            structural_score: 0.6,
            graph: GraphEvidence::default(),
            weighted_anchor: 0.0, // NO content corroboration
            normalized_anchor: 0.0,
        };
        let content = MatchEvidence {
            hash_match: false,
            asset_overlap: 0,
            export_overlap: 0,
            function_overlap: 0,
            top_level_declaration_overlap: 0,
            import_export_surface_overlap: 0,
            class_member_overlap: 0,
            statement_window_overlap: 0,
            block_branch_overlap: 0,
            pq_gram_overlap: 0,
            wl_overlap: 0,
            source_score: SourceEvidenceScore::default(),
            structural_score: 0.0,
            graph: GraphEvidence::default(),
            weighted_anchor: 12.0,
            normalized_anchor: 0.35, // genuine content overlap
        };
        assert_eq!(
            candidate_relevance(hub),
            0.0,
            "uncorroborated hub structural terms must not score"
        );
        assert!(
            candidate_relevance(content) > candidate_relevance(hub),
            "content match must out-rank structural-only hub"
        );
    }

    #[test]
    fn fingerprint_structural_score_uses_multigranularity_axes() {
        let mut subject = fp(&[]);
        subject.statement_window_hashes = BTreeSet::from(["stmt-a".to_string()]);
        subject.block_branch_hashes = BTreeSet::from(["block-a".to_string()]);
        subject.pq_gram_hashes = BTreeSet::from(["pq-a".to_string()]);
        subject.wl_hashes = BTreeSet::from(["wl-a".to_string()]);

        let mut reference = fp(&[]);
        reference.statement_window_hashes = BTreeSet::from(["stmt-a".to_string()]);
        reference.block_branch_hashes = BTreeSet::from(["block-a".to_string()]);
        reference.pq_gram_hashes = BTreeSet::from(["pq-a".to_string()]);
        reference.wl_hashes = BTreeSet::from(["wl-a".to_string()]);

        let evidence = MatchEvidence {
            hash_match: false,
            asset_overlap: 0,
            export_overlap: 0,
            function_overlap: 0,
            top_level_declaration_overlap: 0,
            import_export_surface_overlap: 0,
            class_member_overlap: 0,
            statement_window_overlap: 1,
            block_branch_overlap: 1,
            pq_gram_overlap: 1,
            wl_overlap: 1,
            source_score: SourceEvidenceScore::default(),
            structural_score: 0.0,
            graph: GraphEvidence::default(),
            weighted_anchor: 0.0,
            normalized_anchor: 0.0,
        };

        assert_eq!(
            source_fingerprint_structural_score(&subject, &reference, evidence),
            1.0,
            "identical structural axes should normalize to full support"
        );

        let mut large_reference = reference;
        large_reference
            .statement_window_hashes
            .extend((0..9).map(|index| format!("stmt-ref-only-{index}")));
        assert!(
            source_fingerprint_structural_score(&subject, &large_reference, evidence) < 1.0,
            "normalization should penalize partial coverage of a larger reference"
        );
    }

    #[test]
    fn structural_delta_promotes_only_with_content_corroboration() {
        let mut top = make_plan(30, "features/structural-top", MatchTier::Low).matched;
        top.margin = AMBIGUOUS_PROMOTION_MIN_MARGIN;
        top.structural_score = MEDIUM_STRUCTURAL_SCORE + 0.05;
        top.statement_window_overlap = 3;
        top.normalized_anchor = AMBIGUOUS_PROMOTION_MIN_NANCHOR;

        let mut runner_up = make_plan(31, "features/structural-runner", MatchTier::Low).matched;
        runner_up.structural_score =
            top.structural_score - AMBIGUOUS_PROMOTION_STRUCTURAL_DELTA - 0.01;
        runner_up.statement_window_overlap = 3;

        assert!(
            guarded_ambiguous_promotion(&top, &runner_up),
            "clear structural separation plus content should recover ambiguous near matches"
        );

        let mut no_content = top.clone();
        no_content.normalized_anchor = 0.0;
        no_content.weighted_anchor = 0.0;
        no_content.source_score = SourceEvidenceScore::default();
        no_content.graph_support = 0;
        assert!(
            !guarded_ambiguous_promotion(&no_content, &runner_up),
            "structural shape alone is too generic to promote"
        );
    }

    #[test]
    fn function_axis_delta_requires_independent_corroboration() {
        let mut top = make_plan(32, "features/function-top", MatchTier::Low).matched;
        top.margin = AMBIGUOUS_PROMOTION_MIN_MARGIN;
        top.source_score.function_axis_overlap = 20;
        top.source_score.function_axis_containment = 0.35;
        top.source_score.function_axis_jaccard = 0.12;
        top.structural_score = 0.09;

        let mut runner_up = make_plan(33, "features/function-runner", MatchTier::Low).matched;
        runner_up.source_score.function_axis_overlap =
            top.source_score.function_axis_overlap - AMBIGUOUS_PROMOTION_FUNCTION_AXIS_DELTA;
        runner_up.source_score.function_axis_jaccard = 0.08;

        assert!(
            guarded_ambiguous_promotion(&top, &runner_up),
            "clear function-axis separation should promote only when another axis corroborates"
        );

        let mut no_corroboration = top.clone();
        no_corroboration.structural_score = 0.0;
        no_corroboration.normalized_anchor = 0.0;
        no_corroboration.weighted_anchor = 0.0;
        no_corroboration.source_score.unique_string_anchor_overlap = 0;
        no_corroboration.source_score.anchor_cooccurrence_overlap = 0;
        assert!(!guarded_ambiguous_promotion(&no_corroboration, &runner_up));
    }

    #[test]
    fn high_cooccurrence_mass_is_split_support_not_standalone_tier() {
        let mut matched = make_plan(34, "features/cooccur", MatchTier::Low).matched;
        matched.source_score.anchor_cooccurrence_overlap = 8;
        matched.source_score.anchor_cooccurrence_jaccard = 0.04;
        matched.normalized_anchor = 0.12;
        matched.margin = MEDIUM_SCORE_MARGIN;
        assert!(has_high_cooccurrence_source_mass(&matched));

        let evidence = MatchEvidence {
            hash_match: false,
            asset_overlap: 0,
            export_overlap: 0,
            function_overlap: 0,
            top_level_declaration_overlap: 0,
            import_export_surface_overlap: 0,
            class_member_overlap: 0,
            statement_window_overlap: 0,
            block_branch_overlap: 0,
            pq_gram_overlap: 0,
            wl_overlap: 0,
            source_score: matched.source_score,
            structural_score: 0.0,
            graph: GraphEvidence::default(),
            weighted_anchor: 0.0,
            normalized_anchor: 0.0,
        };
        assert_eq!(
            raw_match_tier(evidence),
            MatchTier::Low,
            "co-occurrence alone is not a direct Medium-tier proof"
        );
    }

    fn make_plan(module_id: u32, name: &str, tier: MatchTier) -> ModulePlan {
        let matched = ModuleMatch {
            file_path: format!("{name}.ts"),
            tier,
            asset_overlap: if tier == MatchTier::High { 1 } else { 0 },
            export_overlap: 0,
            function_overlap: 0,
            top_level_declaration_overlap: 0,
            import_export_surface_overlap: 0,
            class_member_overlap: 0,
            statement_window_overlap: 0,
            block_branch_overlap: 0,
            pq_gram_overlap: 0,
            wl_overlap: 0,
            granular_hash_containment: 0.0,
            statement_window_containment: 0.0,
            block_branch_containment: 0.0,
            structural_score: 0.0,
            graph_support: 0,
            graph_known_edges: 0,
            graph_structure: GraphStructureEvidence::default(),
            anchor_overlap: 0,
            source_score: SourceEvidenceScore::default(),
            weighted_anchor: 0.0,
            normalized_anchor: 0.0,
            margin: 1.0,
            reciprocal_best: true,
        };
        ModulePlan {
            module_id,
            subject_path: format!("modules/m{module_id}.ts"),
            reference_version: "2.1.76".to_string(),
            module_semantic_name: name.to_string(),
            matched: matched.clone(),
            top_candidate: RankedModuleMatch {
                relevance: 1.0,
                matched,
            },
            runner_up: None,
            shared_string_anchors: BTreeSet::new(),
            subject_bindings: Vec::new(),
            reference_exports: std::collections::BTreeSet::new(),
        }
    }

    #[test]
    fn low_boundary_reason_reports_reciprocal_anchor_shortfalls() {
        let mut plan = make_plan(7, "features/near", MatchTier::Low);
        plan.matched.reciprocal_best = true;
        plan.matched.weighted_anchor = MEDIUM_RECIPROCAL_WEIGHTED_ANCHOR + 1.0;
        plan.matched.normalized_anchor = MEDIUM_RECIPROCAL_NORMALIZED_ANCHOR / 2.0;

        assert_eq!(
            low_boundary_reason(&plan.matched),
            "reciprocal_normalized_anchor_shortfall"
        );
        assert!(low_medium_closeness(&plan.matched) >= 0.5);
    }

    #[test]
    fn low_boundary_reason_reports_content_floor_shortfall() {
        let mut plan = make_plan(8, "features/content", MatchTier::Low);
        plan.matched.reciprocal_best = false;
        plan.matched.function_overlap = 2;
        plan.matched.normalized_anchor = 0.0;

        assert_eq!(
            low_boundary_reason(&plan.matched),
            "content_floor_shortfall"
        );
    }

    #[test]
    fn guarded_near_strong_content_promotes_only_with_margin() {
        assert_eq!(
            calibrate_tier(
                MatchTier::Low,
                MEDIUM_SCORE_MARGIN,
                false,
                SourceEvidenceScore::default(),
                0.0,
                MEDIUM_GUARDED_STRONG_NANCHOR,
            ),
            MatchTier::Medium,
            "near-strong normalized overlap is accepted when runner-up separation is clear"
        );
        assert_eq!(
            calibrate_tier(
                MatchTier::Low,
                MEDIUM_SCORE_MARGIN / 2.0,
                false,
                SourceEvidenceScore::default(),
                MEDIUM_WEIGHTED_ANCHOR,
                MEDIUM_GUARDED_STRONG_NANCHOR,
            ),
            MatchTier::Low,
            "near-strong content remains Low when the runner-up is too close"
        );
    }

    #[test]
    fn sourced_near_strong_content_requires_independent_source_evidence() {
        assert_eq!(
            calibrate_tier(
                MatchTier::Low,
                MEDIUM_SCORE_MARGIN,
                false,
                SourceEvidenceScore {
                    unique_string_anchor_overlap: 1,
                    ..SourceEvidenceScore::default()
                },
                0.0,
                MEDIUM_SOURCED_GUARDED_STRONG_NANCHOR,
            ),
            MatchTier::Medium,
            "unique string source evidence can corroborate the sourced near-strong band"
        );
        assert_eq!(
            calibrate_tier(
                MatchTier::Low,
                MEDIUM_SCORE_MARGIN,
                false,
                SourceEvidenceScore::default(),
                MEDIUM_WEIGHTED_ANCHOR,
                MEDIUM_SOURCED_GUARDED_STRONG_NANCHOR,
            ),
            MatchTier::Low,
            "plain string-anchor overlap below the guarded threshold is not enough"
        );
    }

    #[test]
    fn sourced_reciprocal_near_miss_requires_unique_string_evidence() {
        assert_eq!(
            calibrate_tier(
                MatchTier::Low,
                MEDIUM_SCORE_MARGIN,
                true,
                SourceEvidenceScore {
                    unique_string_anchor_overlap: 1,
                    ..SourceEvidenceScore::default()
                },
                MEDIUM_RECIPROCAL_NEAR_WEIGHTED_ANCHOR,
                MEDIUM_RECIPROCAL_NEAR_NORMALIZED_ANCHOR,
            ),
            MatchTier::Medium,
            "reciprocal near misses can promote when unique string evidence corroborates them"
        );
        assert_eq!(
            calibrate_tier(
                MatchTier::Low,
                MEDIUM_SCORE_MARGIN,
                true,
                SourceEvidenceScore::default(),
                MEDIUM_RECIPROCAL_NEAR_WEIGHTED_ANCHOR,
                MEDIUM_RECIPROCAL_NEAR_NORMALIZED_ANCHOR,
            ),
            MatchTier::Low,
            "reciprocal near misses without source corroboration remain Low"
        );
    }

    #[test]
    fn high_unique_anchor_mass_is_only_split_module_support() {
        let mut matched = make_plan(20, "features/high-unique", MatchTier::Medium).matched;
        matched.source_score.unique_string_anchor_overlap = 5;
        matched.weighted_anchor = 80.0;
        matched.normalized_anchor = 0.12;
        matched.margin = MEDIUM_SCORE_MARGIN;
        assert!(
            has_high_unique_anchor_mass(&matched),
            "high unique-anchor mass can support split-module uniqueness retention"
        );

        matched.source_score.unique_string_anchor_overlap = 4;
        assert!(
            !has_high_unique_anchor_mass(&matched),
            "raw anchor mass without enough unique anchors is not split-module support"
        );
    }

    #[test]
    fn split_module_anchor_sets_allow_small_overlap() {
        let left = BTreeSet::from([
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ]);
        let mostly_distinct = BTreeSet::from([
            "a".to_string(),
            "x".to_string(),
            "y".to_string(),
            "z".to_string(),
        ]);
        let overlapping = BTreeSet::from([
            "a".to_string(),
            "b".to_string(),
            "x".to_string(),
            "y".to_string(),
        ]);

        assert!(anchor_sets_cover_distinct_parts(&left, &mostly_distinct));
        assert!(!anchor_sets_cover_distinct_parts(&left, &overlapping));
    }

    #[test]
    fn split_module_cluster_promotes_distinct_low_slice_with_content() {
        let mut seed = make_plan(40, "features/split-source", MatchTier::Medium);
        seed.matched.file_path = "features/split-source.ts".to_string();

        let mut slice = make_plan(41, "features/split-source", MatchTier::Low);
        slice.matched.file_path = "features/split-source.ts".to_string();
        slice.matched.margin = SPLIT_CLUSTER_MIN_MARGIN;
        slice.matched.weighted_anchor = AMBIGUOUS_PROMOTION_MIN_WEIGHTED_ANCHOR;
        slice.matched.normalized_anchor = AMBIGUOUS_PROMOTION_MIN_NANCHOR;
        slice.matched.source_score.unique_string_anchor_overlap = 1;

        let mut no_content = make_plan(42, "features/split-source", MatchTier::Low);
        no_content.matched.file_path = "features/split-source.ts".to_string();
        no_content.matched.margin = SPLIT_CLUSTER_MIN_MARGIN;

        let mut plans = vec![seed, slice, no_content];
        let shared = vec![
            BTreeSet::from(["seed-a".to_string(), "seed-b".to_string()]),
            BTreeSet::from(["slice-a".to_string(), "slice-b".to_string()]),
            BTreeSet::from(["no-content-a".to_string(), "no-content-b".to_string()]),
        ];

        apply_split_module_cluster_promotions(&mut plans, &shared);
        calibrate_global_reference_uniqueness(&mut plans, &shared);

        assert_eq!(
            plans[1].matched.tier,
            MatchTier::Medium,
            "distinct split slice with content should be retained"
        );
        assert_eq!(
            plans[2].matched.tier,
            MatchTier::Low,
            "distinct anchors without content/source corroboration must not promote"
        );
    }

    #[test]
    fn ambiguous_promotion_requires_content_and_runner_up_axis_delta() {
        let mut top = make_plan(10, "features/top", MatchTier::Low).matched;
        top.margin = AMBIGUOUS_PROMOTION_MIN_MARGIN;
        top.normalized_anchor = 0.08;
        top.weighted_anchor = 12.0;
        top.statement_window_overlap = 6;
        top.block_branch_overlap = 4;
        top.wl_overlap = 8;

        let mut runner_up = make_plan(11, "features/runner", MatchTier::Low).matched;
        runner_up.normalized_anchor = 0.02;
        runner_up.weighted_anchor = 4.0;
        runner_up.statement_window_overlap = 2;
        runner_up.block_branch_overlap = 1;
        runner_up.wl_overlap = 3;

        assert!(
            guarded_ambiguous_promotion(&top, &runner_up),
            "content plus clear top-vs-runner-up axis deltas should recover near-medium ambiguous rows"
        );

        let mut no_content = top.clone();
        no_content.normalized_anchor = 0.0;
        no_content.weighted_anchor = 0.0;
        no_content.source_score = SourceEvidenceScore::default();
        assert!(
            !guarded_ambiguous_promotion(&no_content, &runner_up),
            "shape/WL/PQ evidence must not promote without content/source corroboration"
        );

        let mut no_delta = runner_up.clone();
        no_delta.normalized_anchor = top.normalized_anchor;
        no_delta.weighted_anchor = top.weighted_anchor;
        no_delta.statement_window_overlap = top.statement_window_overlap;
        no_delta.block_branch_overlap = top.block_branch_overlap;
        no_delta.wl_overlap = top.wl_overlap;
        assert!(
            !guarded_ambiguous_promotion(&top, &no_delta),
            "ambiguous rows stay Low when the runner-up has equivalent evidence"
        );
    }

    #[test]
    fn region_containment_promotes_only_with_runner_up_delta_and_content() {
        let mut top = make_plan(13, "features/region", MatchTier::Low).matched;
        top.margin = AMBIGUOUS_PROMOTION_MIN_MARGIN;
        top.normalized_anchor = AMBIGUOUS_PROMOTION_MIN_NANCHOR;
        top.granular_hash_containment = 0.80;
        top.statement_window_containment = 0.50;
        top.block_branch_containment = 0.45;

        let mut runner_up = make_plan(14, "features/runner", MatchTier::Low).matched;
        runner_up.granular_hash_containment = 0.45;
        runner_up.statement_window_containment = 0.20;
        runner_up.block_branch_containment = 0.20;

        assert!(
            guarded_ambiguous_promotion(&top, &runner_up),
            "partial reference-region coverage should promote when it clearly beats the runner-up"
        );

        let mut no_content = top.clone();
        no_content.normalized_anchor = 0.0;
        no_content.weighted_anchor = 0.0;
        no_content.source_score = SourceEvidenceScore::default();
        no_content.graph_support = 0;
        assert!(
            !guarded_ambiguous_promotion(&no_content, &runner_up),
            "region containment still needs content/source/graph corroboration"
        );
    }

    #[test]
    fn graph_placement_promotion_requires_anchor_or_content_corroboration() {
        let mut matched = make_plan(12, "features/graph", MatchTier::Low).matched;
        matched.graph_support = 2;
        matched.graph_known_edges = 2;
        matched.weighted_anchor = MEDIUM_STRUCTURAL_WEIGHTED_ANCHOR;
        matched.normalized_anchor = MEDIUM_STRUCTURAL_NORMALIZED_ANCHOR;
        assert!(
            guarded_graph_placement_promotion(&matched),
            "matched graph neighbors plus anchor corroboration should promote"
        );

        let mut graph_only = matched.clone();
        graph_only.weighted_anchor = 0.0;
        graph_only.normalized_anchor = 0.0;
        graph_only.source_score = SourceEvidenceScore::default();
        assert!(
            !guarded_graph_placement_promotion(&graph_only),
            "graph placement without source/content corroboration remains Low"
        );
    }

    #[test]
    fn module_only_planning_builds_graph_structure_without_structural_bags() {
        let subject_a_fp = fp(&["subject-a-rare"]);
        let subject_b_fp = fp(&["subject-b-rare"]);
        let subjects = vec![
            SubjectModule {
                module_id: 1,
                file_path: "modules/a.ts".to_string(),
                source: "import './b'; const value = 'subject-a-rare';".to_string(),
                fingerprint: subject_a_fp.clone(),
                profile: test_profile("modules/a.ts", subject_a_fp),
                dependencies: BTreeSet::new(),
                bindings: Vec::new(),
            },
            SubjectModule {
                module_id: 2,
                file_path: "modules/b.ts".to_string(),
                source: "export const value = 'subject-b-rare';".to_string(),
                fingerprint: subject_b_fp.clone(),
                profile: test_profile("modules/b.ts", subject_b_fp),
                dependencies: BTreeSet::new(),
                bindings: Vec::new(),
            },
        ];
        let ref_a_fp = fp(&["subject-a-rare"]);
        let ref_b_fp = fp(&["subject-b-rare"]);
        let index = test_index(vec![
            ReferenceSourceModule {
                file_path: "src/a.ts".to_string(),
                source: "import './b'; const value = 'subject-a-rare';".to_string(),
                profile: test_profile("src/a.ts", ref_a_fp.clone()),
                fingerprint: ref_a_fp,
                export_names: BTreeSet::new(),
                asset_literals: BTreeSet::new(),
            },
            ReferenceSourceModule {
                file_path: "src/b.ts".to_string(),
                source: "export const value = 'subject-b-rare';".to_string(),
                profile: test_profile("src/b.ts", ref_b_fp.clone()),
                fingerprint: ref_b_fp,
                export_names: BTreeSet::new(),
                asset_literals: BTreeSet::new(),
            },
            refmod("src/filler-one.ts", &["filler-one-rare"]),
            refmod("src/filler-two.ts", &["filler-two-rare"]),
        ]);

        let plans = plan_modules(
            &subjects,
            &index,
            PlanSupportOptions {
                structural_bag: false,
                graph_support: false,
                graph_structure: true,
            },
        )
        .expect("planning should succeed");
        let plan = plans
            .iter()
            .find(|plan| plan.module_id == 1)
            .expect("subject a should be planned");

        assert_eq!(plan.matched.file_path, "src/a.ts");
        assert!(
            plan.matched.graph_structure.subject_has_edges,
            "module-only subject graph should be populated from extracted slice imports"
        );
        assert!(
            plan.matched.graph_structure.reference_has_edges,
            "reference graph should be populated from source imports"
        );
        assert_eq!(plan.matched.graph_structure.subject_out_degree, 1);
        assert_eq!(plan.matched.graph_structure.reference_out_degree, 1);
        assert_eq!(
            plan.matched.graph_known_edges, 1,
            "post-match graph support should expose seeded neighbor coverage in module-only mode"
        );
        assert_eq!(plan.matched.graph_support, 1);
    }

    #[test]
    fn diagnostics_classify_path_families_and_weak_anchors() {
        assert_eq!(
            diagnostic_path_family("services/mcp/client.ts"),
            "services/mcp"
        );
        assert_eq!(diagnostic_path_family("utils/env.ts"), "utils/env.ts");
        assert_eq!(diagnostic_path_family("<none>"), "<none>");
        assert!(is_weak_diagnostic_anchor("function"));
        assert!(is_weak_diagnostic_anchor("object-key:type"));
        assert!(!is_weak_diagnostic_anchor("object-key:access_token"));
    }

    #[test]
    fn dry_run_evaluator_reports_rule_contribution_context() {
        let mut plan = make_plan(70, "schemas/tool-schema", MatchTier::Medium);
        plan.matched.import_export_surface_overlap = 2;
        plan.matched.top_level_declaration_overlap = 1;
        plan.matched.class_member_overlap = 3;
        plan.matched.reciprocal_best = false;
        plan.top_candidate.matched = plan.matched.clone();
        plan.runner_up = Some(RankedModuleMatch {
            relevance: 0.90,
            matched: make_plan(71, "schemas/runner", MatchTier::Low).matched,
        });

        let mut colliding = make_plan(72, "schemas/tool-schema", MatchTier::Low);
        colliding.matched.file_path = plan.matched.file_path.clone();
        let plans = vec![plan, colliding];
        let collision_groups = reference_collision_groups(&plans);
        let low_rows = Vec::new();
        let report = dry_run_evaluator_json(MinTier::Medium, &plans, &low_rows, &collision_groups);

        let contributions = report["accepted_rule_contributions"]
            .as_array()
            .expect("accepted contributions should be an array");
        let surface = contributions
            .iter()
            .find(|row| row["rule"] == "import_export_surface")
            .expect("import/export surface contribution should be reported");
        assert_eq!(surface["module_id"], 70);
        assert_eq!(surface["reference_file"], "schemas/tool-schema.ts");
        assert_eq!(surface["new_rule"], true);
        assert_eq!(surface["baseline_accept_proxy"], false);
        assert_eq!(surface["runner_up_reference"], "schemas/runner.ts");
        assert_eq!(surface["collision_group"]["size"], 2);
    }

    #[test]
    fn graph_structure_profile_hashes_local_role_neighborhood() {
        let outgoing = BTreeMap::from([
            ("subject", BTreeSet::from(["dep"])),
            ("parent", BTreeSet::from(["subject"])),
            ("dep", BTreeSet::new()),
        ]);
        let incoming = BTreeMap::from([
            ("subject", BTreeSet::from(["parent"])),
            ("parent", BTreeSet::new()),
            ("dep", BTreeSet::from(["subject"])),
        ]);
        let profiles = graph_structure_profiles(&outgoing, &incoming);
        let subject = profiles["subject"];

        assert_eq!(
            graph_role_signature(subject.in_degree, subject.out_degree),
            "in:1;out:1"
        );
        assert_ne!(subject.neighborhood_hash, 0);
        assert_ne!(
            subject.neighborhood_hash, profiles["dep"].neighborhood_hash,
            "different graph roles/neighborhoods should not collapse"
        );
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
            write_binding_names(&connection, 1, &rows, &BTreeMap::new(), "source", "2.1.76")
                .expect("write");
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
        let mut lits = function_anchor_tokens(source);
        let mut callees = function_callee_names(source);
        let mut refs = function_referenced_names(source);
        FunctionExtractor::fingerprint(ModuleId(module_id), source)
            .into_iter()
            .filter_map(|f| {
                names.get(&f.id.span).map(|name| SubjectFunction {
                    module_id,
                    subject_path: path.to_string(),
                    name: name.clone(),
                    literals: lits.remove(&f.id.span).unwrap_or_default(),
                    callees: callees.remove(&f.id.span).unwrap_or_default(),
                    references: refs.remove(&f.id.span).unwrap_or_default(),
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
        let mut lits = function_anchor_tokens(source);
        let mut callees = function_callee_names(source);
        let mut refs = function_referenced_names(source);
        FunctionExtractor::fingerprint(ModuleId(0), source)
            .into_iter()
            .filter_map(|f| {
                names.get(&f.id.span).map(|name| ReferenceFunction {
                    file: file.to_string(),
                    name: name.clone(),
                    literals: lits.remove(&f.id.span).unwrap_or_default(),
                    callees: callees.remove(&f.id.span).unwrap_or_default(),
                    references: refs.remove(&f.id.span).unwrap_or_default(),
                    fingerprint: f,
                })
            })
            .collect()
    }

    fn accepted_row(module_id: u32, reference_file: &str, semantic: &str) -> BindingNameRow {
        BindingNameRow {
            module_id,
            subject_path: format!("modules/{module_id}.ts"),
            reference_file: reference_file.to_string(),
            original_name: format!("orig{semantic}"),
            semantic_name: semantic.to_string(),
            accepted: true,
            ast_hash: 1,
            param_count: 0,
            statement_count: 1,
            score: 2.0,
        }
    }

    #[test]
    fn function_track_promotes_unmatched_module_with_two_accepted_functions() {
        let rows = vec![
            accepted_row(7, "util/x.ts", "foo"),
            accepted_row(7, "util/x.ts", "bar"),
            accepted_row(8, "util/y.ts", "baz"), // single accept -> no promotion
        ];
        let promo = derive_module_promotions(&rows, &BTreeMap::new());
        assert_eq!(
            promo.get(&7).map(|(f, c)| (f.as_str(), *c)),
            Some(("util/x.ts", 2))
        );
        assert!(!promo.contains_key(&8), "single accept must not promote");
    }

    #[test]
    fn function_track_does_not_promote_already_matched_module() {
        let rows = vec![
            accepted_row(7, "util/x.ts", "foo"),
            accepted_row(7, "util/x.ts", "bar"),
        ];
        let already = BTreeMap::from([(7u32, "elsewhere.ts".to_string())]);
        let promo = derive_module_promotions(&rows, &already);
        assert!(promo.is_empty(), "matched module must not be re-promoted");
    }

    #[test]
    fn symbol_propagation_names_module_level_referenced_symbols() {
        // Two subject modules each contain a function that is AST-isomorphic to a
        // reference function and references the SAME module-level symbol. Both
        // accepted-function anchors agree the minified `qZ` is `loadConfig` ->
        // accepted by ≥2-vote majority. Locals are excluded.
        let subject_module = |module_id, src: &str| {
            let fp = fingerprint_source("a", src).expect("fp");
            SubjectModule {
                module_id,
                file_path: "modules/a.ts".into(),
                source: src.into(),
                profile: test_profile("modules/a.ts", fp.clone()),
                fingerprint: fp,
                dependencies: BTreeSet::new(),
                bindings: vec![],
            }
        };
        let subjects = vec![
            subject_module(1, "function aB(p){ let t = p; return qZ(t); }"),
            subject_module(1, "function cD(p){ let t = p; return qZ(t); }"),
        ];
        let subject_fns = collect_subject_functions(&subjects);
        // Reference index: one file with two distinct functions referencing loadConfig.
        let index = {
            let temp = tempfile::tempdir().expect("temp");
            std::fs::write(
                temp.path().join("cfg.ts"),
                "function readA(p){ let t = p; return loadConfig(t); } \
                 function readB(p){ let t = p; return loadConfig(t); }",
            )
            .expect("write");
            build_reference_source_index(temp.path(), "v").expect("index")
        };
        let reference_fns = collect_reference_functions(&index);
        // Two accepted function anchors: aB->readA, cD->readB (both isomorphic).
        let binding = |module_id, orig: &str, sem: &str| BindingNameRow {
            module_id,
            subject_path: "modules/a.ts".into(),
            reference_file: "cfg.ts".into(),
            original_name: orig.into(),
            semantic_name: sem.into(),
            accepted: true,
            ast_hash: 1,
            param_count: 1,
            statement_count: 2,
            score: 2.0,
        };
        let rows = vec![binding(1, "aB", "readA"), binding(1, "cD", "readB")];
        let reference_source: BTreeMap<&str, &str> = index
            .modules
            .iter()
            .map(|m| (m.file_path.as_str(), m.source.as_str()))
            .collect();
        let propagated = propagate_symbols(
            &subjects,
            &reference_source,
            &subject_fns,
            &reference_fns,
            &rows,
        );
        let qz = propagated.iter().find(|r| r.original_name == "qZ");
        assert!(qz.is_some(), "qZ should be propagated: {propagated:?}");
        let qz = qz.unwrap();
        assert_eq!(qz.semantic_name, "loadConfig");
        assert!(qz.accepted, "2 consistent votes -> accepted");
        assert!(
            !propagated.iter().any(|r| r.original_name == "t"),
            "local 't' must not be propagated"
        );
    }

    #[test]
    fn function_match_proposes_via_distinctive_inbody_literal() {
        // Subject and reference functions whose bodies DIFFER structurally (no
        // shared AST hash) but share a unique string literal -> the literal
        // anchors a proposal that hash matching alone would miss. The PARAM COUNTS
        // DISAGREE (1 vs 2), so the unique-anchor accept pass (4b) cannot promote it
        // to an accept — it stays a proposal, which is what this test pins.
        let subjects = subject_fn(
            7,
            "modules/m.ts",
            "function aB(x) { if (x) { log(\"uniqueDriftMarker_xyz\"); } return x; }",
        );
        let references = reference_fn(
            "util/drift.ts",
            "function realName(y, z) { return y && z ? emit(\"uniqueDriftMarker_xyz\") : 0; }",
        );
        let rows = match_function_lists(&subjects, &references, &BTreeMap::new());
        assert!(
            rows.iter()
                .any(|r| !r.accepted && r.semantic_name == "realName"),
            "distinctive shared literal should anchor a proposal: {rows:?}"
        );
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
        // match, naming functions in otherwise-unmatched modules. The body is
        // non-trivial (>= MIN_CORROBORATION_FREE_STATEMENTS statements): a unique
        // composite of a trivial single-statement body is too low-entropy to accept
        // corroboration-free, so the fixture must clear that floor to exercise this
        // path.
        let subjects = subject_fn(
            99,
            "modules/unmatched.ts",
            "function aB(x) { let y = x + 1; return y; }",
        );
        let references = reference_fn(
            "util/inc.ts",
            "function increment(x) { let y = x + 1; return y; }",
        );
        let rows = match_function_lists(&subjects, &references, &BTreeMap::new());
        assert!(
            rows.iter()
                .any(|r| r.accepted && r.semantic_name == "increment"),
            "unique composite should accept without corroboration: {rows:?}"
        );
    }

    #[test]
    fn island_mode_accepts_distinctive_anchor_match() {
        // Entrypoint-island case: a function with no module match (empty
        // module_matched_file) but a distinctive 4-anchor set that overlaps exactly
        // one reference function is accepted by the anchor-set-uniqueness pass.
        let body = r#"{ return ["alpha_marker_xyz","beta_marker_xyz","gamma_marker_xyz","delta_marker_xyz"][x]; }"#;
        let subjects = subject_fn(
            ISLAND_MODULE_ID,
            ENTRYPOINT_ISLAND_PATH,
            &format!("function aB(x) {body}"),
        );
        let references = reference_fn(
            "utils/markers.ts",
            &format!("function pickMarker(x) {body}"),
        );
        let rows = match_function_lists_inner(&subjects, &references, &BTreeMap::new(), true);
        let accept = rows.iter().find(|r| r.accepted);
        assert!(
            accept.is_some_and(|r| r.semantic_name == "pickMarker"),
            "distinctive anchor set should accept island function: {rows:?}"
        );
    }

    #[test]
    fn island_mode_keeps_unique_composite_pass() {
        // A globally-unique composite with exact param/stmt agreement (pass 0)
        // carries a distinctive per-function signature, so island_mode KEEPS it —
        // this is the bulk of island recall. No module match, no anchors needed.
        let subjects = subject_fn(
            ISLAND_MODULE_ID,
            ENTRYPOINT_ISLAND_PATH,
            "function aB(x) { let y = x + 1; return y; }",
        );
        let references = reference_fn(
            "util/inc.ts",
            "function increment(x) { let y = x + 1; return y; }",
        );
        assert!(
            match_function_lists_inner(&subjects, &references, &BTreeMap::new(), true)
                .iter()
                .any(|r| r.accepted && r.semantic_name == "increment"),
            "island_mode must keep the unique-composite pass"
        );
    }

    #[test]
    fn island_mode_disables_global_structural_pass() {
        // Two subject functions share the SAME shape, so the composite is NOT
        // globally unique (pass 0 cannot fire) and there are no anchors (passes 3/4
        // cannot fire). The ONLY pass that can accept is pass 5 (graded within-pair
        // similarity on the shared AST hash). The full pass set accepts via pass 5;
        // island_mode disables it, so a global island match must NOT accept — the
        // guarantee that island naming never rests on unvalidatable graded structure.
        let subjects = subject_fn(
            ISLAND_MODULE_ID,
            ENTRYPOINT_ISLAND_PATH,
            "function aB(x) { let y = x * 2; let z = y + 1; return z; } \
             function cD(x) { let y = x * 2; let z = y + 1; return z; }",
        );
        let references = reference_fn(
            "util/calc.ts",
            "function compute(x) { let y = x * 2; let z = y + 1; return z; }",
        );
        // Full pass set accepts (pass 5 graded within-pair on the shared AST hash).
        assert!(
            match_function_lists(&subjects, &references, &BTreeMap::new())
                .iter()
                .any(|r| r.accepted && r.semantic_name == "compute"),
            "full pass set should accept via the global structural pass"
        );
        // island_mode disables pass 5/6, and pass 0 cannot fire (composite shared by
        // aB/cD, not globally unique), so nothing is accepted.
        assert!(
            !match_function_lists_inner(&subjects, &references, &BTreeMap::new(), true)
                .iter()
                .any(|r| r.accepted),
            "island_mode must not accept via graded structural similarity"
        );
    }

    #[test]
    fn unique_anchor_with_exact_arity_promotes_drifted_function() {
        // Bodies differ structurally (different statements -> different AST hash, so
        // pass 4 cannot fire) but share ONE globally-unique reference literal and
        // agree on param + statement counts. Pass 4b promotes it.
        let subjects = subject_fn(
            7,
            "modules/m.ts",
            r#"function aB(x){ let q = "kSliceMarkerZQX_unique"; return q + x; }"#,
        );
        let references = reference_fn(
            "util/slice.ts",
            r#"function sliceWithMarker(x){ let q = "kSliceMarkerZQX_unique"; return x + q; }"#,
        );
        let rows = match_function_lists(&subjects, &references, &BTreeMap::new());
        assert!(
            rows.iter()
                .any(|r| r.accepted && r.semantic_name == "sliceWithMarker"),
            "unique anchor + exact arity/stmt should promote: {rows:?}"
        );
    }

    #[test]
    fn unique_anchor_tolerates_one_statement_drift() {
        // Bodies drifted (subject has one extra statement) but share the decisive
        // globally-unique anchor and agree on param count -> still promoted (±1 stmt).
        let subjects = subject_fn(
            7,
            "modules/m.ts",
            r#"function aB(x){ let q = "kSliceMarkerZQX_unique"; let r = q + x; return r; }"#,
        );
        let references = reference_fn(
            "util/slice.ts",
            r#"function sliceWithMarker(x){ let q = "kSliceMarkerZQX_unique"; return x + q; }"#,
        );
        let rows = match_function_lists(&subjects, &references, &BTreeMap::new());
        assert!(
            rows.iter()
                .any(|r| r.accepted && r.semantic_name == "sliceWithMarker"),
            "one-statement drift should still promote under a unique anchor: {rows:?}"
        );
    }

    #[test]
    fn unique_anchor_blocks_on_arity_mismatch() {
        // Same decisive unique anchor, but the parameter counts disagree -> the lone
        // anchor is not corroborated, so pass 4b must NOT accept (stays a proposal).
        let subjects = subject_fn(
            7,
            "modules/m.ts",
            r#"function aB(x){ let q = "kSliceMarkerZQX_unique"; return q + x; }"#,
        );
        let references = reference_fn(
            "util/slice.ts",
            r#"function sliceWithMarker(x, y){ let q = "kSliceMarkerZQX_unique"; return x + q + y; }"#,
        );
        let rows = match_function_lists(&subjects, &references, &BTreeMap::new());
        assert!(
            !rows.iter().any(|r| r.accepted),
            "arity mismatch must block the unique-anchor promotion: {rows:?}"
        );
    }

    #[test]
    fn multi_rare_anchor_promotes_low_jaccard_function() {
        // Bodies diverge structurally (no shared AST/composite) and share only 3 of
        // many anchors (Jaccard < 0.6, so pass 3 cannot fire), but the 3 shared
        // anchors are globally rare and all point to ONE reference function -> pass
        // 3b promotes it.
        let subjects = subject_fn(
            8,
            "modules/m.ts",
            r#"function aB(){ if (g) { log("alpha_distinct_zzz"); } emit("beta_distinct_zzz"); return foo("gamma_distinct_zzz"); }"#,
        );
        let references = reference_fn(
            "util/multi.ts",
            r#"function multiAnchor(){ return [k("alpha_distinct_zzz"), k("beta_distinct_zzz"), k("gamma_distinct_zzz")]; }"#,
        );
        let rows = match_function_lists(&subjects, &references, &BTreeMap::new());
        assert!(
            rows.iter()
                .any(|r| r.accepted && r.semantic_name == "multiAnchor"),
            ">=3 rare shared anchors should promote despite low Jaccard: {rows:?}"
        );
    }

    #[test]
    fn multi_rare_anchor_blocks_on_tie() {
        // TWO reference functions each share all 3 rare anchors -> no clear winner,
        // so pass 3b must NOT accept (ambiguous). The anchors are in 2 reference
        // functions so pass 4b (needs globally-unique) also cannot fire.
        let subjects = subject_fn(
            8,
            "modules/m.ts",
            r#"function aB(){ log("alpha_distinct_zzz"); emit("beta_distinct_zzz"); return foo("gamma_distinct_zzz"); }"#,
        );
        let mut references = reference_fn(
            "util/a.ts",
            r#"function alphaFn(){ return [k("alpha_distinct_zzz"), k("beta_distinct_zzz"), k("gamma_distinct_zzz")]; }"#,
        );
        references.extend(reference_fn(
            "util/b.ts",
            r#"function betaFn(){ return j("alpha_distinct_zzz") + j("beta_distinct_zzz") + j("gamma_distinct_zzz"); }"#,
        ));
        let rows = match_function_lists(&subjects, &references, &BTreeMap::new());
        assert!(
            !rows.iter().any(|r| r.accepted),
            "a tie on rare-anchor count must block pass 3b: {rows:?}"
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
    fn function_match_keeps_noop_evidence_when_name_already_matches() {
        let subjects = subject_fn(1, "modules/m.ts", "function increment(x) { return x + 1; }");
        let references = reference_fn("util/inc.ts", "function increment(x) { return x + 1; }");
        let rows = match_function_lists(&subjects, &references, &corroborate(1, "util/inc.ts"));
        assert_eq!(rows.iter().filter(|row| row.accepted).count(), 1);
        assert_eq!(rows[0].original_name, "increment");
        assert_eq!(rows[0].semantic_name, "increment");
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

    #[test]
    fn ownership_entry_path_recovers_file_from_decorated_source_path() {
        // Specific-file match: the entry path sits after the `name@version/`
        // marker and runs up to the next `:`-delimited score field.
        assert_eq!(
            ownership_entry_path(
                "anonymous-function-axis-source:react@19.2.4:react@19.2.4/cjs/react.development.js:score=72:runner_up=46",
                "react",
                "19.2.4",
            ),
            Some("cjs/react.development.js".to_string())
        );
        // Package-only match carries no source file (no `name@version/` marker).
        assert_eq!(
            ownership_entry_path(
                "anonymous-function-axis:zod@3.24.1:score=76:runner_up=61",
                "zod",
                "3.24.1",
            ),
            None
        );
    }

    #[test]
    fn package_source_origin_is_not_automated_so_domain_names_pass() {
        // The ownership-naming pass keys names with a composite `package-source:`
        // origin, which must never be treated as automated — otherwise the
        // vocabulary gate would reject package domain tokens like `parseSemVer`.
        let origin = "package-source:owned:semver@7.6.0/index.js";
        validate_name_acceptance(
            "a",
            "parseSemVer",
            origin,
            Some("{\"tier\":\"fn-hash-unique\"}"),
            NamingGateMode::LocalBinding,
        )
        .expect("non-automated package-source origin must accept a domain function name");
    }

    #[test]
    fn rejects_decompiler_path_derived_synthetic_names() {
        for synthetic in [
            "app_bootstrap_agent_config_bX5",
            "init_install_command_wrapper_MK7",
            "app_runtime_environment_detection_DQ_1177",
            "app_bootstrap_btw_prefix_pattern_al2",
            "app_bootstrap_image_processing_init_eQ3",
            "init_state_setup_2_nt8",
            "init_lodash_env_deps_sT0",
            "aws_sdk_imports_cjs_zG3",
        ] {
            assert!(
                is_synthetic_path_derived_name(synthetic),
                "should detect synthetic {synthetic}"
            );
            assert!(
                !is_specific_reference_name(synthetic),
                "synthetic name must not propagate: {synthetic}"
            );
        }
        // Real names must survive: UPPER_SNAKE constants, camelCase, plain words.
        for good in [
            "ALGORITHM_IDENTIFIER_V4A",
            "API_VERSION",
            "ApplyGuardrailCommand",
            "detectRuntimeEnvironment",
            "loadAgentConfig",
        ] {
            assert!(
                !is_synthetic_path_derived_name(good),
                "real name wrongly flagged synthetic: {good}"
            );
            assert!(
                is_specific_reference_name(good),
                "real name must propagate: {good}"
            );
        }
    }
}
