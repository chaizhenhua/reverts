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

use clap::Args;
use reverts_graph::FunctionExtractor;
use reverts_ir::{AxisKind, ModuleId, NormalizationPassId};
use rusqlite::{Connection, OpenFlags, params};

use crate::args::{parse_args_with_name, parse_project_id};
use crate::errors::{CliError, CliRunError};
use crate::help;

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
    /// Jaccard threshold to count a (ref, subject) pair as a match, expressed
    /// as a percent (0-100). Default 40 = Jaccard >= 0.40.
    #[arg(long, default_value_t = 40)]
    pub threshold_percent: u32,
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

#[derive(Debug, Default, Clone)]
struct FingerprintBag {
    /// Primary AST hashes from every function in the module.
    ast: BTreeSet<u64>,
    /// Number of functions extracted (zero = unfingerprinted).
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

    // Strategy: baseline semantic_name overlap.
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

    // Strategy: function-fingerprint Jaccard.
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
    let recall = ast_jaccard_recall(&ref_bags, &sub_bags, threshold);
    println!(
        "[ast_jaccard >= {:.2}]: {} / {} ref modules matched ({:.2}%) — scoring in {:.2}s",
        threshold,
        recall.matched,
        ref_modules.len(),
        pct(recall.matched, ref_modules.len()),
        scoring_started.elapsed().as_secs_f64(),
    );
    print_histogram(&recall.score_histogram, ref_modules.len());

    Ok(())
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

fn fingerprint_modules(modules: &[ModuleRecord]) -> Vec<FingerprintBag> {
    let mut source_cache: HashMap<String, Option<String>> = HashMap::new();
    let mut out = Vec::with_capacity(modules.len());
    for module in modules {
        let source = source_cache
            .entry(module.file_path.clone())
            .or_insert_with(|| fs::read_to_string(&module.file_path).ok());
        let Some(source_text) = source.as_deref() else {
            out.push(FingerprintBag::default());
            continue;
        };
        let start = module.byte_start as usize;
        let end = module.byte_end as usize;
        let slice = source_text
            .get(start..end.min(source_text.len()))
            .filter(|slice| !slice.is_empty());
        let Some(slice) = slice else {
            out.push(FingerprintBag::default());
            continue;
        };
        let module_id = ModuleId(u32::try_from(module.id).unwrap_or(u32::MAX));
        let fingerprints = FunctionExtractor::fingerprint(module_id, slice);
        let mut bag = FingerprintBag {
            function_count: fingerprints.len(),
            ast: BTreeSet::new(),
        };
        for fingerprint in &fingerprints {
            // Use the post-normalization AST hash where available (it
            // strips bundler artifacts like TS-runtime helpers and JSX
            // factory rewrites). Otherwise fall back to primary.
            let alt = fingerprint
                .alternates
                .iter()
                .find(|alt| alt.pass == NormalizationPassId::TsRuntimeErased)
                .or_else(|| fingerprint.alternates.first());
            let hash = alt
                .and_then(|alt| alt.axes.get(AxisKind::Ast))
                .unwrap_or(fingerprint.primary.ast);
            bag.ast.insert(hash);
        }
        out.push(bag);
    }
    out
}

#[derive(Debug, Default)]
struct RecallReport {
    matched: usize,
    /// Bucket counts at score thresholds {0.1, 0.3, 0.5, 0.7, 0.9}.
    score_histogram: BTreeMap<&'static str, usize>,
}

fn ast_jaccard_recall(
    ref_bags: &[FingerprintBag],
    sub_bags: &[FingerprintBag],
    threshold: f64,
) -> RecallReport {
    // Inverted index: AST hash → list of subject bag indexes that contain it.
    let mut by_hash: HashMap<u64, Vec<usize>> = HashMap::new();
    for (idx, bag) in sub_bags.iter().enumerate() {
        for &hash in &bag.ast {
            by_hash.entry(hash).or_default().push(idx);
        }
    }

    let mut report = RecallReport::default();
    for label in ["≥0.1", "≥0.3", "≥0.5", "≥0.7", "≥0.9"] {
        report.score_histogram.insert(label, 0);
    }

    for ref_bag in ref_bags {
        if ref_bag.ast.is_empty() {
            continue;
        }
        let mut candidates: HashMap<usize, usize> = HashMap::new();
        for &hash in &ref_bag.ast {
            if let Some(subject_indexes) = by_hash.get(&hash) {
                for &subject_idx in subject_indexes {
                    *candidates.entry(subject_idx).or_default() += 1;
                }
            }
        }
        let mut best: f64 = 0.0;
        for (&subject_idx, &intersect) in &candidates {
            let union = ref_bag.ast.len() + sub_bags[subject_idx].ast.len() - intersect;
            if union == 0 {
                continue;
            }
            let score = intersect as f64 / union as f64;
            if score > best {
                best = score;
            }
        }
        for (label, count) in report.score_histogram.iter_mut() {
            let bound = match *label {
                "≥0.1" => 0.1,
                "≥0.3" => 0.3,
                "≥0.5" => 0.5,
                "≥0.7" => 0.7,
                "≥0.9" => 0.9,
                _ => continue,
            };
            if best >= bound {
                *count += 1;
            }
        }
        if best >= threshold {
            report.matched += 1;
        }
    }
    report
}

fn print_histogram(histogram: &BTreeMap<&'static str, usize>, total: usize) {
    let mut entries: Vec<_> = histogram.iter().collect();
    entries.sort_by_key(|(label, _)| *label);
    let line = entries
        .iter()
        .map(|(label, count)| format!("{label}: {} ({:.1}%)", count, pct(**count, total)))
        .collect::<Vec<_>>()
        .join("  ");
    println!("  jaccard histogram: {line}");
}
