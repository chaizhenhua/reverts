//! `reference-source-names` command: name a decompiled project's modules,
//! exports, and local bindings by matching its emitted TypeScript against a
//! historical first-party source tree. Tier-gated: only provable matches are
//! auto-accepted; everything else is left for an agent.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Instant;

use clap::{Args, ValueEnum};
use reverts_graph::{
    FunctionExtractor, IdentifierStreams, extract_import_specifiers, function_names,
    function_string_literals, identifier_streams,
};
use reverts_input::sqlite::{load_project_rows_from_connection, load_project_rows_from_sqlite};
use reverts_input::{InputRows, ModuleDependencyTarget, PackageAttributionStatus};
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
            let independently_strong = matched.reciprocal_best
                || matched.normalized_anchor >= MEDIUM_NORMALIZED_ANCHOR
                || guarded_graph_placement_promotion(matched);
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

fn tier_rank(tier: MatchTier) -> u8 {
    match tier {
        MatchTier::High => 2,
        MatchTier::Medium => 1,
        MatchTier::Low => 0,
    }
}

pub(crate) fn run(args: ReferenceSourceNamesArgs) -> Result<(), CliRunError> {
    let trace_start = Instant::now();
    let index = build_reference_source_index(&args.reference_source_root, &args.reference_version)
        .map_err(CliRunError::ReferenceSourceNames)?;
    trace_reference_source_names(trace_start, "build_reference_source_index");
    let subjects = if args.module_only {
        subject_modules_from_extracted_input(&args)?
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
            // single-file targets, so keep structural bags off there but still
            // build dependency graph evidence/diagnostics from extracted module
            // slices.
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
    let module_matched_file: BTreeMap<u32, String> = plans
        .iter()
        .filter(|plan| tier_passes(plan.matched.tier, args.min_tier))
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
    let propagated = propagate_symbols(
        &subjects,
        &index,
        &subject_fns,
        &reference_fns,
        &binding_rows,
    );
    binding_rows.extend(propagated);
    trace_reference_source_names(trace_start, "propagate_symbols");

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
            dependencies: BTreeSet::new(),
            bindings: bindings_for_path
                .remove(file.path.as_str())
                .unwrap_or_default(),
        });
    }
    Ok(modules)
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

fn package_owned_modules_from_rows(rows: &InputRows) -> BTreeSet<u32> {
    rows.package_attributions
        .iter()
        .filter(|attribution| {
            matches!(
                attribution.status,
                PackageAttributionStatus::Accepted | PackageAttributionStatus::Rejected
            ) && attribution.package_version.is_some()
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
            "structural_score": matched.structural_score,
            "graph_support": matched.graph_support,
            "graph_known_edges": matched.graph_known_edges,
            "matched_neighbor_ratio": matched_neighbor_ratio(matched),
        },
        "graph_structure": graph_structure_json(matched.graph_structure),
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
            "structural_score": matched.structural_score,
            "graph_support": matched.graph_support,
            "graph_known_edges": matched.graph_known_edges,
            "matched_neighbor_ratio": matched_neighbor_ratio(matched),
        },
        "graph_structure": graph_structure_json(matched.graph_structure),
        "source_score": source_score_json(matched.source_score),
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
    })
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
        + (evidence.top_level_declaration_overlap as f64) * 18.0
        + (evidence.import_export_surface_overlap as f64) * 16.0
        + (evidence.class_member_overlap as f64) * 18.0
        + (evidence.statement_window_overlap as f64) * 10.0
        + (evidence.block_branch_overlap as f64) * 8.0
        + (evidence.pq_gram_overlap as f64) * 6.0
        + (evidence.wl_overlap as f64) * 5.0
        + evidence.source_score.function_axis_jaccard * 120.0
        + evidence.source_score.function_axis_containment * 60.0
        + evidence.source_score.jsx_react_shape_jaccard * 80.0
        + (evidence.source_score.jsx_react_shape_overlap as f64) * 8.0
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
        || (evidence.source_score.function_axis_jaccard >= 0.20
            && evidence.source_score.function_axis_containment >= 0.50
            && (evidence.normalized_anchor >= 0.03
                || evidence.source_score.unique_string_anchor_overlap >= 1
                || evidence.source_score.jsx_react_shape_jaccard >= 0.15))
        || (evidence.source_score.jsx_react_shape_jaccard >= 0.35
            && (evidence.normalized_anchor >= 0.08
                || evidence.source_score.unique_string_anchor_overlap >= 1
                || evidence.source_score.function_axis_containment >= 0.35))
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
        || (matched.source_score.function_axis_overlap >= 4
            && matched.source_score.function_axis_containment >= 0.25)
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
    unique_delta >= 1
        && (function_delta >= 4
            || top.source_score.function_axis_jaccard
                >= runner_up.source_score.function_axis_jaccard + 0.05
            || positive_metric_delta(top, runner_up, |matched| matched.statement_window_overlap)
                >= AMBIGUOUS_PROMOTION_WINDOW_DELTA
            || positive_metric_delta(top, runner_up, |matched| matched.block_branch_overlap)
                >= AMBIGUOUS_PROMOTION_WINDOW_DELTA)
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
        let anchor_overlap = overlap_len(
            &fingerprint.string_anchors,
            &module.fingerprint.string_anchors,
        );
        let structural_score = structural_support
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
            && structural_score < 1.0
            && graph.matched_edges == 0
            && weighted_anchor < 1.0
        {
            continue;
        }
        let source_score = score_source_evidence(subject, &module.profile, &index.evidence_idf);
        let evidence = MatchEvidence {
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
                top_level_declaration_overlap,
                import_export_surface_overlap,
                class_member_overlap,
                statement_window_overlap,
                block_branch_overlap,
                pq_gram_overlap,
                wl_overlap,
                structural_score,
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
    literals: BTreeSet<String>,
}

/// One subject (emitted) function with a recoverable name.
struct SubjectFunction {
    module_id: u32,
    subject_path: String,
    name: String,
    fingerprint: FunctionFingerprint,
    literals: BTreeSet<String>,
}

/// Every named, specifically-named function across the whole reference tree.
fn collect_reference_functions(index: &ReferenceSourceIndex) -> Vec<ReferenceFunction> {
    let mut out = Vec::new();
    for module in &index.modules {
        let names: BTreeMap<reverts_ir::ByteRange, String> = function_names(module.source.as_str())
            .into_iter()
            .filter(|(_, name)| is_specific_reference_name(name))
            .collect();
        let mut literals = function_string_literals(module.source.as_str());
        for fingerprint in
            FunctionExtractor::fingerprint_primary(ModuleId(0), module.source.as_str())
        {
            if let Some(name) = names.get(&fingerprint.id.span) {
                let function_literals = literals.remove(&fingerprint.id.span).unwrap_or_default();
                out.push(ReferenceFunction {
                    file: module.file_path.clone(),
                    name: name.clone(),
                    fingerprint,
                    literals: function_literals,
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
        let mut literals = function_string_literals(subject.source.as_str());
        for fingerprint in FunctionExtractor::fingerprint_primary(
            ModuleId(subject.module_id),
            subject.source.as_str(),
        ) {
            if let Some(name) = names.get(&fingerprint.id.span) {
                let function_literals = literals.remove(&fingerprint.id.span).unwrap_or_default();
                out.push(SubjectFunction {
                    module_id: subject.module_id,
                    subject_path: subject.file_path.clone(),
                    name: name.clone(),
                    fingerprint,
                    literals: function_literals,
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
    index: &ReferenceSourceIndex,
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
    let reference_source: BTreeMap<&str, &str> = index
        .modules
        .iter()
        .map(|m| (m.file_path.as_str(), m.source.as_str()))
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
        let mut lits = function_string_literals(source);
        FunctionExtractor::fingerprint(ModuleId(module_id), source)
            .into_iter()
            .filter_map(|f| {
                names.get(&f.id.span).map(|name| SubjectFunction {
                    module_id,
                    subject_path: path.to_string(),
                    name: name.clone(),
                    literals: lits.remove(&f.id.span).unwrap_or_default(),
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
        let mut lits = function_string_literals(source);
        FunctionExtractor::fingerprint(ModuleId(0), source)
            .into_iter()
            .filter_map(|f| {
                names.get(&f.id.span).map(|name| ReferenceFunction {
                    file: file.to_string(),
                    name: name.clone(),
                    literals: lits.remove(&f.id.span).unwrap_or_default(),
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
        let propagated = propagate_symbols(&subjects, &index, &subject_fns, &reference_fns, &rows);
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
        // anchors a proposal that hash matching alone would miss.
        let subjects = subject_fn(
            7,
            "modules/m.ts",
            "function aB(x) { if (x) { log(\"uniqueDriftMarker_xyz\"); } return x; }",
        );
        let references = reference_fn(
            "util/drift.ts",
            "function realName(y) { return y ? emit(\"uniqueDriftMarker_xyz\") : 0; }",
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
}
