//! `match-modules-recall` subcommand: measure how well our function-fingerprint
//! matcher can pair ground-truth modules (from a reference project) with
//! subject modules (e.g. a bundled CLI).
//!
//! Read-only measurement tool — writes nothing to the database. Used to drive
//! the cross-version module matching improvement work without round-tripping
//! through ingestion.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use clap::{Args, ValueEnum};
use reverts_graph::{FunctionExtractor, extract_import_specifiers, extract_property_names};
use reverts_ir::{AxisKind, FunctionFingerprint, ModuleId};
use reverts_package_index::{
    Candidate, CfgKey, ExactKey, FeatureKey, FingerprintIndex, StructuralKey,
};
use rusqlite::{Connection, OpenFlags, params};

use crate::args::{parse_args_with_name, parse_project_id};
use crate::errors::{CliError, CliRunError};
use crate::help;

/// Axes scored when comparing two modules. Mirrors the cascade matcher's
/// `StructuralAnchored` tier: AST is the headline shape; Cfg and the anchor
/// axes (LiteralAnchor, CalleeSet, ThrowSet, StructuralAnchor) survive
/// bundler-induced refactoring; the pattern axes round out coverage.
pub(crate) const SCORING_AXES: &[AxisKind] = &[
    AxisKind::Ast,
    AxisKind::Cfg,
    AxisKind::StructuralAnchor,
    AxisKind::LiteralAnchor,
    AxisKind::CalleeSet,
    AxisKind::ThrowSet,
    AxisKind::ReturnPattern,
    AxisKind::EffectPattern,
    AxisKind::BindingPattern,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SimilarityMetric {
    /// `|A ∩ B| / |A ∪ B|`. Penalises modules whose fingerprint bags differ
    /// in size — e.g. a bundle module that inlines extra helpers.
    Jaccard,
    /// `|A ∩ B| / min(|A|, |B|)`. Forgiving of size asymmetry: hits whenever
    /// the smaller bag is largely covered by the larger one. Better for the
    /// "source-truth (small) vs bundle (large) inlined" case.
    Overlap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum MatchStrategy {
    /// Bag-of-fingerprints scoring with per-axis Jaccard/Overlap, combined
    /// across axes. The original module-matcher; tunable via `--metric`,
    /// `--combiner`, `--threshold-percent`.
    BagJaccard,
    /// Per-function tier-based matching that reuses the package matcher's
    /// `FingerprintIndex`. Each ref function looks up subject candidates in
    /// the shared inverted index, dedups by subject module, and aggregates
    /// function-level wins up to the ref module. No Jaccard threshold; the
    /// only knob is the per-tier acceptance ladder.
    FunctionTier,
    /// Aggregate structural-bag scorer reused verbatim from the package
    /// matcher (`reverts_package_matcher::score_structural_bags`). Compares
    /// per-axis multiset counts, axis-pair combinations, and per-function
    /// shape coverage with the same tuned weights as the package matcher's
    /// `match_structural_bags` tier — no module-matcher-specific code.
    StructuralBag,
    /// Two-pass matcher mirroring the package matcher's pipeline:
    /// (1) bag-jaccard pairs ref modules with subject modules ("bucket"
    /// selection — the analog of `(pkg_name, version)`), then (2) the
    /// shared function-tier cascade runs **inside each paired bucket**.
    /// Restricting candidate space to the partner module typically
    /// collapses the noisy multi-candidate cases that drop function-tier
    /// recall, so direct name transfer ratios jump (estimate from
    /// `print_function_naming_coverage` analysis: 3.5 % → ~40 %).
    ModulePinnedFunctionTier,
    /// String-literal corpus overlap. Each module's source is scanned for
    /// non-trivial string literals (>= 4 chars) and hashed into a per-
    /// module corpus; pairs are scored by Jaccard over those corpora. The
    /// signal is orthogonal to function fingerprints — bundlers very
    /// rarely rewrite string contents — so it covers many modules that
    /// AST/CFG-based scoring misses.
    StringLiteral,
    /// Cascade of orthogonal signals. Runs bag-jaccard, string-literal,
    /// keyword-histogram, and module-pinned function-tier independently;
    /// the final pick is the subject with the most agreement (weighted
    /// vote). Each signal that hits the same subject for a given ref
    /// module adds a vote, scaled by that signal's confidence. Subjects
    /// with two or more agreeing signals are considered high-confidence
    /// pairs even when no single signal would have triggered alone.
    SignalCascade,
    /// Cosine similarity over per-module structural-keyword histograms
    /// (Lever 5 lite). Identifier-invariant and bundler-stable, but
    /// coarse — strongest as one signal among many.
    KeywordHistogram,
    /// Dep-graph anchor propagation. Bootstraps with the signal-cascade's
    /// 3-or-more-signal-agreement pairs as anchors, then walks the cli
    /// bundle's `module_dependencies` graph and the ref-side import-
    /// specifier list in lockstep: an anchor's i-th ref import is
    /// proposed as the partner of the anchor's i-th subject dep,
    /// provided string-corpus or keyword-histogram backs it up. Iterates
    /// until no new pairs land.
    DepGraphPropagation,
    /// Bag-jaccard with orthogonal-signal rescue. Runs bag-jaccard at the
    /// usual 0.20 threshold, then for every ref module that *failed* the
    /// threshold checks string-corpus (≥ 0.50) and keyword-histogram
    /// (≥ 0.90). If either orthogonal signal nominates a subject, accept
    /// it. Recovers near-miss bag-jaccard modules that the orthogonal
    /// axes (which scoring noise affects independently) still see clearly.
    BagJaccardRescued,
    /// Property/method/member-name Jaccard. Identifiers that bundlers
    /// cannot rename (class methods, object literal keys, `.prop`
    /// accesses) form a per-module corpus; pairs are scored by Jaccard
    /// over those corpora. Orthogonal to everything else.
    PropertyName,
    /// End-to-end composite — what production naming should run.
    /// Step 1: `bag-jaccard-rescued` produces high-recall module pairs
    /// (+ orthogonal-signal rescues). Step 2: feeds those pairs to
    /// `module-pinned-function-tier` so per-function name transfer
    /// happens inside the candidate-restricted bucket. Reports both
    /// module-level recall and function-level naming coverage in one go.
    /// Also enforces category-respect (cross-category pairs are
    /// dropped before pinning).
    Composite,
}

// ---------------------------------------------------------------------------
// Note on the original "Lever 5: α-renamed body_hash for fingerprints"
// ---------------------------------------------------------------------------
// A code read of `reverts-graph/src/fingerprint/ast.rs` shows the existing
// function-body AST hash already collapses every `Identifier` expression
// to a single `"id"` token (line 86) and ships a regression test
// (`ast_hash_collides_for_alpha_renamed_functions`) locking that in. So
// the headline AST hash is already α-rename invariant at the function-
// body level. The 46 % AST exact-hit ceiling we measured on
// CC 2.1.89 src-ref ↔ bundle is therefore *structural variance*
// (different control-flow shapes between ref and the bundled form), not
// identifier renaming. An extended α-rename pass in `reverts-graph`
// would not move tier_unique. The remaining levers worth pursuing
// (dependency-graph anchor propagation and bag-jaccard prefilter
// expansion) are wired below.

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AxisCombiner {
    /// Per pair, take the single best per-axis score. Recall-friendly but
    /// over-rewards modules whose shape coincidentally matches on one
    /// structural axis (binding_pattern, structural_anchor).
    Max,
    /// Per pair, average the per-axis scores over axes where the ref bag has
    /// at least one hash. Dilutes single-axis coincidences and lifts
    /// multi-axis agreement — the precision-friendly default.
    Mean,
    /// Per pair, count axes whose per-axis score >= 0.5 and normalise by 9.
    /// Strongly categorical: "how many independent signals agree".
    Agreement,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
#[allow(clippy::struct_field_names)]
pub struct MatchModulesRecallArgs {
    #[arg(long)]
    pub input: PathBuf,
    #[arg(long, value_parser = parse_project_id)]
    pub ground_truth_project_id: u32,
    #[arg(long, value_parser = parse_project_id)]
    pub subject_project_id: u32,
    /// Which matching strategy to evaluate. Bag-Jaccard is the original;
    /// function-tier reuses the package matcher's shared FingerprintIndex
    /// for per-function exact-and-cascade matching.
    #[arg(long, value_enum, default_value_t = MatchStrategy::BagJaccard)]
    pub strategy: MatchStrategy,
    /// Similarity threshold percent (0-100) above which a (ref, subject) pair
    /// counts as a match. Default 30 — calibrated for the mean-axis combiner.
    #[arg(long, default_value_t = 30)]
    pub threshold_percent: u32,
    /// Similarity metric used to combine per-axis fingerprint overlap. Jaccard
    /// is the principled default; overlap is more forgiving but easily fires
    /// on subset coincidences and should be reserved for diagnostic sweeps.
    #[arg(long, value_enum, default_value_t = SimilarityMetric::Jaccard)]
    pub metric: SimilarityMetric,
    /// How to combine per-axis scores into a single pair score. Mean is the
    /// precision-friendly default.
    #[arg(long, value_enum, default_value_t = AxisCombiner::Mean)]
    pub combiner: AxisCombiner,
    /// Disable IDF (inverse-doc-frequency) weighting of fingerprint hashes.
    /// IDF down-weights hashes that show up in many subject modules — usually
    /// good for precision because common shapes stop dominating.
    #[arg(long, default_value_t = false)]
    pub no_idf: bool,
    /// Enable a function-count size prior multiplier on the per-pair score.
    /// Penalises pairs whose function counts diverge. Off by default — on the
    /// CC 2.1.89 src-ref vs bundle data it suppresses recall without lifting
    /// precision, because many cli "named" modules legitimately differ in
    /// size from the source counterpart (re-export thin / inlined fat).
    #[arg(long, default_value_t = false)]
    pub size_prior: bool,
    /// Restrict to modules of this category (e.g. "application", "package").
    /// Repeatable. Empty = all categories.
    #[arg(long = "category")]
    pub categories: Vec<String>,
    /// Optional cap on modules per project (for fast iteration).
    #[arg(long)]
    pub limit: Option<usize>,
    /// Print up to N baseline-pair mispair examples, showing ref name, the
    /// matcher's pick, and the named subject the matcher should have picked.
    #[arg(long, default_value_t = 0)]
    pub show_mispairs: usize,
    /// Minimum tier confidence required for a function-tier match to count
    /// toward `tier_unique` naming coverage. Filters the per-function
    /// cascade *after* the fact — does not change which subject module wins
    /// at the module level. `high` only accepts exact AST matches; `medium`
    /// also accepts structural-anchored (Cfg + literal/callee/throw anchor)
    /// matches; `low` (default) accepts every tier the cascade returns,
    /// including weak structural_only fallbacks.
    #[arg(long, value_enum, default_value_t = TierConfidence::Low)]
    pub min_tier_confidence: TierConfidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TierConfidence {
    /// Accept only [`MatchTier::Exact`] and [`MatchTier::ExactAlternate`]
    /// — i.e. AST-identical functions across passes.
    High,
    /// Accept exact tiers plus [`MatchTier::StructuralAnchored`] and its
    /// alternate variant (CFG + ≥1 anchor axis overlap on identity-keyed
    /// dedup).
    Medium,
    /// Accept every cascade tier including structural-only fallbacks.
    Low,
}

impl MatchModulesRecallArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, CliError> {
        let mut args = args.into_iter().collect::<Vec<_>>();
        if args
            .first()
            .is_some_and(|argument| argument == help::MATCH_MODULES_RECALL_COMMAND)
        {
            args.remove(0);
        }
        parse_args_with_name(help::MATCH_MODULES_RECALL_COMMAND, args)
    }
}

#[derive(Debug, Clone)]
struct ModuleRecord {
    id: i64,
    semantic_name: Option<String>,
    category: String,
    file_path: String,
    byte_start: u32,
    byte_end: u32,
}

/// Per-axis bag of hashes for one module. Each axis bag is the union of the
/// primary fingerprint axis hash plus every alternate normalization pass
/// hash, taken over every function in the module. Unioning across passes
/// means a function only has to land its canonical form on one side for the
/// hash to align — the alternate-pass machinery exists precisely to bridge
/// bundler-induced AST drift.
#[derive(Debug, Default, Clone)]
struct ModuleBag {
    by_axis: BTreeMap<AxisKind, BTreeSet<u64>>,
    function_count: usize,
}

/// Subject-module owner stored inside [`FingerprintIndex`] candidates for
/// the function-tier strategy. Plain index into the subject module slice;
/// the matcher never carries package metadata so we keep the owner type
/// minimal.
type ModuleOwner = usize;

pub(crate) fn run(args: MatchModulesRecallArgs) -> Result<(), CliRunError> {
    let conn = Connection::open_with_flags(&args.input, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|source| CliRunError::MatchModulesRecall(format!("open db: {source}")))?;

    let category_filter = if args.categories.is_empty() {
        None
    } else {
        Some(args.categories.iter().cloned().collect::<BTreeSet<_>>())
    };

    let started = Instant::now();
    let ref_modules = load_modules(
        &conn,
        args.ground_truth_project_id,
        category_filter.as_ref(),
        args.limit,
    )
    .map_err(|source| CliRunError::MatchModulesRecall(format!("load ref: {source}")))?;
    let sub_modules = load_modules(
        &conn,
        args.subject_project_id,
        category_filter.as_ref(),
        args.limit,
    )
    .map_err(|source| CliRunError::MatchModulesRecall(format!("load subject: {source}")))?;

    println!(
        "loaded ref project {} ({} modules) and subject project {} ({} modules) in {:.2}s",
        args.ground_truth_project_id,
        ref_modules.len(),
        args.subject_project_id,
        sub_modules.len(),
        started.elapsed().as_secs_f64()
    );

    // Baseline strategy: existing semantic_name overlap.
    let sub_names: BTreeSet<&str> = sub_modules
        .iter()
        .filter_map(|m| m.semantic_name.as_deref())
        .collect();
    let baseline_hits = ref_modules
        .iter()
        .filter(|m| {
            m.semantic_name
                .as_deref()
                .is_some_and(|name| sub_names.contains(name))
        })
        .count();
    println!(
        "[baseline / semantic_name exact]: {} / {} ref modules matched ({:.2}%)",
        baseline_hits,
        ref_modules.len(),
        pct(baseline_hits, ref_modules.len())
    );

    let fp_started = Instant::now();
    let mut ref_fps = fingerprint_modules(&ref_modules);
    let mut sub_fps = fingerprint_modules(&sub_modules);
    // Hydrate dep targets for the subject side from the cli bundle's
    // `module_dependencies` rows so the dep-graph follow-up has both
    // sides of the edge.
    load_dependency_targets(&conn, args.subject_project_id, &sub_modules, &mut sub_fps)
        .map_err(|source| CliRunError::MatchModulesRecall(format!("load sub deps: {source}")))?;
    // Ref side has no DB-stored deps (typical for sources ingested as
    // separate files); rely on the OXC import-specifier extractor that
    // already populated `dependency_specifiers`. Leave `dependency_targets`
    // empty for refs — pairs are resolved by specifier text, not idx.
    let _ = &mut ref_fps;
    let ref_bags: Vec<ModuleBag> = ref_fps.iter().map(|m| m.bag.clone()).collect();
    let sub_bags: Vec<ModuleBag> = sub_fps.iter().map(|m| m.bag.clone()).collect();
    println!(
        "fingerprinted ref ({} fps over {} modules) and subject ({} fps over {} modules) in {:.2}s",
        ref_bags.iter().map(|b| b.function_count).sum::<usize>(),
        ref_bags.iter().filter(|b| b.function_count > 0).count(),
        sub_bags.iter().map(|b| b.function_count).sum::<usize>(),
        sub_bags.iter().filter(|b| b.function_count > 0).count(),
        fp_started.elapsed().as_secs_f64(),
    );

    let scoring_started = Instant::now();
    let threshold = f64::from(args.threshold_percent) / 100.0;
    let use_idf = !args.no_idf;
    let use_size_prior = args.size_prior;
    let (best_per_ref, scoring_context_opt) = match args.strategy {
        MatchStrategy::BagJaccard => {
            let (best, context) = score_best_subjects(
                &ref_bags,
                &sub_bags,
                args.metric,
                args.combiner,
                use_idf,
                use_size_prior,
            );
            let recall = summarise_recall(&best, threshold);
            println!(
                "[bag_jaccard_{}_{}{}{} >= {:.2}]: {} / {} ref modules matched ({:.2}%) — scoring in {:.2}s",
                combiner_label(args.combiner),
                metric_label(args.metric),
                if use_idf { "_idf" } else { "" },
                if use_size_prior { "_sz" } else { "" },
                threshold,
                recall.matched,
                ref_modules.len(),
                pct(recall.matched, ref_modules.len()),
                scoring_started.elapsed().as_secs_f64(),
            );
            print_histogram(&recall.score_histogram, ref_modules.len());
            print_axis_winners(&recall.winning_axis_counts);
            (best, Some(context))
        }
        MatchStrategy::FunctionTier => {
            let report = score_via_function_tier(&ref_fps, &sub_fps);
            let recall = summarise_recall(&report.best, threshold);
            println!(
                "[function_tier (shared FingerprintIndex)]: {} / {} ref modules matched ({:.2}%) — scoring in {:.2}s",
                recall.matched,
                ref_modules.len(),
                pct(recall.matched, ref_modules.len()),
                scoring_started.elapsed().as_secs_f64(),
            );
            print_histogram(&recall.score_histogram, ref_modules.len());
            print_tier_breakdown(&report.tier_counts);
            print_function_naming_coverage(&report, &ref_fps, &sub_fps);
            (report.best, None)
        }
        MatchStrategy::StructuralBag => {
            let best = score_via_structural_bag(&ref_fps, &sub_fps);
            let recall = summarise_recall(&best, threshold);
            println!(
                "[structural_bag (shared package-matcher scorer)]: {} / {} ref modules matched ({:.2}%) — scoring in {:.2}s",
                recall.matched,
                ref_modules.len(),
                pct(recall.matched, ref_modules.len()),
                scoring_started.elapsed().as_secs_f64(),
            );
            print_histogram(&recall.score_histogram, ref_modules.len());
            (best, None)
        }
        MatchStrategy::SignalCascade => {
            // Three independent signals scoring the same (ref, subject)
            // space. Each produces a best pick per ref module.
            let bag_context = build_scoring_context(
                &ref_bags,
                &sub_bags,
                SimilarityMetric::Jaccard,
                AxisCombiner::Mean,
                true,
                false,
            );
            let bag_best = score_best_subjects_with(&bag_context, &ref_bags, &sub_bags);
            let str_best = score_via_string_literal(&ref_fps, &sub_fps);
            let kw_best = score_via_keyword_histogram(&ref_fps, &sub_fps);
            let pinned: Vec<Option<usize>> = bag_best
                .iter()
                .map(|m| m.subject_idx.filter(|_| m.score >= 0.20))
                .collect();
            let func_report = score_via_module_pinned_function_tier(&ref_fps, &sub_fps, &pinned);

            let signals: [&[BestMatch]; 4] = [&bag_best, &str_best, &kw_best, &func_report.best];
            let signal_names: [&str; 4] = [
                "bag_jaccard",
                "string_literal",
                "keyword_histogram",
                "module_pinned_function_tier",
            ];
            let signal_thresholds: [f64; 4] = [0.20, 0.30, 0.85, 0.30];

            let mut best_per_ref: Vec<BestMatch> = vec![BestMatch::default(); ref_modules.len()];
            let mut agreement_counts: BTreeMap<usize, usize> = BTreeMap::new();
            for (ref_idx, _) in ref_modules.iter().enumerate() {
                let mut votes: HashMap<usize, (f64, usize)> = HashMap::new();
                for (i, signal) in signals.iter().enumerate() {
                    let pick = signal[ref_idx];
                    if pick.score < signal_thresholds[i] {
                        continue;
                    }
                    let Some(sub_idx) = pick.subject_idx else {
                        continue;
                    };
                    let entry = votes.entry(sub_idx).or_insert((0.0, 0));
                    entry.0 += pick.score;
                    entry.1 += 1;
                }
                let mut best_subject = None;
                let mut best_total = 0.0_f64;
                let mut best_n = 0usize;
                for (&sub_idx, &(total, n)) in &votes {
                    let weighted = total * (n as f64);
                    if weighted > best_total {
                        best_total = weighted;
                        best_subject = Some(sub_idx);
                        best_n = n;
                    }
                }
                if let Some(sub_idx) = best_subject {
                    *agreement_counts.entry(best_n).or_default() += 1;
                    best_per_ref[ref_idx] = BestMatch {
                        subject_idx: Some(sub_idx),
                        score: (best_total / signals.len() as f64).clamp(0.0, 1.0),
                        axis: None,
                    };
                }
            }
            let recall = summarise_recall(&best_per_ref, threshold);
            println!(
                "[signal_cascade (bag_jaccard ∪ string_literal ∪ module_pinned_function_tier)]: {} / {} ref modules matched ({:.2}%) — scoring in {:.2}s",
                recall.matched,
                ref_modules.len(),
                pct(recall.matched, ref_modules.len()),
                scoring_started.elapsed().as_secs_f64(),
            );
            print_histogram(&recall.score_histogram, ref_modules.len());
            let entries: Vec<_> = agreement_counts.iter().collect();
            let line = entries
                .iter()
                .map(|(n, count)| format!("{n}-agree: {count}"))
                .collect::<Vec<_>>()
                .join("  ");
            println!("  signal agreement breakdown: {line}");
            for (i, name) in signal_names.iter().enumerate() {
                let above = signals[i]
                    .iter()
                    .filter(|m| m.subject_idx.is_some() && m.score >= signal_thresholds[i])
                    .count();
                println!(
                    "    signal {} hit {} ref modules at >= {:.2}",
                    name, above, signal_thresholds[i]
                );
            }
            print_function_naming_coverage(&func_report, &ref_fps, &sub_fps);
            (best_per_ref, None)
        }
        MatchStrategy::PropertyName => {
            let best = score_via_property_name(&ref_fps, &sub_fps);
            let recall = summarise_recall(&best, threshold);
            println!(
                "[property_name (bundler-stable identifier corpus)]: {} / {} ref modules matched ({:.2}%) — scoring in {:.2}s",
                recall.matched,
                ref_modules.len(),
                pct(recall.matched, ref_modules.len()),
                scoring_started.elapsed().as_secs_f64(),
            );
            print_histogram(&recall.score_histogram, ref_modules.len());
            print_property_corpus_stats(&ref_fps, &sub_fps);
            (best, None)
        }
        MatchStrategy::Composite => {
            // Step 1: rescued pairing. Same logic as the standalone
            // `bag-jaccard-rescued` strategy, but with a category-respect
            // filter so cross-category pairings (application ↔ package
            // etc.) get dropped before we pin functions inside them.
            let bag_context = build_scoring_context(
                &ref_bags,
                &sub_bags,
                SimilarityMetric::Jaccard,
                AxisCombiner::Mean,
                true,
                false,
            );
            let bag_best = score_best_subjects_with(&bag_context, &ref_bags, &sub_bags);
            let str_best = score_via_string_literal(&ref_fps, &sub_fps);
            let kw_best = score_via_keyword_histogram(&ref_fps, &sub_fps);
            let prop_best = score_via_property_name(&ref_fps, &sub_fps);
            let rare_best = score_via_rare_ast_anchor(&ref_bags, &sub_bags);
            const BAG_ACCEPT: f64 = 0.20;
            // Tighter category check (no 'unknown' wildcard) allows a
            // lower bag-jaccard floor without absorbing cross-category
            // noise: in-category 0.15-0.20 is still a real signal.
            const BAG_ACCEPT_STRICT: f64 = 0.15;
            const STR_RESCUE: f64 = 0.50;
            const KW_RESCUE: f64 = 0.90;
            const PROP_RESCUE: f64 = 0.40;
            // ≥2 rare-AST hash hits on the same subject is a smoking gun.
            const RARE_RESCUE: f64 = 2.0;
            // Consensus floors: signal must clear these to vote, and ≥2
            // signals must agree on the same subject for a consensus pin.
            const BAG_FLOOR: f64 = 0.10;
            const STR_FLOOR: f64 = 0.20;
            const KW_FLOOR: f64 = 0.70;
            const PROP_FLOOR: f64 = 0.20;

            let mut pinned: Vec<Option<usize>> = Vec::with_capacity(ref_modules.len());
            let mut rescued_str = 0usize;
            let mut rescued_kw = 0usize;
            let mut rescued_prop = 0usize;
            let mut rescued_rare = 0usize;
            let mut rescued_band = 0usize;
            let mut rescued_consensus = 0usize;
            let mut dropped_cross_category = 0usize;
            let category_ok = |ref_idx: usize, sub_idx: usize| -> bool {
                let r = ref_modules[ref_idx].category.as_str();
                let s = sub_modules[sub_idx].category.as_str();
                r == s || r.is_empty() || s.is_empty() || r == "unknown" || s == "unknown"
            };
            // Strict: no 'unknown' wildcard. Gates the lower bag-jaccard
            // band where cross-category noise would be too risky.
            let category_strict_ok = |ref_idx: usize, sub_idx: usize| -> bool {
                let r = ref_modules[ref_idx].category.as_str();
                let s = sub_modules[sub_idx].category.as_str();
                r == s || r.is_empty() || s.is_empty()
            };
            for ref_idx in 0..ref_modules.len() {
                let mut pick: Option<usize> = None;
                let bag = bag_best[ref_idx];
                if bag.score >= BAG_ACCEPT
                    && let Some(sub_idx) = bag.subject_idx
                {
                    if category_ok(ref_idx, sub_idx) {
                        pick = Some(sub_idx);
                    } else {
                        dropped_cross_category += 1;
                    }
                }
                if pick.is_none()
                    && bag.score >= BAG_ACCEPT_STRICT
                    && let Some(sub_idx) = bag.subject_idx
                    && category_strict_ok(ref_idx, sub_idx)
                {
                    pick = Some(sub_idx);
                    rescued_band += 1;
                }
                if pick.is_none() {
                    let str_pick = str_best[ref_idx];
                    if str_pick.score >= STR_RESCUE
                        && let Some(sub_idx) = str_pick.subject_idx
                        && category_ok(ref_idx, sub_idx)
                    {
                        pick = Some(sub_idx);
                        rescued_str += 1;
                    }
                }
                if pick.is_none() {
                    let kw_pick = kw_best[ref_idx];
                    if kw_pick.score >= KW_RESCUE
                        && let Some(sub_idx) = kw_pick.subject_idx
                        && category_ok(ref_idx, sub_idx)
                    {
                        pick = Some(sub_idx);
                        rescued_kw += 1;
                    }
                }
                if pick.is_none() {
                    let prop_pick = prop_best[ref_idx];
                    if prop_pick.score >= PROP_RESCUE
                        && let Some(sub_idx) = prop_pick.subject_idx
                        && category_ok(ref_idx, sub_idx)
                    {
                        pick = Some(sub_idx);
                        rescued_prop += 1;
                    }
                }
                if pick.is_none() {
                    let rare_pick = rare_best[ref_idx];
                    if rare_pick.score >= RARE_RESCUE
                        && let Some(sub_idx) = rare_pick.subject_idx
                        && category_ok(ref_idx, sub_idx)
                    {
                        pick = Some(sub_idx);
                        rescued_rare += 1;
                    }
                }
                // Consensus rescue: if ≥2 weak signals agree on the same
                // subject, pin it. Each signal alone is noisy at low
                // thresholds; agreement is the filter.
                if pick.is_none() {
                    let mut votes: BTreeMap<usize, u32> = BTreeMap::new();
                    if bag.score >= BAG_FLOOR
                        && let Some(sub_idx) = bag.subject_idx
                        && category_ok(ref_idx, sub_idx)
                    {
                        *votes.entry(sub_idx).or_default() += 1;
                    }
                    let str_pick = str_best[ref_idx];
                    if str_pick.score >= STR_FLOOR
                        && let Some(sub_idx) = str_pick.subject_idx
                        && category_ok(ref_idx, sub_idx)
                    {
                        *votes.entry(sub_idx).or_default() += 1;
                    }
                    let kw_pick = kw_best[ref_idx];
                    if kw_pick.score >= KW_FLOOR
                        && let Some(sub_idx) = kw_pick.subject_idx
                        && category_ok(ref_idx, sub_idx)
                    {
                        *votes.entry(sub_idx).or_default() += 1;
                    }
                    let prop_pick = prop_best[ref_idx];
                    if prop_pick.score >= PROP_FLOOR
                        && let Some(sub_idx) = prop_pick.subject_idx
                        && category_ok(ref_idx, sub_idx)
                    {
                        *votes.entry(sub_idx).or_default() += 1;
                    }
                    if let Some((&sub_idx, &count)) = votes.iter().max_by_key(|(_, c)| *c)
                        && count >= 2
                    {
                        pick = Some(sub_idx);
                        rescued_consensus += 1;
                    }
                }
                pinned.push(pick);
            }
            let pin_hits = pinned.iter().filter(|x| x.is_some()).count();
            println!(
                "  composite step-1 (rescued module pairing): {} / {} ref modules pinned ({:.2}%), rescues: band={rescued_band} str={rescued_str} kw={rescued_kw} prop={rescued_prop} rare={rescued_rare} consensus={rescued_consensus}, cross-category-dropped={dropped_cross_category}",
                pin_hits,
                ref_modules.len(),
                pct(pin_hits, ref_modules.len())
            );

            // Step 2: module-pinned function tier on the pinned set.
            let func_report = score_via_module_pinned_function_tier(&ref_fps, &sub_fps, &pinned);
            let best_per_ref: Vec<BestMatch> = pinned
                .iter()
                .map(|sub_idx| BestMatch {
                    subject_idx: *sub_idx,
                    score: if sub_idx.is_some() { 1.0 } else { 0.0 },
                    axis: None,
                })
                .collect();
            let recall = summarise_recall(&best_per_ref, threshold);
            println!(
                "[composite (rescued pairing → module-pinned function tier)]: {} / {} ref modules matched ({:.2}%) — scoring in {:.2}s",
                recall.matched,
                ref_modules.len(),
                pct(recall.matched, ref_modules.len()),
                scoring_started.elapsed().as_secs_f64(),
            );
            print_histogram(&recall.score_histogram, ref_modules.len());
            print_tier_breakdown(&func_report.tier_counts);
            print_function_naming_coverage(&func_report, &ref_fps, &sub_fps);
            (best_per_ref, None)
        }
        MatchStrategy::BagJaccardRescued => {
            let bag_context = build_scoring_context(
                &ref_bags,
                &sub_bags,
                SimilarityMetric::Jaccard,
                AxisCombiner::Mean,
                true,
                false,
            );
            let bag_best = score_best_subjects_with(&bag_context, &ref_bags, &sub_bags);
            let str_best = score_via_string_literal(&ref_fps, &sub_fps);
            let kw_best = score_via_keyword_histogram(&ref_fps, &sub_fps);

            const BAG_ACCEPT: f64 = 0.20;
            const STR_RESCUE: f64 = 0.50;
            const KW_RESCUE: f64 = 0.90;

            let mut best_per_ref: Vec<BestMatch> = Vec::with_capacity(ref_modules.len());
            let mut rescued_str = 0usize;
            let mut rescued_kw = 0usize;
            for ref_idx in 0..ref_modules.len() {
                if bag_best[ref_idx].score >= BAG_ACCEPT && bag_best[ref_idx].subject_idx.is_some()
                {
                    best_per_ref.push(bag_best[ref_idx]);
                    continue;
                }
                // Below the bag threshold — try orthogonal-signal rescue.
                let str_pick = str_best[ref_idx];
                let kw_pick = kw_best[ref_idx];
                if str_pick.score >= STR_RESCUE && str_pick.subject_idx.is_some() {
                    rescued_str += 1;
                    best_per_ref.push(BestMatch {
                        subject_idx: str_pick.subject_idx,
                        // Carry the rescuing signal's own score forward so
                        // histogram bucketing reflects the true confidence
                        // the rescue is based on (not the failed bag-jaccard
                        // score). Downstream callers that need rescue
                        // provenance can branch on score >= STR_RESCUE.
                        score: str_pick.score,
                        axis: None,
                    });
                    continue;
                }
                if kw_pick.score >= KW_RESCUE && kw_pick.subject_idx.is_some() {
                    rescued_kw += 1;
                    best_per_ref.push(BestMatch {
                        subject_idx: kw_pick.subject_idx,
                        score: kw_pick.score,
                        axis: None,
                    });
                    continue;
                }
                best_per_ref.push(bag_best[ref_idx]);
            }

            let recall = summarise_recall(&best_per_ref, threshold);
            println!(
                "[bag_jaccard_rescued (bag>=0.20 OR str>=0.50 OR kw>=0.90)]: {} / {} ref modules matched ({:.2}%) — scoring in {:.2}s",
                recall.matched,
                ref_modules.len(),
                pct(recall.matched, ref_modules.len()),
                scoring_started.elapsed().as_secs_f64(),
            );
            println!("  rescues: string-corpus={rescued_str}, keyword-histogram={rescued_kw}");
            print_histogram(&recall.score_histogram, ref_modules.len());
            (best_per_ref, None)
        }
        MatchStrategy::DepGraphPropagation => {
            // Build the specifier → ref_idx lookup the propagation pass
            // needs. Ref semantic_names look like `bridge/foo` (no leading
            // dot); specifiers in source look like `./bridge/foo` or
            // `../shared/utils`. Normalize both sides to bare relative
            // paths before joining.
            let mut spec_to_ref: BTreeMap<String, usize> = BTreeMap::new();
            for (idx, m) in ref_modules.iter().enumerate() {
                if let Some(name) = m.semantic_name.as_deref() {
                    spec_to_ref.insert(name.to_string(), idx);
                    spec_to_ref.insert(format!("./{name}"), idx);
                    // Also register every suffix of the path so specifiers
                    // walking up directories still resolve. The cost is
                    // O(path-depth) extra entries — typically <= 5.
                    let mut suffix = name.to_string();
                    while let Some(pos) = suffix.find('/') {
                        suffix = suffix[pos + 1..].to_string();
                        spec_to_ref.entry(format!("./{suffix}")).or_insert(idx);
                        spec_to_ref.entry(format!("../{suffix}")).or_insert(idx);
                    }
                }
            }
            let best = score_via_dep_graph_propagation(&ref_fps, &sub_fps, &spec_to_ref);
            let recall = summarise_recall(&best, threshold);
            println!(
                "[dep_graph_propagation]: {} / {} ref modules matched ({:.2}%) — scoring in {:.2}s — {} specifier entries",
                recall.matched,
                ref_modules.len(),
                pct(recall.matched, ref_modules.len()),
                scoring_started.elapsed().as_secs_f64(),
                spec_to_ref.len(),
            );
            print_histogram(&recall.score_histogram, ref_modules.len());
            (best, None)
        }
        MatchStrategy::KeywordHistogram => {
            let best = score_via_keyword_histogram(&ref_fps, &sub_fps);
            let recall = summarise_recall(&best, threshold);
            println!(
                "[keyword_histogram (cosine over JS keyword counts)]: {} / {} ref modules matched ({:.2}%) — scoring in {:.2}s",
                recall.matched,
                ref_modules.len(),
                pct(recall.matched, ref_modules.len()),
                scoring_started.elapsed().as_secs_f64(),
            );
            print_histogram(&recall.score_histogram, ref_modules.len());
            (best, None)
        }
        MatchStrategy::StringLiteral => {
            let best = score_via_string_literal(&ref_fps, &sub_fps);
            let recall = summarise_recall(&best, threshold);
            println!(
                "[string_literal (orthogonal corpus)]: {} / {} ref modules matched ({:.2}%) — scoring in {:.2}s",
                recall.matched,
                ref_modules.len(),
                pct(recall.matched, ref_modules.len()),
                scoring_started.elapsed().as_secs_f64(),
            );
            print_histogram(&recall.score_histogram, ref_modules.len());
            print_string_corpus_stats(&ref_fps, &sub_fps);
            (best, None)
        }
        MatchStrategy::ModulePinnedFunctionTier => {
            // Pass 1: bag-jaccard with the precision-friendly defaults to
            // pair ref modules with subject modules.
            let pin_context = build_scoring_context(
                &ref_bags,
                &sub_bags,
                SimilarityMetric::Jaccard,
                AxisCombiner::Mean,
                true,
                false,
            );
            let pin_best = score_best_subjects_with(&pin_context, &ref_bags, &sub_bags);
            const PIN_ACCEPT_THRESHOLD: f64 = 0.20;
            let pinned: Vec<Option<usize>> = pin_best
                .iter()
                .map(|m| m.subject_idx.filter(|_| m.score >= PIN_ACCEPT_THRESHOLD))
                .collect();
            let pin_hits = pinned.iter().filter(|x| x.is_some()).count();
            println!(
                "  pass-1 bag-jaccard module pairing: {} / {} ref modules pinned (>= {:.2})",
                pin_hits,
                ref_modules.len(),
                PIN_ACCEPT_THRESHOLD
            );

            // Pass 2: inside each (ref, subject) pair, build a tiny
            // FingerprintIndex over the subject module's functions and run
            // the shared function-tier cascade. This is the "bucket then
            // cascade" pattern the package matcher uses for
            // (pkg_name, version) buckets.
            let report = score_via_module_pinned_function_tier(&ref_fps, &sub_fps, &pinned);
            let recall = summarise_recall(&report.best, threshold);
            println!(
                "[module_pinned_function_tier]: {} / {} ref modules matched ({:.2}%) — scoring in {:.2}s",
                recall.matched,
                ref_modules.len(),
                pct(recall.matched, ref_modules.len()),
                scoring_started.elapsed().as_secs_f64(),
            );
            print_histogram(&recall.score_histogram, ref_modules.len());
            print_tier_breakdown(&report.tier_counts);
            print_function_naming_coverage(&report, &ref_fps, &sub_fps);
            (report.best, None)
        }
    };

    print_exact_match_summary(&ref_bags, &sub_bags);

    let precision = precision_against_baseline(
        &ref_modules,
        &sub_modules,
        &ref_bags,
        &sub_bags,
        &best_per_ref,
        scoring_context_opt.as_ref(),
    );
    print_precision(&precision);
    if args.show_mispairs > 0
        && let Some(context) = scoring_context_opt.as_ref()
    {
        print_mispair_samples(
            &ref_modules,
            &sub_modules,
            &ref_bags,
            &sub_bags,
            &best_per_ref,
            context,
            args.show_mispairs,
        );
    }

    Ok(())
}

const fn metric_label(metric: SimilarityMetric) -> &'static str {
    match metric {
        SimilarityMetric::Jaccard => "jaccard",
        SimilarityMetric::Overlap => "overlap",
    }
}

const fn combiner_label(combiner: AxisCombiner) -> &'static str {
    match combiner {
        AxisCombiner::Max => "max",
        AxisCombiner::Mean => "mean",
        AxisCombiner::Agreement => "agreement",
    }
}

fn pct(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        (numerator as f64 * 100.0) / denominator as f64
    }
}

fn load_modules(
    connection: &Connection,
    project_id: u32,
    category_filter: Option<&BTreeSet<String>>,
    limit: Option<usize>,
) -> Result<Vec<ModuleRecord>, rusqlite::Error> {
    let mut statement = connection.prepare(
        r"
        SELECT m.id, m.semantic_name, m.module_category, sf.file_path, m.byte_start, m.byte_end
        FROM modules m
        JOIN project_files pf ON pf.file_id = m.file_id
        JOIN source_files sf  ON sf.id = m.file_id
        WHERE pf.project_id = ?1
        ORDER BY m.id
        ",
    )?;
    let rows = statement.query_map(params![i64::from(project_id)], |row| {
        Ok(ModuleRecord {
            id: row.get(0)?,
            semantic_name: row.get::<_, Option<String>>(1)?,
            category: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            file_path: row.get(3)?,
            byte_start: row.get::<_, i64>(4)? as u32,
            byte_end: row.get::<_, i64>(5)? as u32,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        let record = row?;
        if let Some(filter) = category_filter
            && !filter.contains(&record.category)
        {
            continue;
        }
        out.push(record);
        if let Some(cap) = limit
            && out.len() >= cap
        {
            break;
        }
    }
    Ok(out)
}

/// Per-module fingerprint payload: per-axis bag (bag-Jaccard strategy),
/// raw `FunctionFingerprint` list (function-tier strategies via
/// [`FingerprintIndex`]), the per-module string-literal corpus
/// (orthogonal axis: bundlers rarely rewrite string contents), and a
/// per-module **keyword histogram** (Lever 5 lite: identifier-invariant
/// structural fingerprint that proxies the effect of a full α-rename
/// without touching the upstream extractor).
/// One pass over the source so every strategy sees the same inputs.
struct ModuleFingerprints {
    bag: ModuleBag,
    raw: Vec<FunctionFingerprint>,
    string_corpus: BTreeSet<u64>,
    keyword_histogram: KeywordHistogram,
    /// Ordered import specifiers (`import x from "specifier"`,
    /// `require("specifier")`, dynamic `import("specifier")`). For the
    /// ref side these come from an OXC visitor over the module source;
    /// for the subject side they come from the `module_dependencies`
    /// SQLite table after a sub-module-id ↦ sub-idx remap. The list is
    /// the per-module degree signal used by the dep-graph follow-up.
    dependency_specifiers: Vec<String>,
    /// Indexes into the *peer* slice (ref→ref or sub→sub depending on
    /// which fingerprint slice this belongs to). Populated only for
    /// subjects via the `module_dependencies` join; remains empty for
    /// ref modules.
    dependency_targets: Vec<usize>,
    /// Distinct hashed property / method / static-member-access names
    /// (`componentDidMount`, `processInput`, `fromSSEResponse`, …) that
    /// the module declares or calls. Bundlers cannot rename these
    /// because external code may invoke them by name — so the set is
    /// stable across the cross-version pair and a strong identity
    /// signal orthogonal to AST/CFG fingerprints.
    property_names: BTreeSet<u64>,
}

/// Counts of structural JS keywords / `=>`. Bundler-stable (a minified
/// function still uses `function`, `return`, `if`); identifier-invariant
/// (no local variable names appear). Two modules with the same
/// control-flow shape have similar histograms, even when their AST
/// hashes diverge because renamed identifiers fall outside the
/// existing `collect_universal_renamable_bindings` coverage in
/// `reverts-graph::FunctionExtractor`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct KeywordHistogram {
    counts: [u32; KEYWORD_TOKENS.len()],
    total: u32,
}

impl Default for KeywordHistogram {
    fn default() -> Self {
        Self {
            counts: [0u32; KEYWORD_TOKENS.len()],
            total: 0,
        }
    }
}

const KEYWORD_TOKENS: &[&str] = &[
    "function",
    "return",
    "if",
    "else",
    "for",
    "while",
    "do",
    "switch",
    "case",
    "break",
    "continue",
    "throw",
    "try",
    "catch",
    "finally",
    "var",
    "let",
    "const",
    "new",
    "class",
    "extends",
    "this",
    "super",
    "import",
    "export",
    "default",
    "from",
    "as",
    "async",
    "await",
    "yield",
    "typeof",
    "instanceof",
    "in",
    "of",
    "delete",
    "void",
    "null",
    "true",
    "false",
    "undefined",
    "=>",
];

fn fingerprint_modules(modules: &[ModuleRecord]) -> Vec<ModuleFingerprints> {
    let mut source_cache: HashMap<String, Option<String>> = HashMap::new();
    let mut out = Vec::with_capacity(modules.len());
    for module in modules {
        let source = source_cache
            .entry(module.file_path.clone())
            .or_insert_with(|| fs::read_to_string(&module.file_path).ok());
        let Some(source_text) = source.as_deref() else {
            out.push(ModuleFingerprints {
                bag: ModuleBag::default(),
                raw: Vec::new(),
                string_corpus: BTreeSet::new(),
                keyword_histogram: KeywordHistogram::default(),
                dependency_specifiers: Vec::new(),
                dependency_targets: Vec::new(),
                property_names: BTreeSet::new(),
            });
            continue;
        };
        let start = module.byte_start as usize;
        let end = module.byte_end as usize;
        let slice = source_text
            .get(start..end.min(source_text.len()))
            .filter(|slice| !slice.is_empty());
        let Some(slice) = slice else {
            out.push(ModuleFingerprints {
                bag: ModuleBag::default(),
                raw: Vec::new(),
                string_corpus: BTreeSet::new(),
                keyword_histogram: KeywordHistogram::default(),
                dependency_specifiers: Vec::new(),
                dependency_targets: Vec::new(),
                property_names: BTreeSet::new(),
            });
            continue;
        };
        let module_id = ModuleId(u32::try_from(module.id).unwrap_or(u32::MAX));
        let raw = FunctionExtractor::fingerprint(module_id, slice);
        let bag = bag_from_fingerprints(&raw);
        let string_corpus = extract_string_corpus(slice);
        let keyword_histogram = extract_keyword_histogram(slice);
        let dependency_specifiers = extract_import_specifiers(slice);
        let property_names = extract_property_names(slice);
        out.push(ModuleFingerprints {
            bag,
            raw,
            string_corpus,
            keyword_histogram,
            dependency_specifiers,
            dependency_targets: Vec::new(),
            property_names,
        });
    }
    out
}

/// Hydrate per-subject-module dependency target lists from the cli
/// bundle's `module_dependencies` SQLite table. Maps subject module id
/// → its position in the `sub_fps` slice so propagation can walk the
/// dep graph by index. Modules without a `dependency_id` row in the
/// table get an empty `dependency_targets` Vec (the existing default).
fn load_dependency_targets(
    connection: &Connection,
    project_id: u32,
    sub_modules: &[ModuleRecord],
    sub_fps: &mut [ModuleFingerprints],
) -> Result<(), rusqlite::Error> {
    let id_to_idx: BTreeMap<i64, usize> = sub_modules
        .iter()
        .enumerate()
        .map(|(idx, m)| (m.id, idx))
        .collect();
    let mut stmt = connection.prepare(
        r"
        SELECT md.module_id, md.dependency_id
        FROM module_dependencies md
        JOIN modules m         ON m.id = md.module_id
        JOIN project_files pf  ON pf.file_id = m.file_id
        WHERE pf.project_id = ?1
        ",
    )?;
    let rows = stmt.query_map(params![i64::from(project_id)], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (owner_id, dep_id) = row?;
        let (Some(&owner_idx), Some(&dep_idx)) = (id_to_idx.get(&owner_id), id_to_idx.get(&dep_id))
        else {
            continue;
        };
        sub_fps[owner_idx].dependency_targets.push(dep_idx);
    }
    Ok(())
}

/// Count occurrences of structural JS keywords in a source slice. Reads
/// the slice as bytes, tracks whether we are inside a string / comment /
/// regex, and on every `identifier-or-keyword`-shaped run checks the
/// run against `KEYWORD_TOKENS`. The `=>` token is handled separately
/// because it is not an identifier shape. Templates and JSX are
/// best-effort: keywords inside `` ` ${...} ` `` interpolations are
/// counted (they are real expressions); keywords inside string literals
/// and comments are skipped.
fn extract_keyword_histogram(source: &str) -> KeywordHistogram {
    let mut hist = KeywordHistogram::default();
    let bytes = source.as_bytes();
    let mut i = 0usize;
    let in_ident_char = |c: u8| -> bool { c.is_ascii_alphanumeric() || c == b'_' || c == b'$' };
    while i < bytes.len() {
        let c = bytes[i];
        // Comments
        if c == b'/' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'/' => {
                    i += 2;
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                b'*' => {
                    i += 2;
                    while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                        i += 1;
                    }
                    i = (i + 2).min(bytes.len());
                    continue;
                }
                _ => {}
            }
        }
        // String / template literals
        if c == b'"' || c == b'\'' || c == b'`' {
            let quote = c;
            let mut j = i + 1;
            while j < bytes.len() {
                let cc = bytes[j];
                if cc == b'\\' {
                    j += 2;
                    continue;
                }
                if quote == b'`' && cc == b'$' && j + 1 < bytes.len() && bytes[j + 1] == b'{' {
                    let mut depth = 1usize;
                    j += 2;
                    while j < bytes.len() && depth > 0 {
                        match bytes[j] {
                            b'{' => depth += 1,
                            b'}' => depth -= 1,
                            _ => {}
                        }
                        j += 1;
                    }
                    continue;
                }
                if cc == quote {
                    break;
                }
                j += 1;
            }
            i = j.saturating_add(1).min(bytes.len());
            continue;
        }
        // Arrow `=>`
        if c == b'=' && i + 1 < bytes.len() && bytes[i + 1] == b'>' {
            // Tokens slot for `=>` is the last entry in KEYWORD_TOKENS.
            let slot = KEYWORD_TOKENS.len() - 1;
            hist.counts[slot] = hist.counts[slot].saturating_add(1);
            hist.total = hist.total.saturating_add(1);
            i += 2;
            continue;
        }
        // Identifier-shaped run; only counted when starting on a non-ident
        // char boundary (or at the start of the slice).
        if c.is_ascii_alphabetic() || c == b'_' || c == b'$' {
            let start = i;
            i += 1;
            while i < bytes.len() && in_ident_char(bytes[i]) {
                i += 1;
            }
            let run = &source[start..i];
            if let Some(slot) = KEYWORD_TOKENS
                .iter()
                .position(|kw| *kw != "=>" && *kw == run)
            {
                hist.counts[slot] = hist.counts[slot].saturating_add(1);
                hist.total = hist.total.saturating_add(1);
            }
            continue;
        }
        i += 1;
    }
    hist
}

/// Cosine similarity over keyword histograms, returned in `[0, 1]`. Two
/// modules with identical histogram shapes (regardless of total counts)
/// score 1.0.
fn keyword_histogram_similarity(a: &KeywordHistogram, b: &KeywordHistogram) -> f64 {
    if a.total == 0 || b.total == 0 {
        return 0.0;
    }
    let mut dot = 0.0_f64;
    let mut sq_a = 0.0_f64;
    let mut sq_b = 0.0_f64;
    for (ax, bx) in a.counts.iter().zip(b.counts.iter()) {
        let af = f64::from(*ax);
        let bf = f64::from(*bx);
        dot += af * bf;
        sq_a += af * af;
        sq_b += bf * bf;
    }
    if sq_a == 0.0 || sq_b == 0.0 {
        return 0.0;
    }
    dot / (sq_a.sqrt() * sq_b.sqrt())
}

/// Pull non-trivial string literals out of a JS/TS source slice. Bundler
/// passes touch identifiers and AST shape but almost never rewrite the
/// contents of `"…"`, `'…'`, or `` `…` `` — so the set of distinct
/// `>=` 4-character strings is an orthogonal identity signal that
/// survives minification, helper inlining, and CJS/ESM wrapping.
///
/// Cheap byte-walk implementation:
/// * track which quote (if any) we are inside;
/// * skip `\\` and `\"` style escapes;
/// * skip template-literal interpolations (`${…}`) so we do not slurp
///   expression text into the corpus;
/// * emit each closed literal whose decoded byte length is >= 4 as a
///   stable 64-bit hash via `reverts_ir::hash::fnv1a_hex`.
fn extract_string_corpus(source: &str) -> BTreeSet<u64> {
    let mut out = BTreeSet::new();
    let bytes = source.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'/' && i + 1 < bytes.len() {
            // Skip block / line comments — keeps the corpus from
            // accidentally picking up comment-internal "strings".
            match bytes[i + 1] {
                b'/' => {
                    i += 2;
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                b'*' => {
                    i += 2;
                    while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                        i += 1;
                    }
                    i = (i + 2).min(bytes.len());
                    continue;
                }
                _ => {}
            }
        }
        if b == b'"' || b == b'\'' || b == b'`' {
            let quote = b;
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() {
                let c = bytes[j];
                if c == b'\\' {
                    j += 2;
                    continue;
                }
                if quote == b'`' && c == b'$' && j + 1 < bytes.len() && bytes[j + 1] == b'{' {
                    // Skip template-literal expression block.
                    let mut depth = 1usize;
                    j += 2;
                    while j < bytes.len() && depth > 0 {
                        match bytes[j] {
                            b'{' => depth += 1,
                            b'}' => depth -= 1,
                            _ => {}
                        }
                        j += 1;
                    }
                    continue;
                }
                if c == quote {
                    break;
                }
                j += 1;
            }
            if j < bytes.len() && bytes[j] == quote {
                let literal = &source[start..j];
                if literal.len() >= 4 {
                    out.insert(reverts_ir::hash::fnv1a(literal.as_bytes()));
                }
                i = j + 1;
                continue;
            } else {
                // Unterminated literal (truncated source slice) — bail.
                break;
            }
        }
        i += 1;
    }
    out
}

fn bag_from_fingerprints(fingerprints: &[FunctionFingerprint]) -> ModuleBag {
    let mut bag = ModuleBag {
        by_axis: BTreeMap::new(),
        function_count: fingerprints.len(),
    };
    for &axis in SCORING_AXES {
        bag.by_axis.insert(axis, BTreeSet::new());
    }
    for fingerprint in fingerprints {
        for &axis in SCORING_AXES {
            let axis_bag = bag.by_axis.get_mut(&axis).expect("axis pre-inserted above");
            if let Some(hash) = fingerprint.primary.get(axis) {
                axis_bag.insert(hash);
            }
            for alt in &fingerprint.alternates {
                if let Some(hash) = alt.axes.get(axis) {
                    axis_bag.insert(hash);
                }
            }
        }
    }
    bag
}

#[derive(Debug, Default)]
struct RecallReport {
    matched: usize,
    score_histogram: BTreeMap<&'static str, usize>,
    winning_axis_counts: BTreeMap<AxisKind, usize>,
}

#[derive(Debug, Default, Clone, Copy)]
struct BestMatch {
    /// Index into the subject bag/module slice (None if ref had no fingerprints
    /// or no axis-level overlap with any subject module).
    subject_idx: Option<usize>,
    score: f64,
    axis: Option<AxisKind>,
}

/// Precomputed weighting structures derived from the subject corpus. The
/// same `ScoringContext` is reused for best-subject scoring and for ad-hoc
/// per-pair scoring (e.g. mispair debugging), so all paths agree on weights.
struct ScoringContext {
    metric: SimilarityMetric,
    combiner: AxisCombiner,
    use_size_prior: bool,
    /// (axis, hash) -> IDF weight (1.0 when IDF disabled).
    hash_weight: HashMap<(AxisKind, u64), f64>,
    /// Per ref bag, per axis: sum of weights across the bag's hashes.
    ref_axis_weight: Vec<BTreeMap<AxisKind, f64>>,
    /// Per subject bag, per axis: sum of weights across the bag's hashes.
    sub_axis_weight: Vec<BTreeMap<AxisKind, f64>>,
    /// Inverted index: (axis, hash) -> subject indexes containing it.
    by_axis_hash: BTreeMap<(AxisKind, u64), Vec<usize>>,
}

fn build_scoring_context(
    ref_bags: &[ModuleBag],
    sub_bags: &[ModuleBag],
    metric: SimilarityMetric,
    combiner: AxisCombiner,
    use_idf: bool,
    use_size_prior: bool,
) -> ScoringContext {
    let mut by_axis_hash: BTreeMap<(AxisKind, u64), Vec<usize>> = BTreeMap::new();
    for (sub_idx, bag) in sub_bags.iter().enumerate() {
        for (axis, hashes) in &bag.by_axis {
            for &hash in hashes {
                by_axis_hash.entry((*axis, hash)).or_default().push(sub_idx);
            }
        }
    }
    let total_subjects = sub_bags.len().max(1) as f64;
    let mut hash_weight: HashMap<(AxisKind, u64), f64> = HashMap::with_capacity(by_axis_hash.len());
    for ((axis, hash), subjects) in &by_axis_hash {
        let weight = if use_idf {
            (total_subjects / subjects.len() as f64).ln().max(0.0)
        } else {
            1.0
        };
        hash_weight.insert((*axis, *hash), weight);
    }
    let ref_axis_weight = weighted_axis_sums(ref_bags, &hash_weight);
    let sub_axis_weight = weighted_axis_sums(sub_bags, &hash_weight);
    ScoringContext {
        metric,
        combiner,
        use_size_prior,
        hash_weight,
        ref_axis_weight,
        sub_axis_weight,
        by_axis_hash,
    }
}

/// Returns the best (subject_idx, score, axis) for each ref module, indexed
/// alongside `ref_bags`. Uses an (axis, hash) inverted index to skip
/// candidates that share no fingerprint hash, so we do not pay O(R · S).
///
/// When `use_idf` is true, hashes are weighted by `ln(N / df(h))`. Hashes
/// shared across most subject modules carry near-zero weight, so common
/// shapes (binding_pattern, structural_anchor) no longer drown out the rare
/// hashes that actually identify a module.
fn score_best_subjects(
    ref_bags: &[ModuleBag],
    sub_bags: &[ModuleBag],
    metric: SimilarityMetric,
    combiner: AxisCombiner,
    use_idf: bool,
    use_size_prior: bool,
) -> (Vec<BestMatch>, ScoringContext) {
    let context = build_scoring_context(
        ref_bags,
        sub_bags,
        metric,
        combiner,
        use_idf,
        use_size_prior,
    );
    let best = score_best_subjects_with(&context, ref_bags, sub_bags);
    (best, context)
}

fn score_best_subjects_with(
    context: &ScoringContext,
    ref_bags: &[ModuleBag],
    sub_bags: &[ModuleBag],
) -> Vec<BestMatch> {
    let hash_weight = &context.hash_weight;
    let by_axis_hash = &context.by_axis_hash;
    let ref_axis_weight = &context.ref_axis_weight;
    let sub_axis_weight = &context.sub_axis_weight;

    let mut out = vec![BestMatch::default(); ref_bags.len()];

    for (ref_idx, ref_bag) in ref_bags.iter().enumerate() {
        if ref_bag.function_count == 0 {
            continue;
        }
        // Per-axis: subject_index -> weighted intersection.
        let mut axis_overlap: BTreeMap<AxisKind, HashMap<usize, f64>> = BTreeMap::new();
        for &axis in SCORING_AXES {
            axis_overlap.insert(axis, HashMap::new());
        }
        for (axis, hashes) in &ref_bag.by_axis {
            let axis_table = axis_overlap.get_mut(axis).expect("axis pre-inserted above");
            for &hash in hashes {
                let weight = hash_weight.get(&(*axis, hash)).copied().unwrap_or(0.0);
                if weight <= 0.0 {
                    continue;
                }
                if let Some(subject_indexes) = by_axis_hash.get(&(*axis, hash)) {
                    for &subject_idx in subject_indexes {
                        *axis_table.entry(subject_idx).or_default() += weight;
                    }
                }
            }
        }

        let mut best_score: f64 = 0.0;
        let mut best_axis: Option<AxisKind> = None;
        let mut best_subject: Option<usize> = None;
        // Union of all subject indexes with any axis-level overlap, dedup'd.
        let mut candidate_indexes: BTreeSet<usize> = BTreeSet::new();
        for table in axis_overlap.values() {
            candidate_indexes.extend(table.keys().copied());
        }
        for sub_idx in candidate_indexes {
            let mut per_axis_scores: [f64; SCORING_AXES.len()] = [0.0; SCORING_AXES.len()];
            let mut ref_active_axes: usize = 0;
            for (idx, &axis) in SCORING_AXES.iter().enumerate() {
                let ref_weight = ref_axis_weight[ref_idx].get(&axis).copied().unwrap_or(0.0);
                if ref_weight <= 0.0 {
                    continue;
                }
                ref_active_axes += 1;
                let sub_weight = sub_axis_weight[sub_idx].get(&axis).copied().unwrap_or(0.0);
                if sub_weight <= 0.0 {
                    continue;
                }
                let intersect = axis_overlap
                    .get(&axis)
                    .and_then(|table| table.get(&sub_idx).copied())
                    .unwrap_or(0.0);
                if intersect <= 0.0 {
                    continue;
                }
                per_axis_scores[idx] = match context.metric {
                    SimilarityMetric::Jaccard => {
                        let union = ref_weight + sub_weight - intersect;
                        if union <= 0.0 { 0.0 } else { intersect / union }
                    }
                    SimilarityMetric::Overlap => intersect / ref_weight.min(sub_weight),
                };
            }
            let (raw_score, winning_axis) =
                combine_axis_scores(&per_axis_scores, ref_active_axes, context.combiner);
            let score = if context.use_size_prior {
                raw_score * size_prior(ref_bag.function_count, sub_bags[sub_idx].function_count)
            } else {
                raw_score
            };
            if score > best_score {
                best_score = score;
                best_axis = winning_axis;
                best_subject = Some(sub_idx);
            }
        }
        out[ref_idx] = BestMatch {
            subject_idx: best_subject,
            score: best_score,
            axis: best_axis,
        };
    }
    out
}

/// Function-count similarity prior in `[0, 1]`. Peaks at 1.0 when both
/// modules have the same number of functions; falls toward 0 as the counts
/// diverge. Used to suppress tiny generic helper modules from beating the
/// genuinely-sized true counterpart on coincidental hash overlap.
// ---------------------------------------------------------------------------
// function-tier strategy (shared FingerprintIndex with the package matcher)
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct FunctionTierReport {
    best: Vec<BestMatch>,
    /// Total subject-module winning votes attributed via each tier across all
    /// ref functions. Mirrors the package matcher's MatchTier ladder without
    /// dragging in the package-specific dedup logic.
    tier_counts: BTreeMap<&'static str, usize>,
}

/// Top-tier weight (Hungarian) used as the per-ref-function ceiling when
/// normalising aggregated module scores. Matches `MatchTier::Exact::weight()`
/// from `reverts-ir`.
const TIER_EXACT_WEIGHT: f64 = 1000.0;

fn score_via_function_tier(
    ref_fps: &[ModuleFingerprints],
    sub_fps: &[ModuleFingerprints],
) -> FunctionTierReport {
    use reverts_package_matcher::{
        try_exact, try_exact_alternate, try_structural_anchored, try_structural_anchored_alternate,
        try_structural_only, try_structural_only_alternate,
    };

    // Build the shared FingerprintIndex over subject functions. Owner =
    // subject module index; tier scoring dedups by `(owner, fn_id)` via the
    // CandidateOwner trait.
    let mut index: FingerprintIndex<ModuleOwner> = FingerprintIndex::new();
    for (sub_idx, m) in sub_fps.iter().enumerate() {
        for fp in &m.raw {
            insert_function_into_index(&mut index, sub_idx, fp);
        }
    }

    // For each ref function, run the package matcher's `try_*` ladder in
    // strongest-to-weakest order, stopping at the first tier with a unique
    // winner. The expensive Jaccard-style `try_feature_similarity` tier is
    // intentionally skipped — at module-corpus sizes its per-candidate
    // inner loop dominates wall-clock without adding meaningful precision
    // (the bag-jaccard strategy already covers that signal more cheaply).
    let mut report = FunctionTierReport::default();
    let mut subject_scores: Vec<HashMap<usize, f64>> = vec![HashMap::new(); ref_fps.len()];
    for (ref_idx, m) in ref_fps.iter().enumerate() {
        for fp in &m.raw {
            let top = try_exact(fp, &index)
                .or_else(|| try_exact_alternate(fp, &index))
                .or_else(|| try_structural_anchored(fp, &index))
                .or_else(|| try_structural_anchored_alternate(fp, &index))
                .or_else(|| try_structural_only(fp, &index))
                .or_else(|| try_structural_only_alternate(fp, &index));
            let Some(top) = top else {
                continue;
            };
            *subject_scores[ref_idx]
                .entry(top.candidate.owner)
                .or_default() += f64::from(top.tier.weight());
            *report.tier_counts.entry(tier_label(&top)).or_default() += 1;
        }
    }

    report.best.resize(ref_fps.len(), BestMatch::default());
    for (ref_idx, ref_module) in ref_fps.iter().enumerate() {
        let scores = &subject_scores[ref_idx];
        let mut best_subject = None;
        let mut best_total = 0.0_f64;
        for (&sub_idx, &total) in scores {
            if total > best_total {
                best_total = total;
                best_subject = Some(sub_idx);
            }
        }
        let denom = (ref_module.raw.len() as f64).max(1.0) * TIER_EXACT_WEIGHT;
        let normalised = (best_total / denom).clamp(0.0, 1.0);
        report.best[ref_idx] = BestMatch {
            subject_idx: best_subject,
            score: normalised,
            axis: None,
        };
    }
    report
}

/// Score ref↔subject pairs with the package matcher's structural-bag
/// scorer. Each ref module's fingerprints become a `StructuralBag`; each
/// subject module's fingerprints become another `StructuralBag`; the
/// shared `score_structural_bags` returns the same weighted composite
/// score `match_structural_bags` uses to rank package versions.
///
/// Per ref, picks the subject with the highest score. Normalised to a
/// rough [0, 1] by the maximum score observed across the run (the
/// underlying scorer is unbounded; the absolute number depends on bag
/// sizes, so we rescale for histogram parity with the other strategies).
fn score_via_structural_bag(
    ref_fps: &[ModuleFingerprints],
    sub_fps: &[ModuleFingerprints],
) -> Vec<BestMatch> {
    use reverts_package_matcher::{StructuralBag, build_structural_bag, score_structural_bags};

    let sub_bags: Vec<Option<StructuralBag>> = sub_fps
        .iter()
        .map(|m| build_structural_bag(&m.raw))
        .collect();
    let ref_bags: Vec<Option<StructuralBag>> = ref_fps
        .iter()
        .map(|m| build_structural_bag(&m.raw))
        .collect();

    // Pre-filter subject candidates via the shared FingerprintIndex on the
    // AST axis: only subjects that share at least one AST hash with the ref
    // module are worth scoring. This turns the worst-case O(R × S)
    // structural-bag scoring (3576 × 1810 ≈ 6.5M pairs on the CC dataset)
    // into O(R × avg-candidates-per-ref), typically a single-digit number.
    let mut sub_ast_index: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (sub_idx, m) in sub_fps.iter().enumerate() {
        for fp in &m.raw {
            sub_ast_index
                .entry(fp.primary.ast)
                .or_default()
                .push(sub_idx);
            for alt in &fp.alternates {
                sub_ast_index.entry(alt.axes.ast).or_default().push(sub_idx);
            }
        }
    }

    let mut raw_scores: Vec<(Option<usize>, f64)> = Vec::with_capacity(ref_bags.len());
    let mut global_max: f64 = 0.0;
    for (ref_idx, ref_bag_opt) in ref_bags.iter().enumerate() {
        let Some(ref_bag) = ref_bag_opt else {
            raw_scores.push((None, 0.0));
            continue;
        };
        let mut candidates: BTreeSet<usize> = BTreeSet::new();
        for fp in &ref_fps[ref_idx].raw {
            if let Some(subs) = sub_ast_index.get(&fp.primary.ast) {
                candidates.extend(subs.iter().copied());
            }
            for alt in &fp.alternates {
                if let Some(subs) = sub_ast_index.get(&alt.axes.ast) {
                    candidates.extend(subs.iter().copied());
                }
            }
        }
        let mut best_score = 0.0_f64;
        let mut best_sub: Option<usize> = None;
        for sub_idx in candidates {
            let Some(sub_bag) = sub_bags[sub_idx].as_ref() else {
                continue;
            };
            if let Some(score) = score_structural_bags(ref_bag, sub_bag)
                && score > best_score
            {
                best_score = score;
                best_sub = Some(sub_idx);
            }
        }
        if best_score > global_max {
            global_max = best_score;
        }
        raw_scores.push((best_sub, best_score));
    }

    let normaliser = if global_max > 0.0 { global_max } else { 1.0 };
    raw_scores
        .into_iter()
        .map(|(subject_idx, score)| BestMatch {
            subject_idx,
            score: (score / normaliser).clamp(0.0, 1.0),
            axis: None,
        })
        .collect()
}

/// Dep-graph anchor propagation. Two-pass:
///
/// 1. Bootstrap with the signal-cascade anchors that already have ≥ 3
///    independent signals agreeing — those pairs are confident enough
///    to seed propagation.
/// 2. Walk each anchor's dependency edges in lockstep: the i-th ref
///    import specifier is proposed as the partner of the i-th subject
///    dep target. A proposal lands when neither side is already claimed
///    by a stronger anchor AND the keyword-histogram cosine on the
///    proposed pair clears 0.6 (cheap validation that we aren't
///    pairing wildly different modules just because they happen to
///    be at the same dep position).
/// 3. New pairs become anchors for the next round; iterate until no
///    new pairs land or a hard step cap is reached.
fn score_via_dep_graph_propagation(
    ref_fps: &[ModuleFingerprints],
    sub_fps: &[ModuleFingerprints],
    spec_to_ref: &BTreeMap<String, usize>,
) -> Vec<BestMatch> {
    // Seed anchors: cascade-3-agree pairs from signal-cascade.
    let seed = score_via_cascade_three_agree(ref_fps, sub_fps);
    let mut anchors: Vec<Option<usize>> = seed.iter().map(|m| m.subject_idx).collect();
    let mut claimed: BTreeSet<usize> = anchors.iter().filter_map(|x| *x).collect();

    const MAX_ROUNDS: usize = 6;
    const PROPAGATE_VALIDATION_MIN: f64 = 0.6;
    let mut last_added = usize::MAX;
    let initial_anchors = anchors.iter().filter(|x| x.is_some()).count();
    let mut diag_ref_no_specs = 0usize;
    let mut diag_sub_no_targets = 0usize;
    let mut diag_spec_unresolved = 0usize;
    let mut diag_already_claimed = 0usize;
    let mut diag_validation_failed = 0usize;
    for _round in 0..MAX_ROUNDS {
        if last_added == 0 {
            break;
        }
        last_added = 0;
        for ref_idx in 0..ref_fps.len() {
            let Some(sub_idx) = anchors[ref_idx] else {
                continue;
            };
            let ref_specs = &ref_fps[ref_idx].dependency_specifiers;
            let sub_targets = &sub_fps[sub_idx].dependency_targets;
            if ref_specs.is_empty() {
                diag_ref_no_specs += 1;
                continue;
            }
            if sub_targets.is_empty() {
                diag_sub_no_targets += 1;
                continue;
            }
            let limit = ref_specs.len().min(sub_targets.len());
            for i in 0..limit {
                let spec = ref_specs[i].as_str();
                let Some(ref_dep_idx) = spec_to_ref.get(spec).copied() else {
                    diag_spec_unresolved += 1;
                    continue;
                };
                let sub_dep_idx = sub_targets[i];
                if anchors[ref_dep_idx].is_some() || claimed.contains(&sub_dep_idx) {
                    diag_already_claimed += 1;
                    continue;
                }
                let validation = keyword_histogram_similarity(
                    &ref_fps[ref_dep_idx].keyword_histogram,
                    &sub_fps[sub_dep_idx].keyword_histogram,
                );
                if validation < PROPAGATE_VALIDATION_MIN {
                    diag_validation_failed += 1;
                    continue;
                }
                anchors[ref_dep_idx] = Some(sub_dep_idx);
                claimed.insert(sub_dep_idx);
                last_added += 1;
            }
        }
    }
    let total_anchors = anchors.iter().filter(|x| x.is_some()).count();
    eprintln!(
        "  dep-graph propagation diag: bootstrap={initial_anchors}, final={total_anchors}, refs-without-specs={diag_ref_no_specs}, subs-without-targets={diag_sub_no_targets}, specifier-unresolved={diag_spec_unresolved}, already-claimed={diag_already_claimed}, validation-failed={diag_validation_failed}"
    );

    anchors
        .into_iter()
        .map(|sub_idx| BestMatch {
            subject_idx: sub_idx,
            score: if sub_idx.is_some() { 1.0 } else { 0.0 },
            axis: None,
        })
        .collect()
}

/// Reuses the signal-cascade vote-counting logic to produce a sparse
/// list of (ref_idx -> sub_idx) for the highest-agreement bucket only.
fn score_via_cascade_three_agree(
    ref_fps: &[ModuleFingerprints],
    sub_fps: &[ModuleFingerprints],
) -> Vec<BestMatch> {
    let ref_bags: Vec<ModuleBag> = ref_fps.iter().map(|m| m.bag.clone()).collect();
    let sub_bags: Vec<ModuleBag> = sub_fps.iter().map(|m| m.bag.clone()).collect();
    let bag_context = build_scoring_context(
        &ref_bags,
        &sub_bags,
        SimilarityMetric::Jaccard,
        AxisCombiner::Mean,
        true,
        false,
    );
    let bag_best = score_best_subjects_with(&bag_context, &ref_bags, &sub_bags);
    let str_best = score_via_string_literal(ref_fps, sub_fps);
    let kw_best = score_via_keyword_histogram(ref_fps, sub_fps);
    let pinned: Vec<Option<usize>> = bag_best
        .iter()
        .map(|m| m.subject_idx.filter(|_| m.score >= 0.20))
        .collect();
    let func_report = score_via_module_pinned_function_tier(ref_fps, sub_fps, &pinned);

    let signals: [&[BestMatch]; 4] = [&bag_best, &str_best, &kw_best, &func_report.best];
    let thresholds: [f64; 4] = [0.20, 0.30, 0.85, 0.30];

    let mut out = vec![BestMatch::default(); ref_fps.len()];
    for ref_idx in 0..ref_fps.len() {
        let mut votes: HashMap<usize, usize> = HashMap::new();
        for (i, signal) in signals.iter().enumerate() {
            let pick = signal[ref_idx];
            if pick.score < thresholds[i] {
                continue;
            }
            if let Some(sub_idx) = pick.subject_idx {
                *votes.entry(sub_idx).or_default() += 1;
            }
        }
        if let Some((&sub_idx, &n)) = votes.iter().max_by_key(|(_, n)| *n)
            && n >= 3
        {
            out[ref_idx] = BestMatch {
                subject_idx: Some(sub_idx),
                score: 1.0,
                axis: None,
            };
        }
    }
    out
}

/// Pair ref modules with subject modules by cosine similarity over the
/// keyword histograms. Pre-filters via shared string-corpus or matching
/// total keyword count to avoid O(R × S) cosine on every pair.
fn score_via_keyword_histogram(
    ref_fps: &[ModuleFingerprints],
    sub_fps: &[ModuleFingerprints],
) -> Vec<BestMatch> {
    // Inverted index: AST hash -> subject indexes. Reuse function-level
    // AST hashes as the prefilter — a ref module with any function-level
    // overlap with a subject is worth scoring; everything else is noise.
    let mut sub_ast_index: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (sub_idx, m) in sub_fps.iter().enumerate() {
        for fp in &m.raw {
            sub_ast_index
                .entry(fp.primary.ast)
                .or_default()
                .push(sub_idx);
            for alt in &fp.alternates {
                sub_ast_index.entry(alt.axes.ast).or_default().push(sub_idx);
            }
        }
    }

    let mut out = vec![BestMatch::default(); ref_fps.len()];
    for (ref_idx, m) in ref_fps.iter().enumerate() {
        if m.keyword_histogram.total == 0 {
            continue;
        }
        let mut candidates: BTreeSet<usize> = BTreeSet::new();
        for fp in &m.raw {
            if let Some(subs) = sub_ast_index.get(&fp.primary.ast) {
                candidates.extend(subs.iter().copied());
            }
            for alt in &fp.alternates {
                if let Some(subs) = sub_ast_index.get(&alt.axes.ast) {
                    candidates.extend(subs.iter().copied());
                }
            }
        }
        let mut best_score = 0.0_f64;
        let mut best_sub = None;
        for sub_idx in candidates {
            let score = keyword_histogram_similarity(
                &m.keyword_histogram,
                &sub_fps[sub_idx].keyword_histogram,
            );
            if score > best_score {
                best_score = score;
                best_sub = Some(sub_idx);
            }
        }
        out[ref_idx] = BestMatch {
            subject_idx: best_sub,
            score: best_score,
            axis: None,
        };
    }
    out
}

/// Pair ref modules with subject modules by string-literal Jaccard. For
/// each ref module, builds an inverted index over subject string corpora
/// (string_hash → subject indexes), looks up each ref string, scores by
/// `|A ∩ B| / |A ∪ B|`, picks max. O(ref_strings + ref × candidates)
/// instead of O(ref × sub).
fn score_via_string_literal(
    ref_fps: &[ModuleFingerprints],
    sub_fps: &[ModuleFingerprints],
) -> Vec<BestMatch> {
    let mut by_string: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (sub_idx, m) in sub_fps.iter().enumerate() {
        for &h in &m.string_corpus {
            by_string.entry(h).or_default().push(sub_idx);
        }
    }

    let mut out = vec![BestMatch::default(); ref_fps.len()];
    for (ref_idx, m) in ref_fps.iter().enumerate() {
        if m.string_corpus.is_empty() {
            continue;
        }
        let mut intersect_count: HashMap<usize, usize> = HashMap::new();
        for &h in &m.string_corpus {
            if let Some(subs) = by_string.get(&h) {
                for &sub_idx in subs {
                    *intersect_count.entry(sub_idx).or_default() += 1;
                }
            }
        }
        let mut best_score = 0.0_f64;
        let mut best_sub = None;
        for (&sub_idx, &intersect) in &intersect_count {
            let union = m.string_corpus.len() + sub_fps[sub_idx].string_corpus.len() - intersect;
            if union == 0 {
                continue;
            }
            let score = intersect as f64 / union as f64;
            if score > best_score {
                best_score = score;
                best_sub = Some(sub_idx);
            }
        }
        out[ref_idx] = BestMatch {
            subject_idx: best_sub,
            score: best_score,
            axis: None,
        };
    }
    out
}

/// Pair ref modules with subject modules by Jaccard over their
/// property-name corpora (class methods, object keys, member accesses
/// — names bundlers don't rewrite).
fn score_via_property_name(
    ref_fps: &[ModuleFingerprints],
    sub_fps: &[ModuleFingerprints],
) -> Vec<BestMatch> {
    let mut by_name: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (sub_idx, m) in sub_fps.iter().enumerate() {
        for &h in &m.property_names {
            by_name.entry(h).or_default().push(sub_idx);
        }
    }

    let mut out = vec![BestMatch::default(); ref_fps.len()];
    for (ref_idx, m) in ref_fps.iter().enumerate() {
        if m.property_names.is_empty() {
            continue;
        }
        let mut intersect: HashMap<usize, usize> = HashMap::new();
        for &h in &m.property_names {
            if let Some(subs) = by_name.get(&h) {
                for &sub_idx in subs {
                    *intersect.entry(sub_idx).or_default() += 1;
                }
            }
        }
        let mut best_score = 0.0_f64;
        let mut best_sub = None;
        for (&sub_idx, &i) in &intersect {
            let union = m.property_names.len() + sub_fps[sub_idx].property_names.len() - i;
            if union == 0 {
                continue;
            }
            let score = i as f64 / union as f64;
            if score > best_score {
                best_score = score;
                best_sub = Some(sub_idx);
            }
        }
        out[ref_idx] = BestMatch {
            subject_idx: best_sub,
            score: best_score,
            axis: None,
        };
    }
    out
}

/// Rare-AST-anchor pin: walk each ref module's AST hashes, drop hashes
/// shared by many subject modules (vendor/duplicate noise), and pin to
/// the subject module that holds the most distinct rare hashes. Encodes
/// "if you have a smoking-gun function, that pins the module".
///
/// Score is the count of rare-hash hits (not a Jaccard fraction) — the
/// caller compares against a small absolute threshold.
fn score_via_rare_ast_anchor(ref_bags: &[ModuleBag], sub_bags: &[ModuleBag]) -> Vec<BestMatch> {
    // Subject-side inverted index: hash -> list of modules containing it.
    let mut by_hash: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (sub_idx, bag) in sub_bags.iter().enumerate() {
        if let Some(hashes) = bag.by_axis.get(&AxisKind::Ast) {
            for &h in hashes {
                by_hash.entry(h).or_default().push(sub_idx);
            }
        }
    }
    // A hash is "rare" when ≤ MAX_HOLDERS subject modules contain it.
    // Tuned conservatively: this is the anchor threshold, not a Jaccard.
    const MAX_HOLDERS: usize = 4;

    let mut out = vec![BestMatch::default(); ref_bags.len()];
    for (ref_idx, bag) in ref_bags.iter().enumerate() {
        let Some(hashes) = bag.by_axis.get(&AxisKind::Ast) else {
            continue;
        };
        let mut hits: HashMap<usize, usize> = HashMap::new();
        for &h in hashes {
            if let Some(holders) = by_hash.get(&h)
                && holders.len() <= MAX_HOLDERS
            {
                for &sub_idx in holders {
                    *hits.entry(sub_idx).or_default() += 1;
                }
            }
        }
        let mut best_count = 0usize;
        let mut best_sub: Option<usize> = None;
        for (&sub_idx, &c) in &hits {
            if c > best_count {
                best_count = c;
                best_sub = Some(sub_idx);
            }
        }
        out[ref_idx] = BestMatch {
            subject_idx: best_sub,
            score: best_count as f64,
            axis: None,
        };
    }
    out
}

fn print_property_corpus_stats(ref_fps: &[ModuleFingerprints], sub_fps: &[ModuleFingerprints]) {
    let ref_total: usize = ref_fps.iter().map(|m| m.property_names.len()).sum();
    let ref_nonempty = ref_fps
        .iter()
        .filter(|m| !m.property_names.is_empty())
        .count();
    let sub_total: usize = sub_fps.iter().map(|m| m.property_names.len()).sum();
    let sub_nonempty = sub_fps
        .iter()
        .filter(|m| !m.property_names.is_empty())
        .count();
    let ref_distinct: BTreeSet<u64> = ref_fps
        .iter()
        .flat_map(|m| m.property_names.iter().copied())
        .collect();
    let sub_distinct: BTreeSet<u64> = sub_fps
        .iter()
        .flat_map(|m| m.property_names.iter().copied())
        .collect();
    let shared = ref_distinct.intersection(&sub_distinct).count();
    println!(
        "  property-corpus stats: ref {} names / {} modules ({} distinct); subject {} names / {} modules ({} distinct); {} hashes shared across sides",
        ref_total,
        ref_nonempty,
        ref_distinct.len(),
        sub_total,
        sub_nonempty,
        sub_distinct.len(),
        shared,
    );
}

fn print_string_corpus_stats(ref_fps: &[ModuleFingerprints], sub_fps: &[ModuleFingerprints]) {
    let ref_total: usize = ref_fps.iter().map(|m| m.string_corpus.len()).sum();
    let ref_nonempty = ref_fps
        .iter()
        .filter(|m| !m.string_corpus.is_empty())
        .count();
    let sub_total: usize = sub_fps.iter().map(|m| m.string_corpus.len()).sum();
    let sub_nonempty = sub_fps
        .iter()
        .filter(|m| !m.string_corpus.is_empty())
        .count();
    let ref_distinct: BTreeSet<u64> = ref_fps
        .iter()
        .flat_map(|m| m.string_corpus.iter().copied())
        .collect();
    let sub_distinct: BTreeSet<u64> = sub_fps
        .iter()
        .flat_map(|m| m.string_corpus.iter().copied())
        .collect();
    let shared = ref_distinct.intersection(&sub_distinct).count();
    println!(
        "  string-corpus stats: ref {} strings / {} modules ({} distinct); subject {} strings / {} modules ({} distinct); {} hashes shared across sides",
        ref_total,
        ref_nonempty,
        ref_distinct.len(),
        sub_total,
        sub_nonempty,
        sub_distinct.len(),
        shared,
    );
}

/// Module-pinned function-tier scoring. For each ref module that has a
/// `pinned` subject partner, build a fingerprint index over JUST that
/// subject module's functions and run the shared cascade against it. With
/// the candidate set restricted from "all 27k subject functions" to "~10
/// functions inside the partner module" the unique-winner condition
/// becomes easy to satisfy, so the per-function naming coverage's
/// `tier_unique` line typically jumps an order of magnitude.
fn score_via_module_pinned_function_tier(
    ref_fps: &[ModuleFingerprints],
    sub_fps: &[ModuleFingerprints],
    pinned: &[Option<usize>],
) -> FunctionTierReport {
    use reverts_package_matcher::{
        try_exact, try_exact_alternate, try_structural_anchored, try_structural_anchored_alternate,
        try_structural_only, try_structural_only_alternate,
    };

    let mut report = FunctionTierReport::default();
    report.best.resize(ref_fps.len(), BestMatch::default());

    for (ref_idx, ref_module) in ref_fps.iter().enumerate() {
        let Some(sub_idx) = pinned[ref_idx] else {
            continue;
        };
        if ref_module.raw.is_empty() {
            continue;
        }
        let sub_module = &sub_fps[sub_idx];
        if sub_module.raw.is_empty() {
            continue;
        }

        // Per-pair mini-index. ModuleOwner uses the actual subject index so
        // the dedup key matches what aggregations would expect, even though
        // every candidate here resolves to a single owner.
        let mut index: FingerprintIndex<ModuleOwner> = FingerprintIndex::new();
        for fp in &sub_module.raw {
            insert_function_into_index(&mut index, sub_idx, fp);
        }

        let mut total = 0.0_f64;
        for fp in &ref_module.raw {
            let top = try_exact(fp, &index)
                .or_else(|| try_exact_alternate(fp, &index))
                .or_else(|| try_structural_anchored(fp, &index))
                .or_else(|| try_structural_anchored_alternate(fp, &index))
                .or_else(|| try_structural_only(fp, &index))
                .or_else(|| try_structural_only_alternate(fp, &index));
            let Some(top) = top else {
                continue;
            };
            total += f64::from(top.tier.weight());
            *report.tier_counts.entry(tier_label(&top)).or_default() += 1;
        }
        let denom = (ref_module.raw.len() as f64).max(1.0) * TIER_EXACT_WEIGHT;
        let normalised = (total / denom).clamp(0.0, 1.0);
        report.best[ref_idx] = BestMatch {
            subject_idx: Some(sub_idx),
            score: normalised,
            axis: None,
        };
    }
    report
}

/// Per-function naming-coverage report. Answers: "how many of the ref
/// project's functions are reachable for cross-version name assignment, and
/// at what level of confidence?".
///
/// Three layers, narrow → wide:
/// * `tier_unique`  — function-tier produced exactly one winning subject
///   for this ref function. Name can be transferred 1:1, high confidence.
/// * `any_ast_match` — at least one subject function shares this ref
///   function's AST hash (primary or alt pass). Still narrow but not
///   unique; downstream can pick by majority / Hungarian / module pin.
/// * `module_matched` — the ref function lives inside a module the
///   function-tier strategy picked a subject for. Every function in such a
///   module inherits the subject module's surface, so they can be named
///   via module-level transfer even if the function itself is unmatched.
fn print_function_naming_coverage(
    report: &FunctionTierReport,
    ref_fps: &[ModuleFingerprints],
    sub_fps: &[ModuleFingerprints],
) {
    let total_fns: usize = ref_fps.iter().map(|m| m.raw.len()).sum();
    if total_fns == 0 {
        println!("  function naming coverage: 0 ref functions to evaluate");
        return;
    }

    let tier_unique_low: usize = report.tier_counts.values().sum();
    let tier_unique_medium: usize = report
        .tier_counts
        .iter()
        .filter(|(label, _)| label_clears_confidence(label, TierConfidence::Medium))
        .map(|(_, count)| *count)
        .sum();
    let tier_unique_high: usize = report
        .tier_counts
        .iter()
        .filter(|(label, _)| label_clears_confidence(label, TierConfidence::High))
        .map(|(_, count)| *count)
        .sum();

    // Subject AST-hash universe: which AST hashes appear anywhere in
    // subject (primary or alt). A ref function whose AST hash is in here
    // has at least one content-equal subject candidate.
    let mut subject_ast: BTreeSet<u64> = BTreeSet::new();
    for m in sub_fps {
        for fp in &m.raw {
            subject_ast.insert(fp.primary.ast);
            for alt in &fp.alternates {
                subject_ast.insert(alt.axes.ast);
            }
        }
    }
    let mut any_ast_match: usize = 0;
    for m in ref_fps {
        for fp in &m.raw {
            let any = subject_ast.contains(&fp.primary.ast)
                || fp
                    .alternates
                    .iter()
                    .any(|alt| subject_ast.contains(&alt.axes.ast));
            if any {
                any_ast_match += 1;
            }
        }
    }

    // Functions covered transitively via module-level match: their parent
    // ref module produced *any* winning subject pick.
    let module_matched: usize = report
        .best
        .iter()
        .enumerate()
        .filter_map(|(ref_idx, best)| best.subject_idx.map(|_| ref_idx))
        .map(|ref_idx| ref_fps[ref_idx].raw.len())
        .sum();

    println!(
        "  function naming coverage: {} ref functions total\n    tier_unique high   (exact / exact_alt only):              {} ({:.2}%)\n    tier_unique medium (+ structural_anchored*):             {} ({:.2}%)\n    tier_unique low    (+ structural_only*, default cascade): {} ({:.2}%)\n    any_ast_match (≥1 content-equal subject fn):             {} ({:.2}%)\n    inside module_matched (inherit via module pick):         {} ({:.2}%)",
        total_fns,
        tier_unique_high,
        pct(tier_unique_high, total_fns),
        tier_unique_medium,
        pct(tier_unique_medium, total_fns),
        tier_unique_low,
        pct(tier_unique_low, total_fns),
        any_ast_match,
        pct(any_ast_match, total_fns),
        module_matched,
        pct(module_matched, total_fns),
    );
}

/// Maps a `MatchTier` to the stable string label printed in the recall
/// report. Matches the package matcher's `MatchTier` enum (in `reverts-ir`).
fn tier_label(m: &reverts_package_matcher::FunctionMatch<ModuleOwner>) -> &'static str {
    use reverts_ir::MatchTier;
    match m.tier {
        MatchTier::Exact => "exact",
        MatchTier::ExactAlternate => "exact_alt",
        MatchTier::StructuralAnchored => "structural_anchored",
        MatchTier::StructuralAnchoredAlternate => "structural_anchored_alt",
        MatchTier::FeatureSimilarity => "feature_similarity",
        MatchTier::FeatureSimilarityAlternate => "feature_similarity_alt",
        MatchTier::StructuralOnly => "structural_only",
        MatchTier::StructuralOnlyAlternate => "structural_only_alt",
    }
}

/// True iff a tier label clears the requested confidence bar. Names are
/// the same strings the per-tier counter uses, so we can filter
/// `tier_counts` directly without re-walking match results.
fn label_clears_confidence(label: &str, min: TierConfidence) -> bool {
    match min {
        TierConfidence::Low => true,
        TierConfidence::Medium => matches!(
            label,
            "exact" | "exact_alt" | "structural_anchored" | "structural_anchored_alt"
        ),
        TierConfidence::High => matches!(label, "exact" | "exact_alt"),
    }
}

fn insert_function_into_index(
    index: &mut FingerprintIndex<ModuleOwner>,
    sub_idx: ModuleOwner,
    fp: &FunctionFingerprint,
) {
    // Unique-per-function id within (sub_idx). The (sub_idx, byte_start)
    // pair is unique because each function occupies a distinct span in its
    // module's source slice.
    let fn_id = u64::from(fp.id.span.start);
    let make = |axis: AxisKind, alt: Option<reverts_ir::NormalizationPassId>| Candidate {
        owner: sub_idx,
        external_function_id: fn_id,
        matched_axis: axis,
        matched_alternate: alt,
    };

    index.insert_exact(
        ExactKey {
            param_count: fp.param_count,
            statement_count: fp.statement_count,
            ast_hash: fp.primary.ast,
        },
        make(AxisKind::Ast, None),
    );
    index.insert_cfg(
        CfgKey {
            param_count: fp.param_count,
            cfg_hash: fp.primary.cfg,
        },
        make(AxisKind::Cfg, None),
    );
    index.insert_structural(
        StructuralKey {
            param_count: fp.param_count,
            structural_anchor: fp.primary.structural_anchor,
        },
        make(AxisKind::StructuralAnchor, None),
    );
    for axis in [
        AxisKind::LiteralAnchor,
        AxisKind::CalleeSet,
        AxisKind::ThrowSet,
        AxisKind::ReturnPattern,
        AxisKind::EffectPattern,
        AxisKind::BindingPattern,
    ] {
        if let Some(hash) = fp.primary.get(axis) {
            index.insert_feature(
                FeatureKey {
                    param_count: fp.param_count,
                    kind: axis,
                    hash,
                },
                make(axis, None),
            );
        }
    }
    for alt in &fp.alternates {
        index.insert_exact(
            ExactKey {
                param_count: fp.param_count,
                statement_count: alt.statement_count,
                ast_hash: alt.axes.ast,
            },
            make(AxisKind::Ast, Some(alt.pass)),
        );
        index.insert_cfg(
            CfgKey {
                param_count: fp.param_count,
                cfg_hash: alt.axes.cfg,
            },
            make(AxisKind::Cfg, Some(alt.pass)),
        );
        index.insert_structural(
            StructuralKey {
                param_count: fp.param_count,
                structural_anchor: alt.axes.structural_anchor,
            },
            make(AxisKind::StructuralAnchor, Some(alt.pass)),
        );
        for axis in [
            AxisKind::LiteralAnchor,
            AxisKind::CalleeSet,
            AxisKind::ThrowSet,
            AxisKind::ReturnPattern,
            AxisKind::EffectPattern,
            AxisKind::BindingPattern,
        ] {
            if let Some(hash) = alt.axes.get(axis) {
                index.insert_feature(
                    FeatureKey {
                        param_count: fp.param_count,
                        kind: axis,
                        hash,
                    },
                    make(axis, Some(alt.pass)),
                );
            }
        }
    }
}

fn print_tier_breakdown(tier_counts: &BTreeMap<&'static str, usize>) {
    let mut entries: Vec<_> = tier_counts.iter().collect();
    entries.sort_by_key(|(label, _)| *label);
    let line = entries
        .iter()
        .map(|(label, count)| format!("{label}: {count}"))
        .collect::<Vec<_>>()
        .join("  ");
    println!("  function-match tier wins: {line}");
}

fn size_prior(ref_fp: usize, sub_fp: usize) -> f64 {
    if ref_fp == 0 || sub_fp == 0 {
        return 0.5;
    }
    let diff = ref_fp.abs_diff(sub_fp) as f64;
    let total = (ref_fp + sub_fp) as f64;
    (1.0 - diff / total).max(0.1)
}

fn weighted_axis_sums(
    bags: &[ModuleBag],
    hash_weight: &HashMap<(AxisKind, u64), f64>,
) -> Vec<BTreeMap<AxisKind, f64>> {
    bags.iter()
        .map(|bag| {
            let mut by_axis = BTreeMap::new();
            for (axis, hashes) in &bag.by_axis {
                let sum: f64 = hashes
                    .iter()
                    .map(|h| hash_weight.get(&(*axis, *h)).copied().unwrap_or(0.0))
                    .sum();
                by_axis.insert(*axis, sum);
            }
            by_axis
        })
        .collect()
}

fn combine_axis_scores(
    per_axis: &[f64; SCORING_AXES.len()],
    ref_active_axes: usize,
    combiner: AxisCombiner,
) -> (f64, Option<AxisKind>) {
    let mut best_axis_score = 0.0_f64;
    let mut best_axis: Option<AxisKind> = None;
    let mut sum = 0.0_f64;
    let mut agreement_count = 0usize;
    for (idx, &score) in per_axis.iter().enumerate() {
        if score > best_axis_score {
            best_axis_score = score;
            best_axis = Some(SCORING_AXES[idx]);
        }
        sum += score;
        if score >= 0.5 {
            agreement_count += 1;
        }
    }
    match combiner {
        AxisCombiner::Max => (best_axis_score, best_axis),
        AxisCombiner::Mean => {
            let score = if ref_active_axes == 0 {
                0.0
            } else {
                sum / ref_active_axes as f64
            };
            (score, best_axis)
        }
        AxisCombiner::Agreement => (
            agreement_count as f64 / SCORING_AXES.len() as f64,
            best_axis,
        ),
    }
}

fn summarise_recall(best_per_ref: &[BestMatch], threshold: f64) -> RecallReport {
    let mut report = RecallReport::default();
    for label in HISTOGRAM_LABELS {
        report.score_histogram.insert(label, 0);
    }
    for best in best_per_ref {
        if best.score == 0.0 && best.subject_idx.is_none() {
            continue;
        }
        for &label in HISTOGRAM_LABELS {
            if best.score >= histogram_bound(label) {
                *report.score_histogram.entry(label).or_default() += 1;
            }
        }
        if best.score >= threshold {
            report.matched += 1;
            if let Some(axis) = best.axis {
                *report.winning_axis_counts.entry(axis).or_default() += 1;
            }
        }
    }
    report
}

#[derive(Debug, Default)]
struct PrecisionReport {
    /// Ref modules whose semantic_name is also held by some subject module.
    baseline_universe: usize,
    /// Of those, ref modules whose best subject pick has the same semantic_name.
    correctly_paired: usize,
    /// Of those, ref modules whose best subject pick has a different name
    /// (counted as confusions).
    mispaired: usize,
    /// Of those, ref modules whose scorer returned no candidate at all.
    unranked: usize,
    /// Baseline pairs where the same-named subject also has confident content
    /// overlap with the ref (truth_score >= 0.3). These are the pairs where
    /// the baseline really is a valid ground-truth pairing.
    verified_universe: usize,
    /// Within `verified_universe`, ref modules whose best pick is the named
    /// subject — i.e. the matcher genuinely identifies the right counterpart
    /// when the counterpart exists.
    verified_correctly_paired: usize,
}

const VERIFIED_TRUTH_SCORE_THRESHOLD: f64 = 0.3;

fn precision_against_baseline(
    ref_modules: &[ModuleRecord],
    sub_modules: &[ModuleRecord],
    ref_bags: &[ModuleBag],
    sub_bags: &[ModuleBag],
    best_per_ref: &[BestMatch],
    context: Option<&ScoringContext>,
) -> PrecisionReport {
    let mut subject_names_by_name: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (idx, module) in sub_modules.iter().enumerate() {
        if let Some(name) = module.semantic_name.as_deref() {
            subject_names_by_name.entry(name).or_default().push(idx);
        }
    }

    let mut report = PrecisionReport::default();
    for (ref_idx, ref_module) in ref_modules.iter().enumerate() {
        let Some(name) = ref_module.semantic_name.as_deref() else {
            continue;
        };
        let Some(truth_indexes) = subject_names_by_name.get(name) else {
            continue;
        };
        report.baseline_universe += 1;
        let best = best_per_ref[ref_idx];

        // Verified universe: same-named subject also has confident content
        // overlap, so the baseline is a real pairing. Only the bag-Jaccard
        // strategy carries a `ScoringContext`; the function-tier strategy
        // reports the same `baseline / correctly_paired` numbers but skips
        // the verified-universe slice (no per-pair score to threshold on).
        let verified = if let Some(ctx) = context {
            let mut best_truth_score = 0.0_f64;
            for &truth_idx in truth_indexes {
                let truth_score = score_pair(
                    ref_idx,
                    truth_idx,
                    &ref_bags[ref_idx],
                    &sub_bags[truth_idx],
                    ctx,
                );
                if truth_score > best_truth_score {
                    best_truth_score = truth_score;
                }
            }
            best_truth_score >= VERIFIED_TRUTH_SCORE_THRESHOLD
        } else {
            false
        };
        if verified {
            report.verified_universe += 1;
        }

        let Some(subject_idx) = best.subject_idx else {
            report.unranked += 1;
            continue;
        };
        let picked_name = sub_modules[subject_idx].semantic_name.as_deref();
        if picked_name == Some(name) {
            report.correctly_paired += 1;
            if verified {
                report.verified_correctly_paired += 1;
            }
        } else {
            report.mispaired += 1;
        }
    }
    report
}

/// Counts how many ref modules / ref functions have AT LEAST ONE per-axis
/// fingerprint hash that exactly appears in some subject module's
/// fingerprint bag (any axis-matched). Reported per axis and unioned.
///
/// This is the "no Jaccard" floor: how much of the ref corpus can be linked
/// to the subject purely on the strength of exact hash identity (across the
/// normalization passes that union into each bag).
fn print_exact_match_summary(ref_bags: &[ModuleBag], sub_bags: &[ModuleBag]) {
    let mut subject_hashes: BTreeMap<AxisKind, BTreeSet<u64>> = BTreeMap::new();
    for &axis in SCORING_AXES {
        subject_hashes.insert(axis, BTreeSet::new());
    }
    for bag in sub_bags {
        for (axis, hashes) in &bag.by_axis {
            let target = subject_hashes
                .get_mut(axis)
                .expect("axis pre-inserted above");
            for &h in hashes {
                target.insert(h);
            }
        }
    }

    let mut per_axis_module_hits: BTreeMap<AxisKind, usize> = BTreeMap::new();
    let mut per_axis_hash_hits: BTreeMap<AxisKind, usize> = BTreeMap::new();
    let mut per_axis_hash_total: BTreeMap<AxisKind, usize> = BTreeMap::new();
    for &axis in SCORING_AXES {
        per_axis_module_hits.insert(axis, 0);
        per_axis_hash_hits.insert(axis, 0);
        per_axis_hash_total.insert(axis, 0);
    }
    let mut any_axis_module_hits = 0usize;
    let mut fingerprintable_modules = 0usize;
    for bag in ref_bags {
        if bag.function_count == 0 {
            continue;
        }
        fingerprintable_modules += 1;
        let mut any_axis_hit = false;
        for (axis, hashes) in &bag.by_axis {
            let total = per_axis_hash_total
                .get_mut(axis)
                .expect("axis pre-inserted above");
            *total += hashes.len();
            let subject_set = subject_hashes.get(axis).expect("axis pre-inserted above");
            let module_hit_count = hashes.iter().filter(|h| subject_set.contains(*h)).count();
            if module_hit_count > 0 {
                *per_axis_module_hits
                    .get_mut(axis)
                    .expect("axis pre-inserted above") += 1;
                *per_axis_hash_hits
                    .get_mut(axis)
                    .expect("axis pre-inserted above") += module_hit_count;
                any_axis_hit = true;
            }
        }
        if any_axis_hit {
            any_axis_module_hits += 1;
        }
    }
    println!(
        "[exact match (no Jaccard)]: {} / {} fingerprintable ref modules have >= 1 axis-hash present in subject ({:.2}%)",
        any_axis_module_hits,
        fingerprintable_modules,
        pct(any_axis_module_hits, fingerprintable_modules),
    );
    let mut axes: Vec<_> = per_axis_module_hits.iter().collect();
    axes.sort_by_key(|(axis, _)| axis.as_str());
    let module_line = axes
        .iter()
        .map(|(axis, hits)| {
            format!(
                "{}: {}/{} ({:.0}%)",
                axis.as_str(),
                hits,
                fingerprintable_modules,
                pct(**hits, fingerprintable_modules),
            )
        })
        .collect::<Vec<_>>()
        .join("  ");
    println!("  per-axis ref-module exact-hit rate: {module_line}");
    let hash_line = axes
        .iter()
        .map(|(axis, _)| {
            let hits = per_axis_hash_hits.get(axis).copied().unwrap_or(0);
            let total = per_axis_hash_total.get(axis).copied().unwrap_or(0);
            format!(
                "{}: {hits}/{total} ({:.0}%)",
                axis.as_str(),
                pct(hits, total),
            )
        })
        .collect::<Vec<_>>()
        .join("  ");
    println!("  per-axis ref-hash exact-hit rate: {hash_line}");
}

fn score_pair(
    ref_idx: usize,
    sub_idx: usize,
    ref_bag: &ModuleBag,
    sub_bag: &ModuleBag,
    context: &ScoringContext,
) -> f64 {
    let mut per_axis_scores: [f64; SCORING_AXES.len()] = [0.0; SCORING_AXES.len()];
    let mut ref_active_axes: usize = 0;
    for (idx, &axis) in SCORING_AXES.iter().enumerate() {
        let ref_weight = context.ref_axis_weight[ref_idx]
            .get(&axis)
            .copied()
            .unwrap_or(0.0);
        if ref_weight <= 0.0 {
            continue;
        }
        ref_active_axes += 1;
        let sub_weight = context.sub_axis_weight[sub_idx]
            .get(&axis)
            .copied()
            .unwrap_or(0.0);
        if sub_weight <= 0.0 {
            continue;
        }
        // Direct intersection by walking the smaller axis bag against the
        // larger one. Cheaper than rebuilding the per-axis inverted index.
        let (small, large) = match (ref_bag.by_axis.get(&axis), sub_bag.by_axis.get(&axis)) {
            (Some(r), Some(s)) if r.len() <= s.len() => (r, s),
            (Some(r), Some(s)) => (s, r),
            _ => continue,
        };
        let mut intersect: f64 = 0.0;
        for hash in small {
            if large.contains(hash) {
                intersect += context
                    .hash_weight
                    .get(&(axis, *hash))
                    .copied()
                    .unwrap_or(0.0);
            }
        }
        if intersect <= 0.0 {
            continue;
        }
        per_axis_scores[idx] = match context.metric {
            SimilarityMetric::Jaccard => {
                let union = ref_weight + sub_weight - intersect;
                if union <= 0.0 { 0.0 } else { intersect / union }
            }
            SimilarityMetric::Overlap => intersect / ref_weight.min(sub_weight),
        };
    }
    let (raw_score, _) = combine_axis_scores(&per_axis_scores, ref_active_axes, context.combiner);
    if context.use_size_prior {
        raw_score * size_prior(ref_bag.function_count, sub_bag.function_count)
    } else {
        raw_score
    }
}

fn print_mispair_samples(
    ref_modules: &[ModuleRecord],
    sub_modules: &[ModuleRecord],
    ref_bags: &[ModuleBag],
    sub_bags: &[ModuleBag],
    best_per_ref: &[BestMatch],
    context: &ScoringContext,
    limit: usize,
) {
    let mut subject_by_name: BTreeMap<&str, usize> = BTreeMap::new();
    for (idx, module) in sub_modules.iter().enumerate() {
        if let Some(name) = module.semantic_name.as_deref() {
            subject_by_name.entry(name).or_insert(idx);
        }
    }
    println!("--- mispair samples ---");
    let mut shown = 0;
    for (ref_idx, ref_module) in ref_modules.iter().enumerate() {
        if shown >= limit {
            break;
        }
        let Some(name) = ref_module.semantic_name.as_deref() else {
            continue;
        };
        let Some(&truth_idx) = subject_by_name.get(name) else {
            continue;
        };
        let best = best_per_ref[ref_idx];
        let picked = best.subject_idx.map(|idx| {
            sub_modules[idx]
                .semantic_name
                .as_deref()
                .unwrap_or("<unnamed>")
        });
        let picked_name = picked.unwrap_or("<no pick>");
        if picked == Some(name) {
            continue;
        }
        let ref_fp = ref_bags[ref_idx].function_count;
        let truth_fp = sub_bags[truth_idx].function_count;
        let picked_fp = best
            .subject_idx
            .map_or(0, |idx| sub_bags[idx].function_count);
        let axis = best.axis.map(AxisKind::as_str).unwrap_or("-");
        let truth_score = score_pair(
            ref_idx,
            truth_idx,
            &ref_bags[ref_idx],
            &sub_bags[truth_idx],
            context,
        );
        println!(
            "  ref={name} (fps={ref_fp}) truth_sub={name} (fps={truth_fp}, score={truth_score:.3}) picked_sub={picked_name} (fps={picked_fp}, score={:.3}) axis={axis}",
            best.score,
        );
        shown += 1;
    }
    if shown == 0 {
        println!("  (no mispairs)");
    }
}

fn print_precision(report: &PrecisionReport) {
    if report.baseline_universe == 0 {
        println!("[precision @ baseline pairs]: 0 baseline-true pairs to verify");
        return;
    }
    println!(
        "[precision @ baseline pairs]: {} / {} correctly paired ({:.2}%) — mispaired={} unranked={}",
        report.correctly_paired,
        report.baseline_universe,
        pct(report.correctly_paired, report.baseline_universe),
        report.mispaired,
        report.unranked,
    );
    // The verified universe filters out baseline pairs where the cli's
    // same-name attribution is itself noise (the named subject has no real
    // content overlap with the ref module). What remains is genuine
    // ground-truth pairs the matcher should hit.
    if report.verified_universe == 0 {
        println!(
            "[verified precision]: 0 / 0 — no baseline pairs reached truth_score >= {VERIFIED_TRUTH_SCORE_THRESHOLD:.2}; the same-name attributions in the subject have no content overlap with the ref"
        );
    } else {
        println!(
            "[verified precision]: {} / {} ({:.2}%) — restricted to baseline pairs whose same-name subject has truth_score >= {VERIFIED_TRUTH_SCORE_THRESHOLD:.2}",
            report.verified_correctly_paired,
            report.verified_universe,
            pct(report.verified_correctly_paired, report.verified_universe),
        );
    }
}

const HISTOGRAM_LABELS: &[&str] = &["≥0.1", "≥0.3", "≥0.5", "≥0.7", "≥0.9"];

fn histogram_bound(label: &str) -> f64 {
    match label {
        "≥0.1" => 0.1,
        "≥0.3" => 0.3,
        "≥0.5" => 0.5,
        "≥0.7" => 0.7,
        "≥0.9" => 0.9,
        _ => f64::INFINITY,
    }
}

fn print_histogram(histogram: &BTreeMap<&'static str, usize>, total: usize) {
    let mut entries: Vec<_> = histogram.iter().collect();
    entries.sort_by_key(|(label, _)| *label);
    let line = entries
        .iter()
        .map(|(label, count)| format!("{label}: {count} ({:.1}%)", pct(**count, total)))
        .collect::<Vec<_>>()
        .join("  ");
    println!("  similarity histogram: {line}");
}

fn print_axis_winners(counts: &BTreeMap<AxisKind, usize>) {
    if counts.is_empty() {
        return;
    }
    let mut entries: Vec<_> = counts.iter().collect();
    entries.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let line = entries
        .iter()
        .map(|(axis, count)| format!("{}: {count}", axis.as_str()))
        .collect::<Vec<_>>()
        .join("  ");
    println!("  winning axis: {line}");
}
