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
use reverts_graph::FunctionExtractor;
use reverts_ir::{AxisKind, FunctionFingerprint, ModuleId};
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
    let ref_bags = fingerprint_modules(&ref_modules);
    let sub_bags = fingerprint_modules(&sub_modules);
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
    let (best_per_ref, scoring_context) = score_best_subjects(
        &ref_bags,
        &sub_bags,
        args.metric,
        args.combiner,
        use_idf,
        use_size_prior,
    );
    let recall = summarise_recall(&best_per_ref, threshold);
    println!(
        "[multi_axis_{}_{}{}{} >= {:.2}]: {} / {} ref modules matched ({:.2}%) — scoring in {:.2}s",
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

    let precision = precision_against_baseline(&ref_modules, &sub_modules, &best_per_ref);
    print_precision(&precision);
    if args.show_mispairs > 0 {
        print_mispair_samples(
            &ref_modules,
            &sub_modules,
            &ref_bags,
            &sub_bags,
            &best_per_ref,
            &scoring_context,
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

fn fingerprint_modules(modules: &[ModuleRecord]) -> Vec<ModuleBag> {
    let mut source_cache: HashMap<String, Option<String>> = HashMap::new();
    let mut out = Vec::with_capacity(modules.len());
    for module in modules {
        let source = source_cache
            .entry(module.file_path.clone())
            .or_insert_with(|| fs::read_to_string(&module.file_path).ok());
        let Some(source_text) = source.as_deref() else {
            out.push(ModuleBag::default());
            continue;
        };
        let start = module.byte_start as usize;
        let end = module.byte_end as usize;
        let slice = source_text
            .get(start..end.min(source_text.len()))
            .filter(|slice| !slice.is_empty());
        let Some(slice) = slice else {
            out.push(ModuleBag::default());
            continue;
        };
        let module_id = ModuleId(u32::try_from(module.id).unwrap_or(u32::MAX));
        let fingerprints = FunctionExtractor::fingerprint(module_id, slice);
        out.push(bag_from_fingerprints(&fingerprints));
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
}

fn precision_against_baseline(
    ref_modules: &[ModuleRecord],
    sub_modules: &[ModuleRecord],
    best_per_ref: &[BestMatch],
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
        if !subject_names_by_name.contains_key(name) {
            continue;
        }
        report.baseline_universe += 1;
        let best = best_per_ref[ref_idx];
        let Some(subject_idx) = best.subject_idx else {
            report.unranked += 1;
            continue;
        };
        let picked_name = sub_modules[subject_idx].semantic_name.as_deref();
        if picked_name == Some(name) {
            report.correctly_paired += 1;
        } else {
            report.mispaired += 1;
        }
    }
    report
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
