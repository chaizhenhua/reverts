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
    /// counts as a match. Default 70 — the Jaccard knee point for clean
    /// matches on CC 2.1.89 src-ref vs bundle.
    #[arg(long, default_value_t = 70)]
    pub threshold_percent: u32,
    /// Similarity metric used to combine per-axis fingerprint overlap. Jaccard
    /// is the principled default; overlap is more forgiving but easily fires
    /// on subset coincidences and should be reserved for diagnostic sweeps.
    #[arg(long, value_enum, default_value_t = SimilarityMetric::Jaccard)]
    pub metric: SimilarityMetric,
    /// Restrict to modules of this category (e.g. "application", "package").
    /// Repeatable. Empty = all categories.
    #[arg(long = "category")]
    pub categories: Vec<String>,
    /// Optional cap on modules per project (for fast iteration).
    #[arg(long)]
    pub limit: Option<usize>,
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
    let best_per_ref = score_best_subjects(&ref_bags, &sub_bags, args.metric);
    let recall = summarise_recall(&best_per_ref, threshold);
    println!(
        "[multi_axis_{} >= {:.2}]: {} / {} ref modules matched ({:.2}%) — scoring in {:.2}s",
        metric_label(args.metric),
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

    Ok(())
}

const fn metric_label(metric: SimilarityMetric) -> &'static str {
    match metric {
        SimilarityMetric::Jaccard => "jaccard",
        SimilarityMetric::Overlap => "overlap",
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

/// Returns the best (subject_idx, score, axis) for each ref module, indexed
/// alongside `ref_bags`. Uses an (axis, hash) inverted index to skip
/// candidates that share no fingerprint hash, so we do not pay O(R · S).
fn score_best_subjects(
    ref_bags: &[ModuleBag],
    sub_bags: &[ModuleBag],
    metric: SimilarityMetric,
) -> Vec<BestMatch> {
    // Inverted index: (axis, hash) -> Vec<subject_index>.
    let mut by_axis_hash: BTreeMap<(AxisKind, u64), Vec<usize>> = BTreeMap::new();
    for (sub_idx, bag) in sub_bags.iter().enumerate() {
        for (axis, hashes) in &bag.by_axis {
            for &hash in hashes {
                by_axis_hash.entry((*axis, hash)).or_default().push(sub_idx);
            }
        }
    }

    let mut out = vec![BestMatch::default(); ref_bags.len()];

    for (ref_idx, ref_bag) in ref_bags.iter().enumerate() {
        if ref_bag.function_count == 0 {
            continue;
        }
        // Per-axis: (subject_index -> shared hash count). We tally on the
        // axis where we observed the hash, so each pair's per-axis intersect
        // count is correct without re-walking subject bags.
        let mut axis_overlap: BTreeMap<AxisKind, HashMap<usize, usize>> = BTreeMap::new();
        for &axis in SCORING_AXES {
            axis_overlap.insert(axis, HashMap::new());
        }
        for (axis, hashes) in &ref_bag.by_axis {
            let axis_table = axis_overlap.get_mut(axis).expect("axis pre-inserted above");
            for &hash in hashes {
                if let Some(subject_indexes) = by_axis_hash.get(&(*axis, hash)) {
                    for &subject_idx in subject_indexes {
                        *axis_table.entry(subject_idx).or_default() += 1;
                    }
                }
            }
        }

        let mut best_score: f64 = 0.0;
        let mut best_axis: Option<AxisKind> = None;
        let mut best_subject: Option<usize> = None;
        // Union of all subject indexes with any axis-level overlap, dedup'd.
        let mut candidate_indexes: BTreeSet<usize> = BTreeSet::new();
        for (_, table) in &axis_overlap {
            candidate_indexes.extend(table.keys().copied());
        }
        for sub_idx in candidate_indexes {
            for &axis in SCORING_AXES {
                let ref_size = ref_bag.by_axis.get(&axis).map_or(0, BTreeSet::len);
                let sub_size = sub_bags[sub_idx]
                    .by_axis
                    .get(&axis)
                    .map_or(0, BTreeSet::len);
                if ref_size == 0 || sub_size == 0 {
                    continue;
                }
                let intersect = axis_overlap
                    .get(&axis)
                    .and_then(|table| table.get(&sub_idx).copied())
                    .unwrap_or(0);
                if intersect == 0 {
                    continue;
                }
                let score = match metric {
                    SimilarityMetric::Jaccard => {
                        let union = ref_size + sub_size - intersect;
                        intersect as f64 / union as f64
                    }
                    SimilarityMetric::Overlap => intersect as f64 / ref_size.min(sub_size) as f64,
                };
                if score > best_score {
                    best_score = score;
                    best_axis = Some(axis);
                    best_subject = Some(sub_idx);
                }
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
